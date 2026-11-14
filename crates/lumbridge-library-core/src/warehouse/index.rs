//! Sync + incremental indexing. Files are streamed one at a time through `git cat-file --batch`,
//! parsed, and written in small batches; nothing holds a whole repo in memory (the shipped unit
//! sets MemoryHigh=512M, MemoryMax=1G).

use super::card::{self, CardSummarizer, Manifest};
use super::git::{self, Blob, CatFile};
use super::lang::{self, Lang};
use super::license::{self, LicenseInfo};
use super::parse::{Parsed, Parsers};
use super::terms;
use super::{RepoRow, require_repo};
use crate::models::{Embedder, Kind};
use crate::rank::approx_tokens;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub const MAX_FILE_BYTES: u64 = 512 * 1024;
const EMBED_TEXT_CHARS: usize = 6000;
/// Bump when the card format changes: sync_repo rebuilds cards (and package names) whose
/// `health.card_version` differs, even if HEAD did not move.
const CARD_VERSION: i64 = 2;
/// Manifests read per repo (Cargo.toml / package.json / pyproject.toml / go.mod).
const MAX_MANIFESTS: usize = 300;
/// Chunks per wh.chunk_terms backfill batch.
const TERMS_BATCH: i64 = 500;

#[derive(Clone)]
pub struct IndexOpts {
    /// Re-index even if HEAD did not move (files with unchanged blobs are still skipped).
    pub force: bool,
    /// Skip a full re-parse of unchanged blobs. With `reparse_all`, every file is re-parsed
    /// (use after changing the parser); embeddings are still reused by content hash.
    pub reparse_all: bool,
    /// `git fetch` before indexing (ignored for components).
    pub fetch: bool,
    pub max_file_bytes: u64,
    /// Files per write transaction.
    pub batch_files: usize,
    /// Optional LLM summary for the repo card. Not called when None.
    pub summarizer: Option<Arc<dyn CardSummarizer>>,
}

impl Default for IndexOpts {
    fn default() -> Self {
        Self { force: false, reparse_all: false, fetch: true, max_file_bytes: MAX_FILE_BYTES, batch_files: 64, summarizer: None }
    }
}

