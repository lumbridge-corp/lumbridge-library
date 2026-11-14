//! Vault → Postgres. One sha256 per file decides whether anything else happens; changed files are
//! re-chunked and keep the embeddings of chunks whose embed text hash is unchanged.

use super::chunk::chunk_markdown;
use super::frontmatter::parse_document;
use super::links::parse_wikilinks;
use super::{MAX_FILE_BYTES, default_author, embed_text, layer_for, sha256_hex, slugify, stem};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::HashMap;
use std::path::{Component, Path};
use std::time::Instant;
use uuid::Uuid;

/// Serialises syncs within one process (a write's re-sync waits for a running full sync).
static SYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ParseError {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SyncReport {
    /// Markdown files looked at.
    pub scanned: usize,
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub deleted: usize,
    /// Files skipped (over 2 MB, unreadable or non-UTF-8 names).
    pub skipped: usize,
    /// Chunks written without an embedding (the embed backlog).
    pub chunks_new: usize,
    /// Chunks written with an embedding reused from an identical chunk.
    pub chunks_reused: usize,
    pub parse_errors: Vec<ParseError>,
    pub elapsed_ms: u64,
}

impl SyncReport {
    fn changed(&self) -> bool {
        self.added + self.updated + self.deleted > 0
    }
}

fn skip_name(name: &str) -> bool {
    name.starts_with('.') || name == "Attachments"
}

/// Vault-relative markdown paths (forward slashes), skipping dot-dirs and `Attachments/`.
pub(crate) fn list_markdown(root: &Path) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0;
    let walker = walkdir::WalkDir::new(root).follow_links(false).sort_by_file_name().into_iter().filter_entry(|e| {
        e.depth() == 0 || !e.file_name().to_str().is_some_and(skip_name)
    });
    for entry in walker {
        let Ok(entry) = entry else {
            skipped += 1;
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let Some(rel) = rel.to_str() else {
            skipped += 1;
            continue;
        };
        if !rel.ends_with(".md") {
            continue;
        }
        out.push(rel.replace('\\', "/"));
    }
    (out, skipped)
}

/// Re-index every changed markdown file under `vault_root`, drop rows whose files are gone, then
/// re-resolve wikilinks. Cheap when nothing changed: one `path → sha256` query plus one hash per file.
pub async fn sync_vault(pool: &PgPool, vault_root: &Path) -> Result<SyncReport> {
    let _guard = SYNC_LOCK.lock().await;
    let t0 = Instant::now();
    if !vault_root.is_dir() {
        bail!("vault root {} is not a directory", vault_root.display());
    }
    let mut known: HashMap<String, String> = sqlx::query_as::<_, (String, String)>("select path, sha256 from kb.documents")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();

    let root = vault_root.to_path_buf();
    let (paths, skipped) = tokio::task::spawn_blocking(move || list_markdown(&root)).await?;
    let mut rep = SyncReport { skipped, ..Default::default() };

    for rel in paths {
        rep.scanned += 1;
        let prev = known.remove(&rel);
        match index_path(pool, vault_root, &rel, prev.as_deref(), &mut rep).await {
            Ok(true) => {}
            Ok(false) => {
                // Skipped (too big / unreadable): an old row for it must go too.
                if prev.is_some() {
                    known.insert(rel, String::new());
                }
            }
            Err(e) => {
                tracing::warn!(path = %rel, "sync failed: {e:#}");
                rep.parse_errors.push(ParseError { path: rel, error: format!("{e:#}") });
            }
        }
    }

    if !known.is_empty() {
        let gone: Vec<String> = known.into_keys().collect();
        let n = sqlx::query("delete from kb.documents where path = any($1)").bind(&gone).execute(pool).await?;
        rep.deleted = n.rows_affected() as usize;
    }
    if rep.changed() {
        resolve_links(pool).await?;
    }
    rep.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(rep)
}

/// Re-index one vault-relative file (or delete its row if the file is gone) and re-resolve links.
pub async fn sync_file(pool: &PgPool, vault_root: &Path, rel_path: &str) -> Result<SyncReport> {
    let rel = clean_rel(rel_path)?;
    let _guard = SYNC_LOCK.lock().await;
    let t0 = Instant::now();
    let mut rep = SyncReport::default();
    let prev: Option<String> = sqlx::query_scalar("select sha256 from kb.documents where path = $1")
        .bind(&rel)
        .fetch_optional(pool)
        .await?;
    let full = vault_root.join(&rel);
    let present = rel.ends_with(".md") && tokio::fs::metadata(&full).await.map(|m| m.is_file()).unwrap_or(false);
    let indexed = if present {
        rep.scanned = 1;
        index_path(pool, vault_root, &rel, prev.as_deref(), &mut rep).await?
    } else {
        false
    };
    if !indexed && prev.is_some() {
        let n = sqlx::query("delete from kb.documents where path = $1").bind(&rel).execute(pool).await?;
        rep.deleted = n.rows_affected() as usize;
    }
    if rep.changed() {
        resolve_links(pool).await?;
    }
    rep.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(rep)
}

/// Normalise a vault-relative path; rejects absolute paths and `..`.
pub(crate) fn clean_rel(p: &str) -> Result<String> {
    let p = super::strip_docs_prefix(p).replace('\\', "/");
    let mut parts = Vec::new();
    for c in Path::new(&p).components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str().context("non-UTF-8 path")?.to_string()),
            Component::CurDir => {}
            _ => bail!("path {p:?} must be vault-relative without '..'"),
        }
    }
    if parts.is_empty() {
        bail!("empty path");
    }
    Ok(parts.join("/"))
}