impl std::fmt::Debug for IndexOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexOpts")
            .field("force", &self.force)
            .field("reparse_all", &self.reparse_all)
            .field("fetch", &self.fetch)
            .field("max_file_bytes", &self.max_file_bytes)
            .field("batch_files", &self.batch_files)
            .field("summarizer", &self.summarizer.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SyncReport {
    pub slug: String,
    pub previous_commit: Option<String>,
    pub head_commit: Option<String>,
    /// False when HEAD had not moved and nothing was re-indexed.
    pub reindexed: bool,
    pub fetch_error: Option<String>,
    pub files_added: usize,
    pub files_changed: usize,
    pub files_removed: usize,
    pub files_unchanged: usize,
    pub files_skipped: usize,
    /// Skip counts by reason (too_large, lockfile, vendored_or_generated_dir, binary, ...).
    pub skipped_by_reason: HashMap<String, usize>,
    pub symbols: usize,
    pub refs: usize,
    pub chunks_new: usize,
    /// New chunk rows whose embedding was copied from an identical chunk (same sha256).
    pub chunks_reused: usize,
    pub license_spdx: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SyncOutcome {
    pub slug: String,
    pub report: Option<SyncReport>,
    pub error: Option<String>,
}

/// What gets embedded for a chunk (and hashed for embedding reuse).
pub(crate) fn embed_text(path: &str, signature: Option<&str>, content: &str) -> String {
    let mut s = String::with_capacity(content.len().min(EMBED_TEXT_CHARS) + 200);
    s.push_str(path);
    s.push('\n');
    if let Some(sig) = signature {
        s.push_str(sig);
        s.push('\n');
    }
    for (n, c) in content.chars().enumerate() {
        if n >= EMBED_TEXT_CHARS {
            break;
        }
        s.push(c);
    }
    s
}

fn sha256_hex(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(64);
    for b in d {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// License from root license files + manifests at `commit`.
pub(crate) async fn detect_license(dir: &Path, commit: &str) -> Result<LicenseInfo> {
    let out = git::run(Some(dir), &["ls-tree", "--name-only", commit], git::SHORT).await?;
    let mut files = Vec::new();
    let mut manifest: Option<(String, String)> = None;
    let mut cat = CatFile::spawn(dir)?;
    for name in out.lines() {
        let read = license::is_license_file(name) || matches!(name, "Cargo.toml" | "package.json" | "pyproject.toml");
        if !read {
            continue;
        }
        let Blob::Data(bytes) = cat.blob(&format!("{commit}:{name}"), git::META_MAX_BYTES).await? else { continue };
        let text = String::from_utf8_lossy(&bytes);
        if license::is_license_file(name) {
            if let Some((id, conf)) = license::identify_text(&text) {
                files.push((name.to_string(), id, conf));
            }
        } else if manifest.is_none() {
            let m = match name {
                "Cargo.toml" => license::from_cargo_toml(&text),
                "package.json" => license::from_package_json(&text),
                _ => license::from_pyproject(&text),
            };
            manifest = m.map(|e| (e, name.to_string()));
        }
    }
    cat.close().await;
    Ok(license::combine(manifest, files))
}

struct FileRec {
    path: String,
    sha: String,
    lang: Lang,
    bytes: i32,
    parsed: Parsed,
}

#[derive(Default)]
struct Counts {
    added: usize,
    changed: usize,
    removed: usize,
    unchanged: usize,
    skipped: usize,
    skipped_by: HashMap<String, usize>,
    symbols: usize,
    refs: usize,
    chunks: usize,
    reused: usize,
    manifests: Vec<Manifest>,
}

impl Counts {
    fn skip(&mut self, why: &str) {
        self.skipped += 1;
        *self.skipped_by.entry(why.to_string()).or_default() += 1;
    }
}

/// Fetch (unless a component), re-index if HEAD moved or never indexed, rebuild the card, update
/// health. Safe to re-run after a failure: unchanged blobs are skipped.
pub async fn sync_repo(pool: &PgPool, library_root: &Path, slug: &str, opts: &IndexOpts) -> Result<SyncReport> {
    let _ = library_root; // paths come from wh.repos.local_path
    let repo = require_repo(pool, slug).await?;
    let t0 = Instant::now();
    let res = sync_inner(pool, &repo, opts, t0).await;
    if let Err(e) = &res {
        {
            let msg = format!("{e:#}");
            let _ = sqlx::query(
                "update wh.repos set health = health || jsonb_build_object('ok', false, 'error', $2::text, 'at', now()) where id = $1::uuid",
            )
            .bind(&repo.id)
            .bind(&msg)
            .execute(pool)
            .await;
        }
    }
    res
}

async fn sync_inner(pool: &PgPool, repo: &RepoRow, opts: &IndexOpts, t0: Instant) -> Result<SyncReport> {
    let dir = PathBuf::from(&repo.local_path);
    let mut report = SyncReport { slug: repo.slug.clone(), previous_commit: repo.last_indexed_commit.clone(), ..Default::default() };
    if opts.fetch && repo.tier != "component" {
        // Only the owner's own repos may have a local-path origin.
        let t = if repo.tier == "own" { git::Transport::AllowLocal } else { git::Transport::Remote };
        match git::fetch(&dir, t).await {
            Ok(()) => {
                let _ = git::refresh_remote_head(&dir, t).await;
            }
            Err(e) => report.fetch_error = Some(format!("{e:#}")),
        }
    }
    let head = git::head(&dir).await?;
    report.head_commit = head.clone();
    let Some(head) = head else {
        sqlx::query("update wh.repos set last_synced_at = now(), health = jsonb_build_object('ok', true, 'empty', true) where id = $1::uuid")
            .bind(&repo.id)
            .execute(pool)
            .await?;
        report.duration_ms = t0.elapsed().as_millis() as u64;
        return Ok(report);
    };
    let card_missing: bool = sqlx::query_scalar(
        "select card_md is null or (health->>'card_version') is distinct from $2::text from wh.repos where id = $1::uuid",
    )
    .bind(&repo.id)
    .bind(CARD_VERSION.to_string())
    .fetch_one(pool)
    .await?;
    // Chunks indexed before wh.chunk_terms existed (or by an older indexer) get their terms here.
    fill_terms(pool, Some(&repo.id), true).await?;
    let moved = repo.last_indexed_commit.as_deref() != Some(head.as_str());
    if !(moved || opts.force || card_missing) {
        sqlx::query(
            "update wh.repos set last_synced_at = now(),
               health = health || jsonb_build_object('ok', true, 'fetch_error', $2::text, 'checked_at', now())
             where id = $1::uuid",
        )
        .bind(&repo.id)
        .bind(&report.fetch_error)
        .execute(pool)
        .await?;
        report.license_spdx = repo.license_spdx.clone();
        report.duration_ms = t0.elapsed().as_millis() as u64;
        return Ok(report);
    }

    let mut counts = index_commit(pool, &repo.id, &dir, &head, opts).await?;
    let crates = card::crate_names(&counts.manifests, repo.slug.rsplit('/').next().unwrap_or(&repo.slug));
    let skipped_by_reason = std::mem::take(&mut counts.skipped_by);
    report.reindexed = true;
    report.files_added = counts.added;
    report.files_changed = counts.changed;
    report.files_removed = counts.removed;
    report.files_unchanged = counts.unchanged;
    report.files_skipped = counts.skipped;
    report.skipped_by_reason = skipped_by_reason;
    report.symbols = counts.symbols;
    report.refs = counts.refs;
    report.chunks_new = counts.chunks;
    report.chunks_reused = counts.reused;

    // Components carry the license their creator declared (checked against the source).
    let lic = if repo.tier == "component" {
        LicenseInfo { spdx: repo.license_spdx.clone(), confidence: repo.license_confidence, copyleft: repo.copyleft, sources: vec![] }
    } else {
        detect_license(&dir, &head).await?
    };
    report.license_spdx = lic.spdx.clone();
    let committed_at = git::commit_date(&dir, &head).await.ok().flatten();
    let branch = git::default_branch(&dir).await.unwrap_or_else(|_| repo.default_branch.clone());

    let langs: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
        "select language, count(*), coalesce(sum(bytes), 0)::bigint from wh.files where repo_id = $1::uuid
         group by language order by 3 desc",
    )
    .bind(&repo.id)
    .fetch_all(pool)
    .await?;
    let languages: Vec<String> = langs
        .iter()
        .filter_map(|(l, _, _)| l.clone())
        .filter(|l| !matches!(l.as_str(), "markdown" | "text"))
        .collect();
    let file_count: i64 = langs.iter().map(|x| x.1).sum();

    let meta = card::RepoMeta {
        slug: repo.slug.clone(),
        tier: repo.tier.clone(),
        origin_url: repo.origin_url.clone(),
        default_branch: branch.clone(),
        head: head.clone(),
        committed_at,
        license: lic.clone(),
        manifests: counts.manifests.clone(),
    };
    let built = card::build(pool, &repo.id, &dir, &meta, opts.summarizer.as_deref()).await?;
    let description = repo.description.clone().filter(|_| repo.tier == "component").or(built.description.clone());

    let health = serde_json::json!({
        "ok": true,
        "fetch_error": report.fetch_error,
        "indexed_at": chrono::Utc::now(),
        "files": file_count,
        "symbols_added": report.symbols,
        "chunks_added": report.chunks_new,
        "skipped": report.files_skipped,
        "skipped_by_reason": report.skipped_by_reason,
        "card_version": CARD_VERSION.to_string(),
        "duration_ms": t0.elapsed().as_millis() as u64,
    });
    sqlx::query(
        "update wh.repos set head_commit = $2, last_indexed_commit = $2, head_committed_at = $3, default_branch = $4,
            license_spdx = $5, license_confidence = $6, copyleft = $7, description = $8, languages = $9,
            file_count = $10, card_md = $11, card_commit = $2, last_synced_at = now(), health = $12, crates = $13
         where id = $1::uuid",
    )
    .bind(&repo.id)
    .bind(&head)
    .bind(committed_at)
    .bind(&branch)
    .bind(&lic.spdx)
    .bind(lic.confidence)
    .bind(lic.copyleft)
    .bind(&description)
    .bind(&languages)
    .bind(file_count as i32)
    .bind(&built.card_md)
    .bind(&health)
    .bind(&crates)
    .execute(pool)
    .await?;
    report.duration_ms = t0.elapsed().as_millis() as u64;
    Ok(report)
}

/// Sync every repo, one after another. Errors are reported per repo, never abort the loop.
pub async fn sync_all(pool: &PgPool, library_root: &Path, opts: &IndexOpts) -> Result<Vec<SyncOutcome>> {
    let slugs: Vec<String> = sqlx::query_scalar("select slug from wh.repos order by tier, slug").fetch_all(pool).await?;
    let mut out = Vec::with_capacity(slugs.len());
    for slug in slugs {
        match sync_repo(pool, library_root, &slug, opts).await {
            Ok(r) => out.push(SyncOutcome { slug, report: Some(r), error: None }),
            Err(e) => out.push(SyncOutcome { slug, report: None, error: Some(format!("{e:#}")) }),
        }
    }
    Ok(out)
}

async fn index_commit(pool: &PgPool, repo_id: &str, dir: &Path, commit: &str, opts: &IndexOpts) -> Result<Counts> {
    let git::LsTree { entries: tree, non_utf8_paths } = git::ls_tree(dir, commit).await?;
    let existing: HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>("select path, blob_sha from wh.files where repo_id = $1::uuid")
            .bind(repo_id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
    let mut counts = Counts::default();
    for _ in 0..non_utf8_paths {
        counts.skip("non_utf8_path");
    }
    // Paths already taken in this run: a duplicate would make one upsert batch touch a row twice.
    let mut seen: HashSet<String> = HashSet::new();
    let mut keep: HashSet<String> = HashSet::with_capacity(existing.len());
    let mut cat = CatFile::spawn(dir)?;
    let mut parsers = Parsers::default();
    let mut batch: Vec<FileRec> = Vec::new();
    let mut batch_bytes = 0usize;
    // (path, blob) of package manifests, read after the files (whether or not they changed).
    let mut manifests: Vec<(String, String)> = Vec::new();

    for e in tree {
        if !seen.insert(e.path.clone()) {
            counts.skip("duplicate_path");
            continue;
        }
        if manifests.len() < MAX_MANIFESTS
            && card::is_manifest(e.path.rsplit('/').next().unwrap_or(&e.path))
            && e.path.matches('/').count() <= 4
            && e.size <= git::META_MAX_BYTES
            && !matches!(lang::skip_reason(&e.path, e.size, git::META_MAX_BYTES), Some(r) if r != "unsupported_type")
        {
            manifests.push((e.path.clone(), e.sha.clone()));
        }
        if let Some(why) = lang::skip_reason(&e.path, e.size, opts.max_file_bytes) {
            counts.skip(why);
            continue;
        }
        let prev = existing.get(&e.path);
        if !opts.reparse_all && prev == Some(&e.sha) {
            keep.insert(e.path);
            counts.unchanged += 1;
            continue;
        }
        let bytes = match cat.blob(&e.sha, opts.max_file_bytes).await? {
            Blob::Data(b) => b,
            Blob::TooLarge(_) => {
                counts.skip("too_large");
                continue;
            }
            Blob::Missing => {
                counts.skip("missing_blob");
                continue;
            }
        };
        if let Some(why) = lang::content_skip_reason(&e.path, &bytes) {
            counts.skip(why);
            continue;
        }
        let text = String::from_utf8(bytes).context("utf8 checked above")?;
        let lang = lang::detect(&e.path).context("detect checked by skip_reason")?;
        let parsed = parsers.parse_file(lang, &text);
        match prev {
            Some(_) => counts.changed += 1,
            None => counts.added += 1,
        }
        batch_bytes += text.len();
        drop(text);
        keep.insert(e.path.clone());
        batch.push(FileRec { path: e.path, sha: e.sha, lang, bytes: e.size as i32, parsed });
        if batch.len() >= opts.batch_files.max(1) || batch_bytes > 4 << 20 {
            flush(pool, repo_id, commit, &mut batch, &mut counts).await?;
            batch_bytes = 0;
        }
    }
    flush(pool, repo_id, commit, &mut batch, &mut counts).await?;
    for (path, sha) in manifests {
        if let Blob::Data(b) = cat.blob(&sha, git::META_MAX_BYTES).await?
            && let Some(m) = card::parse_manifest(&path, &String::from_utf8_lossy(&b))
        {
            counts.manifests.push(m);
        }
    }
    for m in counts.manifests.iter_mut() {
        let dir = format!("{}/", m.dir);
        m.files = if m.dir.is_empty() { seen.len() } else { seen.iter().filter(|p| p.starts_with(&dir)).count() };
    }
    cat.close().await;

    // Removed (or now skipped) files go last, so a renamed file's chunks could reuse embeddings.
    let removed: Vec<String> = existing.into_keys().filter(|p| !keep.contains(p)).collect();
    counts.removed = removed.len();
    for part in removed.chunks(1000) {
        sqlx::query("delete from wh.files where repo_id = $1::uuid and path = any($2)")
            .bind(repo_id)
            .bind(part)
            .execute(pool)
            .await?;
    }
    Ok(counts)
}

async fn next_ids(tx: &mut sqlx::PgConnection, table: &str, n: usize) -> Result<Vec<i64>> {
    if n == 0 {
        return Ok(vec![]);
    }
    let sql = format!("select nextval(pg_get_serial_sequence('{table}', 'id')) from generate_series(1, $1)");
    Ok(sqlx::query_scalar(&sql).bind(n as i64).fetch_all(&mut *tx).await?)
}

async fn flush(pool: &PgPool, repo_id: &str, commit: &str, batch: &mut Vec<FileRec>, counts: &mut Counts) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let files = std::mem::take(batch);
    let mut tx = pool.begin().await?;

    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let shas: Vec<&str> = files.iter().map(|f| f.sha.as_str()).collect();
    let langs: Vec<&str> = files.iter().map(|f| f.lang.name()).collect();
    let sizes: Vec<i32> = files.iter().map(|f| f.bytes).collect();
    let nlines: Vec<i32> = files.iter().map(|f| f.parsed.lines as i32).collect();
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "insert into wh.files (repo_id, path, blob_sha, language, bytes, lines, indexed_commit)
         select $1::uuid, p, s, l, b, n, $7 from unnest($2::text[], $3::text[], $4::text[], $5::int[], $6::int[]) as u(p, s, l, b, n)
         on conflict (repo_id, path) do update set blob_sha = excluded.blob_sha, language = excluded.language,
           bytes = excluded.bytes, lines = excluded.lines, indexed_commit = excluded.indexed_commit
         returning id, path",
    )
    .bind(repo_id)
    .bind(&paths)
    .bind(&shas)
    .bind(&langs)
    .bind(&sizes)
    .bind(&nlines)
    .bind(commit)
    .fetch_all(&mut *tx)
    .await?;
    let file_id: HashMap<String, i64> = rows.into_iter().map(|(id, p)| (p, id)).collect();
    let fids: Vec<i64> = file_id.values().copied().collect();

    let old_chunks: Vec<i64> = sqlx::query_scalar("select id from wh.chunks where file_id = any($1)")
        .bind(&fids)
        .fetch_all(&mut *tx)
        .await?;
    sqlx::query("delete from wh.symbols where file_id = any($1)").bind(&fids).execute(&mut *tx).await?;
    sqlx::query("delete from wh.refs where file_id = any($1)").bind(&fids).execute(&mut *tx).await?;

    // Symbols, with ids allocated up front so chunks can point at them.
    let nsym: usize = files.iter().map(|f| f.parsed.symbols.len()).sum();
    let sym_ids = next_ids(&mut tx, "wh.symbols", nsym).await?;
    let mut s_id = Vec::with_capacity(nsym);
    let mut s_file = Vec::with_capacity(nsym);
    let mut s_name = Vec::with_capacity(nsym);
    let mut s_kind = Vec::with_capacity(nsym);
    let mut s_parent: Vec<Option<&str>> = Vec::with_capacity(nsym);
    let mut s_sig: Vec<Option<&str>> = Vec::with_capacity(nsym);
    let mut s_doc: Vec<Option<&str>> = Vec::with_capacity(nsym);
    let mut s_ls = Vec::with_capacity(nsym);
    let mut s_le = Vec::with_capacity(nsym);
    // Per file: offset into sym_ids.
    let mut sym_base = Vec::with_capacity(files.len());
    let mut k = 0;
    for f in &files {
        sym_base.push(k);
        let fid = file_id[&f.path];
        for s in &f.parsed.symbols {
            s_id.push(sym_ids[k]);
            s_file.push(fid);
            s_name.push(s.name.as_str());
            s_kind.push(s.kind);
            s_parent.push(s.parent.as_deref());
            s_sig.push(s.signature.as_deref());
            s_doc.push(s.doc.as_deref());
            s_ls.push(s.line_start as i32);
            s_le.push(s.line_end as i32);
            k += 1;
        }
    }
    if nsym > 0 {
        sqlx::query(
            "insert into wh.symbols (id, repo_id, file_id, name, kind, parent, signature, doc, line_start, line_end)
             select i, $1::uuid, f, n, k, p, s, d, a, b
             from unnest($2::bigint[], $3::bigint[], $4::text[], $5::text[], $6::text[], $7::text[], $8::text[], $9::int[], $10::int[])
               as u(i, f, n, k, p, s, d, a, b)",
        )
        .bind(repo_id)
        .bind(&s_id)
        .bind(&s_file)
        .bind(&s_name)
        .bind(&s_kind)
        .bind(&s_parent)
        .bind(&s_sig)
        .bind(&s_doc)
        .bind(&s_ls)
        .bind(&s_le)
        .execute(&mut *tx)
        .await?;
    }
    counts.symbols += nsym;

    let mut r_file = Vec::new();
    let mut r_name = Vec::new();
    let mut r_line = Vec::new();
    for f in &files {
        let fid = file_id[&f.path];
        for r in &f.parsed.refs {
            r_file.push(fid);
            r_name.push(r.name.as_str());
            r_line.push(r.line as i32);
        }
    }
    if !r_file.is_empty() {
        sqlx::query(
            "insert into wh.refs (repo_id, file_id, name, line)
             select $1::uuid, f, n, l from unnest($2::bigint[], $3::text[], $4::int[]) as u(f, n, l)",
        )
        .bind(repo_id)
        .bind(&r_file)
        .bind(&r_name)
        .bind(&r_line)
        .execute(&mut *tx)
        .await?;
    }
    counts.refs += r_file.len();

    let nch: usize = files.iter().map(|f| f.parsed.chunks.len()).sum();
    let ch_ids = next_ids(&mut tx, "wh.chunks", nch).await?;
    let mut c_file = Vec::with_capacity(nch);
    let mut c_sym: Vec<Option<i64>> = Vec::with_capacity(nch);
    let mut c_path = Vec::with_capacity(nch);
    let mut c_ls = Vec::with_capacity(nch);
    let mut c_le = Vec::with_capacity(nch);
    let mut c_lang = Vec::with_capacity(nch);
    let mut c_content = Vec::with_capacity(nch);
    let mut c_tokens = Vec::with_capacity(nch);
    let mut c_sha = Vec::with_capacity(nch);
    let mut c_search = Vec::with_capacity(nch);
    let mut c_pathw = Vec::with_capacity(nch);
    let mut c_ident = Vec::with_capacity(nch);
    for (fi, f) in files.iter().enumerate() {
        let fid = file_id[&f.path];
        for c in &f.parsed.chunks {
            let sym = c.symbol_idx.map(|i| &f.parsed.symbols[i]);
            c_file.push(fid);
            c_sym.push(c.symbol_idx.map(|i| sym_ids[sym_base[fi] + i]));
            c_path.push(f.path.as_str());
            c_ls.push(c.line_start as i32);
            c_le.push(c.line_end as i32);
            c_lang.push(f.lang.name());
            c_content.push(c.content.as_str());
            c_tokens.push(approx_tokens(&c.content) as i32);
            c_sha.push(sha256_hex(&embed_text(&f.path, sym.and_then(|s| s.signature.as_deref()), &c.content)));
            c_search.push(terms::search_text(sym.map(|s| s.name.as_str()), sym.and_then(|s| s.parent.as_deref())));
            c_pathw.push(terms::path_text(&f.path, sym.and_then(|s| s.signature.as_deref())));
            c_ident.push(terms::ident_text(&c.content));
        }
    }
    if nch > 0 {
        sqlx::query(
            "insert into wh.chunks (id, repo_id, file_id, symbol_id, path, line_start, line_end, language, content, tokens, sha256)
             select i, $1::uuid, f, s, p, a, b, l, c, t, h
             from unnest($2::bigint[], $3::bigint[], $4::bigint[], $5::text[], $6::int[], $7::int[], $8::text[], $9::text[], $10::int[], $11::text[])
               as u(i, f, s, p, a, b, l, c, t, h)",
        )
        .bind(repo_id)
        .bind(&ch_ids)
        .bind(&c_file)
        .bind(&c_sym)
        .bind(&c_path)
        .bind(&c_ls)
        .bind(&c_le)
        .bind(&c_lang)
        .bind(&c_content)
        .bind(&c_tokens)
        .bind(&c_sha)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "insert into wh.chunk_terms (chunk_id, search_text, path_text, ident_text)
             select * from unnest($1::bigint[], $2::text[], $3::text[], $4::text[])",
        )
        .bind(&ch_ids)
        .bind(&c_search)
        .bind(&c_pathw)
        .bind(&c_ident)
        .execute(&mut *tx)
        .await?;
        // Reuse embeddings of identical chunks (old rows of these files, or anywhere else).
        let reused = sqlx::query(
            "update wh.chunks n set embedding = o.embedding, embed_model_id = o.embed_model_id
             from (select distinct on (sha256) sha256, embedding, embed_model_id from wh.chunks
                    where sha256 = any($2) and embedding is not null and not (id = any($1))
                    order by sha256, embed_model_id desc nulls last) o
             where n.id = any($1) and n.sha256 = o.sha256",
        )
        .bind(&ch_ids)
        .bind(&c_sha)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        counts.reused += reused as usize;
    }
    counts.chunks += nch;
    if !old_chunks.is_empty() {
        sqlx::query("delete from wh.chunks where id = any($1)").bind(&old_chunks).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TermsReport {
    /// Chunks whose `wh.chunk_terms` row was written.
    pub chunks: usize,
    pub batches: usize,
    pub duration_ms: u64,
}

/// Recompute `wh.chunk_terms` (the code-aware search text) for every chunk, in batches of 500
/// by chunk id; never holds more than one batch in memory. Run once after migration 0007 (or
/// after changing `terms::search_text`). `sync_repo` also fills chunks that have no row yet.
pub async fn reindex_search_text(pool: &PgPool) -> Result<TermsReport> {
    fill_terms(pool, None, false).await
}

/// One-shot backfill after migration 0007: [`reindex_search_text`], then a no-fetch
/// [`sync_all`] that re-reads manifests (package names → `wh.repos.crates`) and rebuilds v2 cards
/// (unchanged files are skipped, nothing is re-parsed or re-embedded).
pub async fn backfill_ranking(pool: &PgPool, library_root: &Path) -> Result<(TermsReport, Vec<SyncOutcome>)> {
    let terms = reindex_search_text(pool).await?;
    let synced = sync_all(pool, library_root, &IndexOpts { fetch: false, ..Default::default() }).await?;
    Ok((terms, synced))
}

/// Write `wh.chunk_terms` for the chunks of one repo (or all), optionally only those without a row.
pub(crate) async fn fill_terms(pool: &PgPool, repo_id: Option<&str>, only_missing: bool) -> Result<TermsReport> {
    let t0 = Instant::now();
    let mut rep = TermsReport::default();
    let mut after: i64 = 0;
    loop {
        type Row = (i64, String, String, Option<String>, Option<String>, Option<String>);
        let rows: Vec<Row> = sqlx::query_as(
            "select c.id, c.path, c.content, s.name, s.parent, s.signature
               from wh.chunks c left join wh.symbols s on s.id = c.symbol_id
              where c.id > $1 and ($2::uuid is null or c.repo_id = $2::uuid)
                and (not $3 or not exists (select 1 from wh.chunk_terms t where t.chunk_id = c.id))
              order by c.id limit $4",
        )
        .bind(after)
        .bind(repo_id)
        .bind(only_missing)
        .bind(TERMS_BATCH)
        .fetch_all(pool)
        .await?;
        let Some(last) = rows.last() else { break };
        after = last.0;
        let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let search: Vec<String> = rows.iter().map(|(_, _, _, n, par, _)| terms::search_text(n.as_deref(), par.as_deref())).collect();
        let pathw: Vec<String> = rows.iter().map(|(_, p, _, _, _, sig)| terms::path_text(p, sig.as_deref())).collect();
        let ident: Vec<String> = rows.iter().map(|r| terms::ident_text(&r.2)).collect();
        drop(rows);
        // The join drops chunks a concurrent sync deleted since the select.
        let n = sqlx::query(
            "insert into wh.chunk_terms (chunk_id, search_text, path_text, ident_text)
             select u.id, u.s, u.p, u.i from unnest($1::bigint[], $2::text[], $3::text[], $4::text[]) as u(id, s, p, i)
               join wh.chunks c on c.id = u.id
             on conflict (chunk_id) do update
               set search_text = excluded.search_text, path_text = excluded.path_text, ident_text = excluded.ident_text",
        )
        .bind(&ids)
        .bind(&search)
        .bind(&pathw)
        .bind(&ident)
        .execute(pool)
        .await?
        .rows_affected();
        rep.chunks += n as usize;
        rep.batches += 1;
    }
    rep.duration_ms = t0.elapsed().as_millis() as u64;
    if rep.chunks > 0 {
        tracing::info!(chunks = rep.chunks, ms = rep.duration_ms, repo = repo_id.unwrap_or("*"), "warehouse: chunk terms written");
    }
    Ok(rep)
}

/// Embed chunks with no vector or a vector from another model. Returns how many were embedded.
pub async fn embed_pending(pool: &PgPool, embedder: &Embedder, embed_model_id: i32, limit: usize) -> Result<usize> {
    const PAGE: i64 = 64;
    let mut done = 0usize;
    let mut after: i64 = 0;
    while done < limit {
        let take = PAGE.min((limit - done) as i64);
        let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
            "select c.id, c.path, c.content, s.signature from wh.chunks c
               left join wh.symbols s on s.id = c.symbol_id
              where (c.embedding is null or c.embed_model_id is distinct from $1) and c.id > $2
              order by c.id limit $3",
        )
        .bind(embed_model_id)
        .bind(after)
        .bind(take)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            break;
        }
        after = rows.last().map(|r| r.0).unwrap_or(after);
        let texts: Vec<String> = rows.iter().map(|(_, p, c, s)| embed_text(p, s.as_deref(), c)).collect();
        let vecs = embedder.embed(&texts, Kind::Passage).await?;
        let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let lits: Vec<String> = vecs.iter().map(|v| crate::db::halfvec_literal(v)).collect();
        sqlx::query(
            "update wh.chunks c set embedding = u.v::extensions.halfvec, embed_model_id = $3
               from unnest($1::bigint[], $2::text[]) as u(id, v) where c.id = u.id",
        )
        .bind(&ids)
        .bind(&lits)
        .bind(embed_model_id)
        .execute(pool)
        .await?;
        done += rows.len();
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    #[test]
    fn embed_text_truncates() {
        let t = super::embed_text("a.rs", Some("fn x()"), &"y".repeat(10_000));
        assert!(t.starts_with("a.rs\nfn x()\n"));
        assert_eq!(t.chars().count(), "a.rs\nfn x()\n".len() + 6000);
    }
}