/// Hash one file and index it if its sha differs from `prev`. Ok(false) = skipped.
async fn index_path(pool: &PgPool, root: &Path, rel: &str, prev: Option<&str>, rep: &mut SyncReport) -> Result<bool> {
    let full = root.join(rel);
    let meta = match tokio::fs::metadata(&full).await {
        Ok(m) => m,
        Err(_) => {
            rep.skipped += 1;
            return Ok(false);
        }
    };
    if meta.len() > MAX_FILE_BYTES {
        rep.skipped += 1;
        return Ok(false);
    }
    let bytes = tokio::fs::read(&full).await.with_context(|| format!("reading {rel}"))?;
    let sha = sha256_hex(&bytes);
    if prev == Some(sha.as_str()) {
        rep.unchanged += 1;
        return Ok(true);
    }
    let text = String::from_utf8_lossy(&bytes).replace('\0', "");
    drop(bytes);
    let out = index_text(pool, rel, &text, &sha).await?;
    if out.inserted {
        rep.added += 1;
    } else {
        rep.updated += 1;
    }
    rep.chunks_new += out.chunks_new;
    rep.chunks_reused += out.chunks_reused;
    if let Some(e) = out.parse_error {
        rep.parse_errors.push(ParseError { path: rel.to_string(), error: e });
    }
    Ok(true)
}

struct Indexed {
    inserted: bool,
    chunks_new: usize,
    chunks_reused: usize,
    parse_error: Option<String>,
}

async fn upsert_source(tx: &mut Transaction<'_, Postgres>, uri: &str, title: Option<&str>, kind: &str) -> Result<Uuid> {
    if let Some(id) = sqlx::query_scalar::<_, Uuid>("select id from kb.sources where uri = $1 order by fetched_at limit 1")
        .bind(uri)
        .fetch_optional(&mut **tx)
        .await?
    {
        return Ok(id);
    }
    Ok(sqlx::query_scalar("insert into kb.sources (kind, uri, title) values ($1, $2, $3) returning id")
        .bind(kind)
        .bind(uri)
        .bind(title)
        .fetch_one(&mut **tx)
        .await?)
}

/// Write one document (row, chunks, links, entity) in a single transaction.
async fn index_text(pool: &PgPool, rel: &str, text: &str, sha: &str) -> Result<Indexed> {
    let (fm, body, parse_error) = parse_document(text);
    let layer = layer_for(rel);
    let author = fm.author.clone().unwrap_or_else(|| default_author(layer).to_string());
    let status = match fm.status.as_deref() {
        Some(s @ ("raw" | "draft" | "reviewed")) => s.to_string(),
        _ => "draft".to_string(),
    };
    let title = fm.title.clone().unwrap_or_else(|| stem(rel).to_string());

    let mut tx = pool.begin().await?;
    let source_id = match fm.source.as_deref() {
        Some(u) if u.contains("://") => Some(upsert_source(&mut tx, u, Some(&title), "url").await?),
        _ => None,
    };
    let row = sqlx::query(
        "insert into kb.documents (path, layer, author, status, title, categories, frontmatter, body, sha256,
                                   source_id, created, aliases, private, parse_error, updated_at, indexed_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, now(), now())
         on conflict (path) do update set
           layer = excluded.layer, author = excluded.author, status = excluded.status, title = excluded.title,
           categories = excluded.categories, frontmatter = excluded.frontmatter, body = excluded.body,
           sha256 = excluded.sha256, source_id = excluded.source_id, created = excluded.created,
           aliases = excluded.aliases, private = excluded.private, parse_error = excluded.parse_error,
           updated_at = now(), indexed_at = now()
         returning id, (xmax = 0) as inserted",
    )
    .bind(rel)
    .bind(layer)
    .bind(&author)
    .bind(&status)
    .bind(&title)
    .bind(&fm.categories)
    .bind(serde_json::Value::Object(fm.raw.clone()))
    .bind(body)
    .bind(sha)
    .bind(source_id)
    .bind(fm.created)
    .bind(&fm.aliases)
    .bind(fm.private)
    .bind(&parse_error)
    .fetch_one(&mut *tx)
    .await?;
    let doc_id: Uuid = row.get("id");
    let inserted: bool = row.get("inserted");

    // Chunks: park the old rows at negative ords, insert the new ones copying any embedding whose
    // embed-text hash matches (this doc first, then anywhere), then drop the parked rows.
    let chunks = chunk_markdown(body);
    let mut ords = Vec::with_capacity(chunks.len());
    let mut heads = Vec::with_capacity(chunks.len());
    let mut contents = Vec::with_capacity(chunks.len());
    let mut tokens = Vec::with_capacity(chunks.len());
    let mut shas = Vec::with_capacity(chunks.len());
    let mut line_starts = Vec::with_capacity(chunks.len());
    let mut line_ends = Vec::with_capacity(chunks.len());
    for c in chunks {
        shas.push(sha256_hex(embed_text(&title, &c.heading, &c.content).as_bytes()));
        ords.push(c.ord);
        tokens.push(c.tokens);
        line_starts.push(c.line_start);
        line_ends.push(c.line_end);
        heads.push(c.heading);
        contents.push(c.content);
    }
    sqlx::query("update kb.chunks set ord = -ord - 1 where doc_id = $1 and ord >= 0")
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
    let reused: Vec<bool> = sqlx::query_scalar(
        "insert into kb.chunks (doc_id, ord, heading, content, tokens, sha256, embedding, embed_model_id,
                                title, line_start, line_end)
         select $1, n.ord, n.heading, n.content, n.tokens, n.sha, o.embedding, o.embed_model_id, $7, n.ls, n.le
         from unnest($2::int[], $3::text[], $4::text[], $5::int[], $6::text[], $8::int[], $9::int[])
              as n(ord, heading, content, tokens, sha, ls, le)
         left join lateral (
           select c.embedding, c.embed_model_id from kb.chunks c
           where c.sha256 = n.sha and c.embedding is not null
           order by (c.doc_id = $1) desc, c.embed_model_id desc nulls last
           limit 1
         ) o on true
         returning embedding is not null",
    )
    .bind(doc_id)
    .bind(&ords)
    .bind(&heads)
    .bind(&contents)
    .bind(&tokens)
    .bind(&shas)
    .bind(chunk_title(&title, &fm.aliases))
    .bind(&line_starts)
    .bind(&line_ends)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("delete from kb.chunks where doc_id = $1 and ord < 0").bind(doc_id).execute(&mut *tx).await?;
    let chunks_reused = reused.iter().filter(|r| **r).count();

    // Links (resolved globally after the sync).
    let links = parse_wikilinks(body);
    sqlx::query("delete from kb.links where src_doc = $1").bind(doc_id).execute(&mut *tx).await?;
    if !links.is_empty() {
        let dst: Vec<&str> = links.iter().map(|l| l.dst_text.as_str()).collect();
        let heading: Vec<Option<&str>> = links.iter().map(|l| l.heading.as_deref()).collect();
        let alias: Vec<Option<&str>> = links.iter().map(|l| l.alias.as_deref()).collect();
        let kind: Vec<&str> = links.iter().map(|l| l.kind()).collect();
        sqlx::query(
            "insert into kb.links (src_doc, dst_text, heading, alias, kind)
             select $1, d, h, a, k from unnest($2::text[], $3::text[], $4::text[], $5::text[]) as t(d, h, a, k)",
        )
        .bind(doc_id)
        .bind(&dst)
        .bind(&heading)
        .bind(&alias)
        .bind(&kind)
        .execute(&mut *tx)
        .await?;
    }

    // Entity block.
    let slug = fm.entity.as_ref().map(|e| e.slug.clone().unwrap_or_else(|| slugify(&title)));
    sqlx::query("update kb.entities set doc_id = null where doc_id = $1 and slug is distinct from $2")
        .bind(doc_id)
        .bind(&slug)
        .execute(&mut *tx)
        .await?;
    if let (Some(e), Some(slug)) = (&fm.entity, &slug) {
        let mut aliases = e.aliases.clone();
        for a in &fm.aliases {
            if !aliases.contains(a) {
                aliases.push(a.clone());
            }
        }
        sqlx::query(
            "insert into kb.entities (slug, kind, name, aliases, doc_id) values ($1, $2, $3, $4, $5)
             on conflict (slug) do update set kind = excluded.kind, name = excluded.name,
               aliases = excluded.aliases, doc_id = excluded.doc_id",
        )
        .bind(slug)
        .bind(e.kind.as_deref().unwrap_or("concept"))
        .bind(e.name.as_deref().unwrap_or(&title))
        .bind(&aliases)
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Indexed { inserted, chunks_new: reused.len() - chunks_reused, chunks_reused, parse_error })
}

/// `kb.chunks.title`: the document title plus its aliases, weighted like a heading in full-text search.
fn chunk_title(title: &str, aliases: &[String]) -> String {
    let mut out = title.trim().to_string();
    for a in aliases {
        let a = a.trim();
        if !a.is_empty() && !out.to_lowercase().contains(&a.to_lowercase()) {
            out.push(' ');
            out.push_str(a);
        }
    }
    out
}

/// Point every link at its document (path, title, filename stem, alias; case-insensitive) and entity
/// (slug, name, alias, or the entity declared by the target document). A link that only matches an
/// entity also points at that entity's document. Set-based; idempotent.
pub async fn resolve_links(pool: &PgPool) -> Result<u64> {
    let n = sqlx::query(
        r"with d as (select id, path, title, aliases, (layer = 'template') as tpl from kb.documents),
        keys as (
          select lower(regexp_replace(path, '\.md$', '')) as key, id, 0 as prio, tpl, path from d
          union all select lower(title), id, 1, tpl, path from d
          union all select lower(regexp_replace(path, '^.*/|\.md$', '', 'g')), id, 2, tpl, path from d
          union all select lower(a), id, 3, tpl, path from d, unnest(aliases) a
        ),
        best as (select distinct on (key) key, id from keys order by key, prio, tpl, path),
        ekeys as (
          select lower(slug) as key, id, doc_id, 0 as prio from kb.entities
          union all select lower(name), id, doc_id, 1 from kb.entities
          union all select lower(a), id, doc_id, 2 from kb.entities, unnest(aliases) a
        ),
        ebest as (select distinct on (key) key, id, doc_id from ekeys order by key, prio, id),
        edoc as (select distinct on (doc_id) doc_id, id from kb.entities where doc_id is not null order by doc_id, slug),
        r as (
          select l.id, coalesce(b.id, e.doc_id) as doc, coalesce(e.id, ed.id) as ent
          from kb.links l
          left join best b on b.key = lower(btrim(split_part(l.dst_text, '#', 1)))
          left join ebest e on e.key = lower(btrim(split_part(l.dst_text, '#', 1)))
          left join edoc ed on ed.doc_id = b.id
        )
        update kb.links l set dst_doc = r.doc, dst_entity = r.ent
        from r
        where r.id = l.id and (l.dst_doc is distinct from r.doc or l.dst_entity is distinct from r.ent)",
    )
    .execute(pool)
    .await?;
    Ok(n.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_title_adds_new_aliases_only() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(chunk_title("Huawei Ascend 950DT", &a(&["Ascend 950DT"])), "Huawei Ascend 950DT");
        assert_eq!(chunk_title("NVIDIA GB10", &a(&["Spark chip", " ", "GB10"])), "NVIDIA GB10 Spark chip");
    }

    #[test]
    fn clean_rel_rejects_escape() {
        assert_eq!(clean_rel("Wiki/./a.md").unwrap(), "Wiki/a.md");
        assert_eq!(clean_rel("/docs/Wiki/a.md").unwrap(), "Wiki/a.md");
        assert!(clean_rel("../etc/passwd").is_err());
        assert!(clean_rel("Wiki/../../x.md").is_err());
        assert!(clean_rel("").is_err());
    }

    #[test]
    fn walk_skips_hidden_and_attachments() {
        let d = tempfile::tempdir().unwrap();
        for p in ["a.md", "Wiki/b.md", ".obsidian/c.md", ".git/d.md", "Attachments/e.md", "Wiki/f.png"] {
            let f = d.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        let (paths, _) = list_markdown(d.path());
        assert_eq!(paths, vec!["Wiki/b.md", "a.md"]);
    }
}
