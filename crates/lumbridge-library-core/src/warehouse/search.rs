//! Retrieval: hybrid code search (code-aware FTS + HNSW + symbol names → RRF → rerank), exact
//! `git grep`, symbol lookup, file/symbol reads, directory trees, repo resolution and budgeted
//! context for agents.

use super::git;
use super::lang;
use super::terms;
use super::{RepoRow, normalise_slug, require_repo};
use crate::models::{Embedder, Kind, Reranker};
use crate::rank;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const CANDIDATES: i64 = 50;
const RERANK_TOP: usize = 30;
/// Passage the reranker reads per candidate (see `HitRow::rerank_doc`).
const RERANK_DOC_CHARS: usize = 3000;
const RERANK_LINES: usize = 40;
const RERANK_DOC_COMMENT_CHARS: usize = 600;

// ───────────────────────────── types ─────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeSearchParams {
    pub query: String,
    /// Repo slugs (`mirrors/o/n`, `own/x`, `components/y`) or bare names (`tokio`). Empty = all.
    #[serde(default)]
    pub repos: Vec<String>,
    /// mirror | own | component. Empty = all.
    #[serde(default)]
    pub tiers: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Default 10, max 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HitSymbol {
    pub name: String,
    pub kind: String,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeHit {
    pub repo: String,
    pub path: String,
    pub line_start: i32,
    pub line_end: i32,
    pub language: Option<String>,
    pub symbol: Option<HitSymbol>,
    pub content: String,
    pub score: f32,
    pub tokens: i32,
    pub license_spdx: Option<String>,
    pub copyleft: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeSearchResult {
    pub hits: Vec<CodeHit>,
    /// True when the embedder or reranker was unavailable and results are FTS / fused-only.
    pub degraded: bool,
    /// Repos the question named (`rmcp`, `ripgrep`) that the search was restricted to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detected_repos: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
    pub pattern: String,
    /// Extended regex (`git grep -E`); false = fixed string.
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default)]
    pub repos: Vec<String>,
    /// Glob like `*.rs` or `src/**/*.go`.
    #[serde(default)]
    pub path_glob: Option<String>,
    /// Default 200, max 2000.
    #[serde(default)]
    pub max_matches: Option<usize>,
    /// Context lines around each match (max 10).
    #[serde(default)]
    pub context: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GrepMatch {
    pub repo: String,
    pub path: String,
    pub line: usize,
    pub text: String,
    /// A context line, not a match.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub context: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub truncated: bool,
    pub repos_searched: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolQuery {
    /// `Name` (exact first, then prefix, then fuzzy), `prefix*`, `Parent::name` / `Parent.name`, or a
    /// path such as `crate::module::name` / `tokio::spawn` (the nearest qualifier must be the
    /// parent, a directory or file of the symbol's path, or the repo's package name).
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub language: Option<String>,
    /// Default 20, max 200.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, sqlx::FromRow)]
pub struct SymbolHit {
    pub repo: String,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub parent: Option<String>,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub line_start: i32,
    pub line_end: i32,
    pub language: Option<String>,
    pub license_spdx: Option<String>,
    /// exact | prefix | fuzzy
    pub match_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileView {
    pub repo: String,
    pub path: String,
    pub commit: String,
    pub language: Option<String>,
    /// Numbered lines (`{n:>6}  {text}`).
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
    pub total_lines: usize,
    pub truncated: bool,
    pub license_spdx: Option<String>,
    pub copyleft: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolView {
    pub symbol: SymbolHit,
    pub view: FileView,
    /// Other symbols with exactly this name, best first (at most 10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<SymbolHit>,
    /// True when the best candidate is as good a match as `symbol` (pick one by path or parent).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ambiguous: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TreeEntry {
    pub path: String,
    /// dir | file
    pub kind: String,
    /// Files at or beneath this path (1 for a file).
    pub files: usize,
    pub indexed_files: usize,
    pub bytes: u64,
    pub language: Option<String>,
    /// Why a file is not indexed (too_large, vendored_or_generated_dir, lockfile, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TreeView {
    pub repo: String,
    pub commit: String,
    /// The listed directory ("" = repo root).
    pub path: String,
    pub depth: usize,
    pub entries: Vec<TreeEntry>,
    pub total_files: usize,
    pub indexed_files: usize,
    /// More than 1000 entries; list a subdirectory or lower the depth.
    pub truncated: bool,
    /// Repo-wide skip counts from the last index (reason → files).
    pub skipped_by_reason: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepoRef {
    /// `/warehouse/<slug>`
    pub id: String,
    pub tier: String,
    pub slug: String,
    pub description: Option<String>,
    pub license_spdx: Option<String>,
    pub copyleft: Option<bool>,
    pub stars: Option<i32>,
    pub tags: Vec<String>,
    pub languages: Vec<String>,
    /// Component status (draft | approved | retired), for tier component.
    pub status: Option<String>,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Citation {
    pub n: usize,
    pub repo: String,
    pub path: String,
    pub line_start: i32,
    pub line_end: i32,
    pub symbol: Option<String>,
    pub license_spdx: Option<String>,
    pub copyleft: Option<bool>,
    pub score: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeAnswer {
    pub context_md: String,
    pub citations: Vec<Citation>,
    pub degraded: bool,
    /// The repo the scope resolved to, if any.
    pub resolved: Option<String>,
}

// ───────────────────────────── search ─────────────────────────────

/// Largest file [`read`] will load.
const READ_MAX_BYTES: u64 = 8 << 20;
/// Every search statement runs under this timeout.
const SEARCH_TIMEOUT: &str = "8s";
/// Query text sent to the embedder / reranker: ≈512 tokens.
const QUERY_MAX_CHARS: usize = 2048;
/// FTS rows scored per query (before noise demotion and the per-file cap).
const FTS_POOL: i64 = 200;
/// Symbol-name matches per identifier in the question.
const SYMBOLS_PER_TERM: i64 = 40;
/// Chunks per file in a result list.
const PER_FILE: usize = 2;
/// Score multiplier for hits in a repo the question names with a common-word name (`time`, `log`).
const WEAK_REPO_BOOST: f32 = 1.25;

fn like_prefix(p: &str) -> String {
    let mut s = super::like_escape(p);
    s.push('%');
    s
}

fn norm_repos(repos: &[String]) -> Vec<String> {
    repos.iter().map(|r| normalise_slug(r)).filter(|r| !r.is_empty()).collect()
}

/// Shared WHERE fragment: $2 repos, $3 tiers, $4 languages, $5 path LIKE prefix.
const FILTER: &str = "(cardinality($2::text[]) = 0 or r.slug = any($2) or split_part(r.slug, '/', -1) = any($2))
   and (cardinality($3::text[]) = 0 or r.tier = any($3))
   and (cardinality($4::text[]) = 0 or c.language = any($4))
   and ($5::text is null or c.path like $5)";

/// A transaction with `statement_timeout` set, for search SQL.
async fn timed(pool: &PgPool) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("set local statement_timeout = '{SEARCH_TIMEOUT}'")).execute(&mut *tx).await?;
    Ok(tx)
}

/// Validate filter values; returns the repo filter as exact slugs. Unknown values are an error
/// that names the valid choices.
async fn check_filters(pool: &PgPool, repos: &[String], tiers: &[String], languages: &[String]) -> Result<Vec<String>> {
    for t in tiers {
        if !super::TIERS.contains(&t.as_str()) {
            bail!("unknown tier {t:?}; valid tiers: {}", super::TIERS.join(", "));
        }
    }
    if !languages.is_empty() {
        let known: Vec<String> =
            sqlx::query_scalar("select distinct language from wh.files where language is not null order by 1").fetch_all(pool).await?;
        for l in languages {
            if !known.contains(l) {
                bail!("unknown language {l:?}; indexed languages: {}", known.join(", "));
            }
        }
    }
    let repos = norm_repos(repos);
    if repos.is_empty() {
        return Ok(repos);
    }
    let slugs: Vec<String> = sqlx::query_scalar("select slug from wh.repos order by slug").fetch_all(pool).await?;
    let mut out = Vec::new();
    for r in &repos {
        let m: Vec<&String> = slugs
            .iter()
            .filter(|s| *s == r || s.rsplit('/').next() == Some(r.as_str()) || **s == format!("components/{r}"))
            .collect();
        if m.is_empty() {
            let more = if slugs.len() > 40 { ", ..." } else { "" };
            let list: Vec<&str> = slugs.iter().take(40).map(String::as_str).collect();
            bail!("unknown repo {r:?}; warehouse repos: {}{more}", list.join(", "));
        }
        for s in m {
            if !out.contains(s) {
                out.push(s.clone());
            }
        }
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct HitRow {
    id: i64,
    repo: String,
    path: String,
    line_start: i32,
    line_end: i32,
    language: Option<String>,
    content: String,
    tokens: i32,
    license_spdx: Option<String>,
    copyleft: Option<bool>,
    sym_name: Option<String>,
    sym_kind: Option<String>,
    signature: Option<String>,
    sym_parent: Option<String>,
    sym_doc: Option<String>,
}

impl HitRow {
    fn into_hit(self, score: f32) -> CodeHit {
        let symbol = match (self.sym_name, self.sym_kind) {
            (Some(name), Some(kind)) => Some(HitSymbol { name, kind, signature: self.signature }),
            _ => None,
        };
        CodeHit {
            repo: self.repo,
            path: self.path,
            line_start: self.line_start,
            line_end: self.line_end,
            language: self.language,
            symbol,
            content: self.content,
            score,
            tokens: self.tokens,
            license_spdx: self.license_spdx,
            copyleft: self.copyleft,
        }
    }
    /// What the reranker reads: `repo path :: kind Parent::name`, the doc comment, then the first
    /// [`RERANK_LINES`] lines of the chunk (at most [`RERANK_DOC_CHARS`] in all). Measured on the
    /// code question set in `tests/warehouse_eval.rs` (raw logits, same blend): the old
    /// `path\nsignature\n3000 chars` scored MRR 0.65 with prose (GUIDE.md, CHANGELOG.md) at the top
    /// of the reranker's list; this one 0.75 (reranker alone 0.56 → 0.79). 20 and 80 lines did no
    /// better (0.75 / 0.75).
    fn rerank_doc(&self) -> String {
        let repo = self.repo.rsplit('/').next().unwrap_or(&self.repo);
        let mut d = format!("{repo} {}", self.path);
        if let (Some(kind), Some(name)) = (&self.sym_kind, &self.sym_name) {
            d.push_str(" :: ");
            d.push_str(kind);
            d.push(' ');
            if let Some(p) = self.sym_parent.as_deref().filter(|p| !p.is_empty()) {
                d.push_str(p);
                d.push_str("::");
            }
            d.push_str(name);
        }
        d.push('\n');
        if let Some(doc) = self.sym_doc.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            d.push_str(terms::cap_chars(doc, RERANK_DOC_COMMENT_CHARS));
            d.push('\n');
        }
        for line in self.content.lines().take(RERANK_LINES) {
            d.push_str(line);
            d.push('\n');
        }
        terms::cap_chars(d.trim_end(), RERANK_DOC_CHARS).to_string()
    }
    fn noise(&self, wants: &terms::Wants) -> f32 {
        terms::noise_factor(&self.path, self.line_start, self.line_end, &self.content, wants)
    }
}

/// OR-of-terms tsquery text (terms are [a-z0-9] only, so it is always valid).
fn or_query(ts: &[terms::Term]) -> Option<String> {
    (!ts.is_empty()).then(|| ts.iter().map(|t| t.tsquery("")).collect::<Vec<_>>().join(" | "))
}

/// Everything the legs share.
struct Plan<'a> {
    p: &'a CodeSearchParams,
    /// Capped query text for the embedder and reranker.
    text: String,
    repos: Vec<String>,
    prefix: Option<String>,
    terms: Vec<terms::Term>,
    symbols: Vec<(String, bool)>,
}

/// FTS over `c.tsv || t.tsv` (content + code-aware terms). Candidates match any term; they are
/// ranked by the sum over matched terms of idf (within the candidates) × 1.0 for a match in the
/// symbol name / parent (weight A), 0.7 in the path / signature (C), 0.4 in the content (B / D);
/// ties go to `ts_rank_cd`.
async fn fts_ids(pool: &PgPool, pl: &Plan<'_>) -> Result<Vec<i64>> {
    let Some(orq) = or_query(&pl.terms) else { return Ok(vec![]) };
    let q = |w: &str| -> Vec<String> { pl.terms.iter().map(|t| t.tsquery(w)).collect() };
    let sql = format!(
        "with cand as (
           select id from wh.chunks where tsv @@ to_tsquery('simple', $1)
           union
           select chunk_id from wh.chunk_terms where tsv @@ to_tsquery('simple', $1)
         ), docs as (
           select c.id, v.v, ts_rank_cd(v.v, to_tsquery('simple', $1)) as rk
             from cand x join wh.chunks c on c.id = x.id join wh.repos r on r.id = c.repo_id
             left join wh.chunk_terms t on t.chunk_id = c.id
             cross join lateral (select c.tsv || coalesce(t.tsv, ''::tsvector) as v) v
            where {FILTER}
         ), m as (
           select d.id, d.rk, a.i, d.v @@ a.qa as in_a, d.v @@ a.qc as in_c, d.v @@ a.qany as hit
             from docs d
             cross join unnest($6::text[]::tsquery[], $7::text[]::tsquery[], $8::text[]::tsquery[]) with ordinality as a(qany, qa, qc, i)
         ), df as (select i, count(*) filter (where hit) as n from m group by i),
         tot as (select count(*)::float8 as n from docs)
         select m.id from m join df using (i) cross join tot
          group by m.id
          order by sum(case when m.in_a then 1.0 when m.in_c then 0.7 when m.hit then 0.4 else 0 end
                       * ln(1.0 + tot.n / (1.0 + df.n))) desc, max(m.rk) desc, m.id
          limit {FTS_POOL}"
    );
    let mut tx = timed(pool).await?;
    let ids = sqlx::query_scalar::<_, i64>(&sql)
        .bind(&orq)
        .bind(&pl.repos)
        .bind(&pl.p.tiers)
        .bind(&pl.p.languages)
        .bind(&pl.prefix)
        .bind(q(""))
        .bind(q("A"))
        .bind(q("C"))
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ids)
}

async fn vector_ids(pool: &PgPool, embedder: &Embedder, model_id: i32, pl: &Plan<'_>) -> Result<Vec<i64>> {
    let v = embedder.embed(std::slice::from_ref(&pl.text), Kind::Query).await?;
    let lit = crate::db::halfvec_literal(&v[0]);
    let mut tx = timed(pool).await?;
    sqlx::query("set local hnsw.ef_search = 100").execute(&mut *tx).await?;
    // pgvector 0.8: keep scanning the graph when filters drop candidates.
    sqlx::query("set local hnsw.iterative_scan = relaxed_order").execute(&mut *tx).await?;
    let sql = format!(
        "select c.id from wh.chunks c join wh.repos r on r.id = c.repo_id
          where c.embed_model_id = $6 and c.embedding is not null and {FILTER}
          order by c.embedding operator(extensions.<=>) $1::extensions.halfvec limit {}",
        CANDIDATES * 2
    );
    let ids = sqlx::query_scalar::<_, i64>(&sql)
        .bind(&lit)
        .bind(&pl.repos)
        .bind(&pl.p.tiers)
        .bind(&pl.p.languages)
        .bind(&pl.prefix)
        .bind(model_id)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ids)
}

#[derive(sqlx::FromRow)]
struct SymLegRow {
    chunk_id: i64,
    term: i64,
    exact: bool,
    name: String,
    kind: String,
    signature: Option<String>,
    path: String,
}

/// Lower bound / exclusive upper bound for a `text_pattern_ops` prefix range scan.
fn prefix_range(p: &str) -> (String, String) {
    let mut hi = p.to_string();
    match hi.pop() {
        Some(c) => hi.push(char::from_u32(c as u32 + 1).unwrap_or(char::MAX)),
        None => hi.push(char::MAX),
    }
    (p.to_string(), hi)
}

/// Rank of a symbol definition kind for the symbol leg and lookups (lower = better).
fn kind_rank(kind: &str) -> u8 {
    match kind {
        "struct" | "trait" | "enum" | "class" | "interface" | "function" | "macro" => 0,
        "method" | "type" => 1,
        "const" | "static" => 2,
        "impl" => 3,
        _ => 4,
    }
}

/// Visibility rank from a signature (lower = more public): `pub` / `export` / `public` 0,
/// `pub(crate)` / `pub(super)` 1, no modifier where the language has one (Rust/TS keywords)
/// or a `_name` 2. Languages without modifiers (Go, C, Python without `_`) count as 1.
fn vis_rank(sig: Option<&str>, name: &str) -> u8 {
    let mut s = sig.unwrap_or("").trim_start();
    while let Some(r) = s.strip_prefix("#[") {
        s = r.split_once(']').map(|x| x.1.trim_start()).unwrap_or("");
    }
    if s.starts_with("pub ") || s.starts_with("export ") || s.starts_with("public ") {
        return 0;
    }
    if s.starts_with("pub(") {
        return 1;
    }
    const PRIVATE: &[&str] = &[
        "fn ", "async fn ", "unsafe fn ", "const fn ", "struct ", "enum ", "trait ", "union ", "const ", "static ",
        "type ", "mod ", "macro_rules!", "private ", "protected ", "function ", "class ", "interface ",
    ];
    if name.starts_with('_') || PRIVATE.iter().any(|p| s.starts_with(p)) {
        return 2;
    }
    1
}

/// Third ranked list: chunks of symbols whose name equals (or starts with) an identifier in the
/// question. Exact matches first, then real source over tests, public over private, definitions
/// over impl blocks.
async fn symbol_ids(pool: &PgPool, pl: &Plan<'_>) -> Result<Vec<i64>> {
    if pl.symbols.is_empty() {
        return Ok(vec![]);
    }
    let names: Vec<&str> = pl.symbols.iter().map(|s| s.0.as_str()).collect();
    let (los, his): (Vec<String>, Vec<String>) =
        pl.symbols.iter().map(|(n, pre)| if *pre { prefix_range(n) } else { (n.clone(), n.clone()) }).unzip();
    let sql = format!(
        "select ch.id as chunk_id, p.i as term, (lower(s.name) = p.name) as exact, s.name, s.kind, s.signature, f.path
           from unnest($1::text[], $6::text[], $7::text[]) with ordinality as p(name, lo, hi, i)
           cross join lateral (
             select s.id, s.name, s.kind, s.signature, s.file_id from wh.symbols s
               join wh.files c on c.id = s.file_id join wh.repos r on r.id = s.repo_id
              where (lower(s.name) = p.name or (p.lo <> p.hi and lower(s.name) ~>=~ p.lo and lower(s.name) ~<~ p.hi))
                and s.name <> '' and {FILTER}
              order by (lower(s.name) = p.name) desc, length(s.name), s.id
              limit {SYMBOLS_PER_TERM}) s
           join wh.files f on f.id = s.file_id
           cross join lateral (select c2.id from wh.chunks c2 where c2.symbol_id = s.id order by c2.line_start limit 1) ch"
    );
    let mut tx = timed(pool).await?;
    let rows: Vec<SymLegRow> = sqlx::query_as(&sql)
        .bind(&names)
        .bind(&pl.repos)
        .bind(&pl.p.tiers)
        .bind(&pl.p.languages)
        .bind(&pl.prefix)
        .bind(&los)
        .bind(&his)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    // How many of the question's words a name contains (`StreamableHttpService` for "streamable
    // http server" → 2): a compound name that covers the question beats an exact common word.
    let words: Vec<(&str, &str)> = pl.terms.iter().map(|t| (t.word.as_str(), t.stem.as_str())).collect();
    let coverage = |name: &str| -> usize {
        let parts = terms::split_words(name);
        words.iter().filter(|(w, st)| parts.iter().any(|p| p == w || terms::stem(p) == *st)).count()
    };
    let mut rows = rows;
    rows.sort_by_cached_key(|r| {
        (
            r.kind == "module",
            terms::path_kind(&r.path) != terms::PathKind::Source,
            std::cmp::Reverse(coverage(&r.name)),
            !r.exact,
            kind_rank(&r.kind),
            vis_rank(r.signature.as_deref(), &r.name),
            r.term,
            r.name.len(),
        )
    });
    // A plain word (`context`) can name one method per impl block: two per name is enough.
    let mut seen = std::collections::HashSet::new();
    let mut per_name: HashMap<String, usize> = HashMap::new();
    Ok(rows
        .into_iter()
        .filter(|r| {
            let n = per_name.entry(r.name.to_lowercase()).or_default();
            *n += 1;
            *n <= 2 && seen.insert(r.chunk_id)
        })
        .map(|r| r.chunk_id)
        .take(CANDIDATES as usize)
        .collect())
}

async fn hit_rows(pool: &PgPool, ids: &[i64]) -> Result<HashMap<i64, HitRow>> {
    let mut tx = timed(pool).await?;
    let rows: Vec<HitRow> = sqlx::query_as(
        "select c.id, r.slug as repo, c.path, c.line_start, c.line_end, c.language, c.content, c.tokens,
                r.license_spdx, r.copyleft, s.name as sym_name, s.kind as sym_kind, s.signature,
                s.parent as sym_parent, s.doc as sym_doc
           from wh.chunks c join wh.repos r on r.id = c.repo_id left join wh.symbols s on s.id = c.symbol_id
          where c.id = any($1)",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|r| (r.id, r)).collect())
}

/// Re-order one leg: a chunk's position is divided by its noise factor (a test at #2 moves to
/// ~#4), at most [`PER_FILE`] + 1 chunks per file are kept, and the leg is cut to [`CANDIDATES`].
fn tidy_leg(ids: &[i64], meta: &HashMap<i64, HitRow>, wants: &terms::Wants) -> Vec<i64> {
    let mut v: Vec<(f32, i64, &str)> = ids
        .iter()
        .enumerate()
        .filter_map(|(i, id)| meta.get(id).map(|m| ((i as f32 + 1.0) / m.noise(wants), *id, m.path.as_str())))
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut per: HashMap<&str, usize> = HashMap::new();
    v.into_iter()
        .filter(|(_, _, p)| {
            let n = per.entry(p).or_default();
            *n += 1;
            *n <= PER_FILE + 1
        })
        .map(|x| x.1)
        .take(CANDIDATES as usize)
        .collect()
}

/// Repo names a question mentions. Strong (short name, primary package, distinctive package or
/// tag) → filter; weak (a common word such as `time` or `log`) → boost only.
#[derive(Debug, Default, Clone, PartialEq)]
struct Detected {
    strong: Vec<String>,
    weak: Vec<String>,
    /// Question words that named a strong repo (dropped from lexical terms).
    words: std::collections::HashSet<String>,
}

/// Names too generic to restrict a search to one repo.
fn common_name(n: &str) -> bool {
    const COMMON: &[&str] = &[
        "time", "log", "core", "test", "tests", "ignore", "bytes", "regex", "url", "http", "json", "util", "utils",
        "macros", "macro", "cli", "server", "client", "common", "types", "build", "derive", "examples", "example",
        "bench", "benches", "docs", "doc", "error", "errors", "config", "api", "sdk", "rust", "async", "sync", "std",
        "io", "net", "fs", "futures", "stream", "task", "runtime", "parse", "parser", "query", "model", "models",
        "data", "image", "text", "cache", "queue", "search", "index", "hash", "app", "web", "lib", "src", "main",
        "tools", "tool", "rand", "random", "grep", "matcher", "printer", "searcher", "globset", "walk", "context",
        "file", "files", "path", "glob", "lexer", "token", "service", "transport", "protocol", "schema", "proto",
        "codec", "buffer", "pool", "signal", "process", "thread", "threads", "channel", "select", "join", "spawn",
        "debug", "trace", "tracing", "testing", "fuzz", "xtask", "integration", "mock", "mocks", "example-crate",
    ];
    COMMON.contains(&n) || n.len() < 3
}

fn norm_name(n: &str) -> String {
    n.trim().to_lowercase().replace('_', "-")
}

fn detect_in(q: &str, repos: &[(String, Vec<String>, Vec<String>)]) -> Detected {
    let words: Vec<String> = q
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .map(norm_name)
        .map(|w| w.trim_matches('-').to_string())
        .filter(|w| !w.is_empty())
        .collect();
    let mut d = Detected::default();
    for (slug, crates, tags) in repos {
        let short = norm_name(slug.rsplit('/').next().unwrap_or(slug));
        let mut best: Option<(bool, String)> = None; // (strong, word)
        for w in &words {
            let primary = crates.first().map(|c| norm_name(c)) == Some(w.clone());
            let in_crates = crates.iter().any(|c| norm_name(c) == *w || c.strip_prefix('@').and_then(|c| c.split_once('/')).is_some_and(|(_, b)| norm_name(b) == *w));
            let in_tags = tags.iter().any(|t| norm_name(t) == *w);
            let named = *w == short || primary || in_crates || in_tags;
            if !named {
                continue;
            }
            let strong = !common_name(w) && (*w == short || primary || w.len() >= 4);
            if best.as_ref().is_none_or(|b| strong && !b.0) {
                best = Some((strong, w.clone()));
            }
        }
        match best {
            Some((true, w)) => {
                d.strong.push(slug.clone());
                d.words.insert(w.replace('-', ""));
                d.words.insert(w);
            }
            Some((false, _)) => d.weak.push(slug.clone()),
            None => {}
        }
    }
    d
}

async fn detect_repos(pool: &PgPool, q: &str, tiers: &[String]) -> Result<Detected> {
    let repos: Vec<(String, Vec<String>, Vec<String>)> = sqlx::query_as(
        "select slug, crates, tags from wh.repos where cardinality($1::text[]) = 0 or tier = any($1) order by slug",
    )
    .bind(tiers)
    .fetch_all(pool)
    .await?;
    Ok(detect_in(q, &repos))
}

/// Hybrid code search: FTS over code-aware terms, HNSW, and exact/prefix symbol names, fused with
/// RRF and reranked; tests / examples / docs / tiny chunks are demoted unless the question asks
/// for them, and at most two chunks per file are returned. Degrades to the lexical legs when the
/// models are down.
pub async fn search(
    pool: &PgPool,
    embedder: &Embedder,
    reranker: &Reranker,
    embed_model_id: i32,
    p: &CodeSearchParams,
) -> Result<CodeSearchResult> {
    Ok(search_traced(pool, embedder, reranker, embed_model_id, p).await?.0)
}

/// One reranked candidate, for offline ranking experiments (`tests/warehouse_eval.rs`).
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct TraceCand {
    pub repo: String,
    pub path: String,
    pub line_start: i32,
    pub line_end: i32,
    pub sym_name: Option<String>,
    pub sym_kind: Option<String>,
    pub signature: Option<String>,
    pub parent: Option<String>,
    pub doc: Option<String>,
    pub content: String,
    /// Fused score after noise and weak-repo boost (what `rank::blend` gets).
    pub fused: f32,
    pub noise: f32,
    pub rerank_doc: String,
    pub logit: Option<f64>,
}

/// The reranked head of a search, in fused order.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct SearchTrace {
    pub rerank_query: String,
    pub head: Vec<TraceCand>,
}

/// [`search`], also returning the reranked head with its raw scores.
#[doc(hidden)]
pub async fn search_traced(
    pool: &PgPool,
    embedder: &Embedder,
    reranker: &Reranker,
    embed_model_id: i32,
    p: &CodeSearchParams,
) -> Result<(CodeSearchResult, SearchTrace)> {
    let mut trace = SearchTrace::default();
    let text = terms::cap_chars(p.query.trim(), QUERY_MAX_CHARS).to_string();
    if text.is_empty() {
        bail!("empty query");
    }
    let limit = p.limit.unwrap_or(10).clamp(1, 50);
    let explicit = check_filters(pool, &p.repos, &p.tiers, &p.languages).await?;
    let detected = if explicit.is_empty() { detect_repos(pool, &text, &p.tiers).await? } else { Detected::default() };
    let wants = terms::Wants::from_query(&text);
    let prefix = p.path_prefix.as_deref().filter(|s| !s.is_empty()).map(like_prefix);
    let mut degraded = false;

    let mut repos = if explicit.is_empty() { detected.strong.clone() } else { explicit.clone() };
    let mut dropped = detected.words.clone();
    let mut res = None;
    // With a detected repo filter, retry unfiltered if it finds nothing.
    for attempt in 0..2 {
        let pl = Plan {
            p,
            text: text.clone(),
            repos: repos.clone(),
            prefix: prefix.clone(),
            terms: terms::query_terms(&text, &dropped),
            symbols: terms::symbol_candidates(&text, &dropped),
        };
        let (fts, vec, sym) = tokio::join!(fts_ids(pool, &pl), vector_ids(pool, embedder, embed_model_id, &pl), symbol_ids(pool, &pl));
        let fts = fts.unwrap_or_else(|e| {
            tracing::warn!("warehouse search: fts leg failed: {e:#}");
            degraded = true;
            vec![]
        });
        let vec = vec.unwrap_or_else(|e| {
            tracing::warn!("warehouse search: vector leg failed: {e:#}");
            degraded = true;
            vec![]
        });
        let sym = sym.unwrap_or_else(|e| {
            tracing::warn!("warehouse search: symbol leg failed: {e:#}");
            vec![]
        });
        if fts.is_empty() && vec.is_empty() && sym.is_empty() && attempt == 0 && explicit.is_empty() && !repos.is_empty() {
            repos.clear();
            dropped.clear();
            continue;
        }
        res = Some((fts, vec, sym));
        break;
    }
    let Some((fts, vec, sym)) = res else { return Ok((CodeSearchResult { hits: vec![], degraded, detected_repos: vec![] }, trace)) };
    let mut all: Vec<i64> = fts.iter().chain(&vec).chain(&sym).copied().collect();
    all.sort_unstable();
    all.dedup();
    if all.is_empty() {
        return Ok((CodeSearchResult { hits: vec![], degraded, detected_repos: repos }, trace));
    }
    let mut rows = hit_rows(pool, &all).await?;
    let fts = tidy_leg(&fts, &rows, &wants);
    let vec = tidy_leg(&vec, &rows, &wants);
    let fused = rank::rrf(&[&fts, &vec, &sym]);
    // Fused score × noise × weak-repo boost, then at most PER_FILE chunks per file.
    let mut adj: Vec<(i64, f32)> = fused
        .iter()
        .filter_map(|(id, s)| {
            let r = rows.get(id)?;
            let boost = if detected.weak.contains(&r.repo) { WEAK_REPO_BOOST } else { 1.0 };
            Some((*id, s * r.noise(&wants) * boost))
        })
        .collect();
    adj.sort_by(|a, b| b.1.total_cmp(&a.1));
    let adj = cap_per_file(adj, &rows, PER_FILE);
    let take = adj.len().min(limit.max(RERANK_TOP));
    let ids: Vec<i64> = adj[..take].iter().map(|f| f.0).collect();

    let n_rr = take.min(RERANK_TOP);
    let head_ids = &ids[..n_rr];
    let docs: Vec<String> = head_ids.iter().filter_map(|id| rows.get(id)).map(|r| r.rerank_doc()).collect();
    let fused_scores: Vec<f32> = adj[..n_rr].iter().map(|f| f.1).collect();
    let fallback = |fs: &[f32]| -> Vec<(i64, f32)> { head_ids.iter().copied().zip(rank::normalise(fs)).collect() };
    trace.rerank_query = text.clone();
    trace.head = head_ids
        .iter()
        .zip(&fused_scores)
        .zip(&docs)
        .filter_map(|((id, f), d)| {
            let r = rows.get(id)?;
            Some(TraceCand {
                repo: r.repo.clone(),
                path: r.path.clone(),
                line_start: r.line_start,
                line_end: r.line_end,
                sym_name: r.sym_name.clone(),
                sym_kind: r.sym_kind.clone(),
                signature: r.signature.clone(),
                parent: r.sym_parent.clone(),
                doc: r.sym_doc.clone(),
                content: r.content.clone(),
                fused: *f,
                noise: r.noise(&wants),
                rerank_doc: d.clone(),
                logit: None,
            })
        })
        .collect();
    let mut order: Vec<(i64, f32)> = if docs.len() != n_rr {
        fallback(&fused_scores)
    } else {
        match reranker.rerank(&text, &docs).await {
            Ok(hits) => {
                // Raw logits, not probabilities: on code the f32 sigmoid saturates (most of the
                // head is p = 1.0) and the reranker's ordering was lost (see rank::blend_logits).
                let mut logits = vec![f64::NEG_INFINITY; n_rr];
                for h in hits {
                    if h.index < n_rr {
                        if let Some(c) = trace.head.get_mut(h.index) {
                            c.logit = Some(h.logit);
                        }
                        logits[h.index] = h.logit;
                    }
                }
                // A document the server did not score ranks with the worst one it did.
                let worst = logits.iter().copied().filter(|l| l.is_finite()).fold(f64::INFINITY, f64::min);
                for l in logits.iter_mut().filter(|l| !l.is_finite()) {
                    *l = if worst.is_finite() { worst } else { 0.0 };
                }
                rank::blend_logits(&fused_scores, &logits).into_iter().map(|(i, s)| (head_ids[i], s)).collect()
            }
            Err(e) => {
                tracing::warn!("warehouse search: rerank failed: {e:#}");
                degraded = true;
                fallback(&fused_scores)
            }
        }
    };
    // The reranker may lift a test or example again: demote once more after blending.
    for o in order.iter_mut() {
        if let Some(r) = rows.get(&o.0) {
            o.1 *= r.noise(&wants);
        }
    }
    order.sort_by(|a, b| b.1.total_cmp(&a.1));
    // Past the reranked head: keep fused order, below every reranked score.
    let floor = order.iter().map(|o| o.1).fold(f32::INFINITY, f32::min).min(1.0);
    for (k, id) in ids[n_rr..].iter().enumerate() {
        order.push((*id, floor * 0.5 / (k as f32 + 1.0)));
    }
    let hits = order.into_iter().take(limit).filter_map(|(id, s)| rows.remove(&id).map(|r| r.into_hit(s))).collect();
    Ok((CodeSearchResult { hits, degraded, detected_repos: if explicit.is_empty() { repos } else { vec![] } }, trace))
}

fn cap_per_file(v: Vec<(i64, f32)>, rows: &HashMap<i64, HitRow>, max: usize) -> Vec<(i64, f32)> {
    let mut per: HashMap<(&str, &str), usize> = HashMap::new();
    v.into_iter()
        .filter(|(id, _)| match rows.get(id) {
            Some(r) => {
                let n = per.entry((r.repo.as_str(), r.path.as_str())).or_default();
                *n += 1;
                *n <= max
            }
            None => false,
        })
        .collect()
}

// ───────────────────────────── grep ─────────────────────────────

/// Total match / context text a grep returns.
const GREP_MAX_BYTES: usize = 64 * 1024;
/// Characters kept per line.
const GREP_LINE_CHARS: usize = 400;

fn pathspec(glob: &str) -> String {
    let g = glob.trim().trim_start_matches("./");
    if g.contains('/') { format!(":(glob){g}") } else { format!(":(glob)**/{g}") }
}

/// git's message for a pattern it cannot compile (`fatal: -e option, '(': Unmatched ( or \(`).
fn bad_pattern(err: &str) -> bool {
    err.contains("-e option, '") || err.contains("command line, '")
}

/// Exact search with `git grep` at each repo's indexed commit (10 s per repo, 30 s total). Output
/// is capped at `max_matches` and 64 KB of text (lines cut at 400 chars); `truncated` says so.
pub async fn grep(pool: &PgPool, library_root: &Path, p: &GrepParams) -> Result<GrepResult> {
    let _ = library_root;
    if p.pattern.is_empty() {
        bail!("empty pattern");
    }
    if p.pattern.contains('\n') || p.pattern.len() > 1000 {
        bail!("pattern must be one line of at most 1000 bytes");
    }
    let max = p.max_matches.unwrap_or(200).clamp(1, 2000);
    let ctx = p.context.unwrap_or(0).min(10);
    let repos = check_filters(pool, &p.repos, &[], &[]).await?;
    let targets: Vec<(String, String, String)> = sqlx::query_as(
        "select slug, local_path, coalesce(last_indexed_commit, head_commit) from wh.repos
          where coalesce(last_indexed_commit, head_commit) is not null
            and (cardinality($1::text[]) = 0 or slug = any($1))
          order by tier, slug",
    )
    .bind(&repos)
    .fetch_all(pool)
    .await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut out = GrepResult::default();
    let mut nmatch = 0usize;
    let mut bytes = 0usize;
    for (slug, path, commit) in targets {
        if nmatch >= max || bytes >= GREP_MAX_BYTES || Instant::now() >= deadline {
            out.truncated = true;
            break;
        }
        out.repos_searched += 1;
        let budget = Duration::from_secs(10).min(deadline.saturating_duration_since(Instant::now()));
        let lim = GrepLimits { max: max - nmatch, bytes: GREP_MAX_BYTES - bytes };
        match tokio::time::timeout(budget, grep_repo(&slug, Path::new(&path), &commit, p, ctx, lim, &mut out.matches)).await {
            Ok(Ok((n, b, trunc))) => {
                nmatch += n;
                bytes += b;
                out.truncated |= trunc;
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                if bad_pattern(&msg) {
                    bail!("invalid {} pattern {:?}: {}", if p.regex { "regex" } else { "fixed-string" }, p.pattern, msg.rsplit(": ").next().unwrap_or(&msg));
                }
                out.errors.push(format!("{slug}: {msg}"));
            }
            Err(_) => {
                out.truncated = true;
                out.errors.push(format!("{slug}: timed out"));
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy)]
struct GrepLimits {
    max: usize,
    bytes: usize,
}

/// Returns (matches, text bytes, truncated).
async fn grep_repo(
    slug: &str,
    dir: &Path,
    commit: &str,
    p: &GrepParams,
    ctx: usize,
    lim: GrepLimits,
    out: &mut Vec<GrepMatch>,
) -> Result<(usize, usize, bool)> {
    let mut c = git::cmd(Some(dir));
    c.args(["grep", "-n", "-I", "--no-color", "--heading", "--break", "--full-name"]);
    if p.ignore_case {
        c.arg("-i");
    }
    c.arg(if p.regex { "-E" } else { "-F" });
    if ctx > 0 {
        c.arg(format!("-C{ctx}"));
    }
    c.arg("-e").arg(&p.pattern).arg(commit).arg("--");
    if let Some(g) = p.path_glob.as_deref().filter(|g| !g.trim().is_empty()) {
        c.arg(pathspec(g));
    }
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let mut rd = BufReader::new(stdout);
    let heading_prefix = format!("{commit}:");
    let mut cur_path: Option<String> = None;
    let mut n = 0usize;
    let mut bytes = 0usize;
    let mut truncated = false;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // A pathological line (minified file) must not be buffered whole.
        let read = AsyncReadExt::take(&mut rd, 64 * 1024).read_until(b'\n', &mut buf).await?;
        if read == 0 {
            break;
        }
        if !buf.ends_with(b"\n") {
            // Skip the rest of an over-long line.
            let mut sink = Vec::new();
            loop {
                sink.clear();
                let r = AsyncReadExt::take(&mut rd, 64 * 1024).read_until(b'\n', &mut sink).await?;
                if r == 0 || sink.ends_with(b"\n") {
                    break;
                }
            }
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.strip_suffix('\n').unwrap_or(&line);
        if line.is_empty() || line == "--" {
            continue;
        }
        if let Some(path) = line.strip_prefix(&heading_prefix) {
            cur_path = Some(path.to_string());
            continue;
        }
        let Some(path) = &cur_path else { continue };
        let digits = line.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 || digits >= line.len() {
            continue;
        }
        let sep = line.as_bytes()[digits];
        let is_ctx = match sep {
            b':' => false,
            b'-' => true,
            _ => continue,
        };
        if !is_ctx && n >= lim.max {
            truncated = true;
            break;
        }
        let lineno: usize = line[..digits].parse().unwrap_or(0);
        let raw = &line[digits + 1..];
        let mut text = terms::cap_chars(raw, GREP_LINE_CHARS).to_string();
        if text.len() < raw.len() {
            text.push('…');
        }
        if bytes + text.len() + path.len() > lim.bytes {
            truncated = true;
            break;
        }
        bytes += text.len() + path.len();
        out.push(GrepMatch { repo: slug.to_string(), path: path.clone(), line: lineno, text, context: is_ctx });
        if !is_ctx {
            n += 1;
        }
    }
    if truncated {
        let _ = child.start_kill();
        // Drop trailing context lines after the cap.
        while out.last().is_some_and(|m| m.context) {
            out.pop();
        }
        return Ok((n, bytes, true));
    }
    let status = child.wait().await?;
    if !status.success() && status.code() != Some(1) {
        let mut err = String::new();
        if let Some(e) = child.stderr.take() {
            let _ = e.take(4096).read_to_string(&mut err).await;
        }
        bail!("git grep failed: {}", err.trim());
    }
    Ok((n, bytes, false))
}

// ───────────────────────────── symbols ─────────────────────────────

pub(crate) async fn trgm_schema(pool: &PgPool) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "select n.nspname::text from pg_extension e join pg_namespace n on n.oid = e.extnamespace where e.extname = 'pg_trgm'",
    )
    .fetch_optional(pool)
    .await?)
}

/// `a::b::C::f` → (["a", "b", "C"], "f"); `Store.load` → (["Store"], "load"). `crate`, `self`,
/// `super` qualifiers are dropped.
fn split_path(name: &str) -> (Vec<String>, String) {
    let (quals, n): (Vec<&str>, &str) = if name.contains("::") {
        let mut parts: Vec<&str> = name.split("::").collect();
        let n = parts.pop().unwrap_or("");
        (parts, n)
    } else if let Some((p, n)) = name.rsplit_once('.')
        && !p.is_empty()
        && !n.is_empty()
    {
        (p.split('.').collect(), n)
    } else {
        (vec![], name)
    };
    let quals = quals.into_iter().map(str::trim).filter(|q| !q.is_empty() && !matches!(*q, "crate" | "self" | "super")).map(String::from).collect();
    (quals, n.trim().to_string())
}

/// The nearest qualifier, for the `Parent::name` form.
#[cfg(test)]
fn split_qualified(name: &str) -> (Option<String>, String) {
    let (q, n) = split_path(name);
    (q.last().cloned(), n)
}

/// Symbol lookup: exact, then prefix, then trigram (if pg_trgm). `foo*` = prefix only.
/// Qualified names (`Parent::name`, `crate::module::name`, `tokio::spawn`) match the nearest
/// qualifier as the symbol's parent, a directory / file of its path, or the repo's package name.
/// Within a match kind: real source before tests / examples / benches, `pub` first, definitions
/// before impl blocks.
pub async fn symbols(pool: &PgPool, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
    let raw = q.name.trim();
    let prefix_only = raw.ends_with('*');
    let (quals, name) = split_path(raw.trim_end_matches('*'));
    if name.is_empty() || name.chars().all(|c| c == '*') {
        bail!("symbol name is empty (use `prefix*` to list symbols by prefix)");
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 200) as i64;
    let repos = check_filters(pool, &q.repos, &[], &q.language.iter().cloned().collect::<Vec<_>>()).await?;
    let parent = quals.last().cloned();
    let trgm = if prefix_only || name.chars().count() < 3 { None } else { trgm_schema(pool).await? };
    // $1 name, $2 lower-prefix LIKE, $3 repos, $4 kind, $5 language, $6 parent, $7 limit,
    // $8 parent as a path directory LIKE, $9 parent as a file stem LIKE, $10 parent as a package name.
    let filter = "(cardinality($3::text[]) = 0 or r.slug = any($3))
        and ($4::text is null or s.kind = $4) and ($5::text is null or f.language = $5)
        and s.name <> ''
        and ($6::text is null or s.parent = $6 or lower(s.parent) = lower($6)
             or ('/' || lower(f.path)) like $8 or ('/' || lower(f.path)) like $9
             or exists (select 1 from unnest(r.crates) k where replace(lower(k), '-', '_') = $10)
             or replace(lower(split_part(r.slug, '/', -1)), '-', '_') = $10)";
    let from = "from wh.symbols s join wh.files f on f.id = s.file_id join wh.repos r on r.id = s.repo_id";
    let mut branches = Vec::new();
    if !prefix_only {
        branches.push(format!("select s.id, 0 as rank, 1.0::real as sim {from} where s.name = $1 and {filter}"));
    }
    branches.push(format!("select s.id, 1 as rank, 0.9::real as sim {from} where lower(s.name) like $2 and {filter}"));
    if let Some(ns) = &trgm {
        branches.push(format!(
            "(select s.id, 2 as rank, {ns}.similarity(s.name, $1)::real as sim {from}
               where s.name operator({ns}.%) $1 and {filter}
               order by 3 desc limit 500)"
        ));
    }
    let sql = format!(
        "with cand as ({}), best as (select id, min(rank) as rank, max(sim) as sim from cand group by id)
         select r.slug as repo, f.path, s.name, s.kind, s.parent, s.signature, s.doc, s.line_start, s.line_end,
                f.language, r.license_spdx,
                (case b.rank when 0 then 'exact' when 1 then 'prefix' else 'fuzzy' end) as match_kind
           from best b join wh.symbols s on s.id = b.id join wh.files f on f.id = s.file_id join wh.repos r on r.id = s.repo_id
          order by b.rank,
                   ($6::text is not null and s.parent is distinct from $6),
                   (f.path ~* '(^|/)(tests?|testing|benches|benchmarks?|examples?|fuzz)/' or f.path ~* '(_test|_tests|_spec)\\.[a-z]+$'),
                   (case when coalesce(s.signature, '') ~ '^(#\\[[^]]*\\]\\s*)*(pub|export|public)\\s' then 0
                         when coalesce(s.signature, '') ~ '^(#\\[[^]]*\\]\\s*)*pub\\(' then 1
                         when s.name like '\\_%' or coalesce(s.signature, '') ~ '^(#\\[[^]]*\\]\\s*)*(fn|async fn|unsafe fn|const fn|struct|enum|trait|union|const|static|type|mod|macro_rules!|private|protected)\\M' then 2
                         else 1 end),
                   (s.kind = 'impl'), b.sim desc, length(s.name), r.slug, length(f.path), f.path, s.line_start
          limit $7",
        branches.join(" union all ")
    );
    let lower_prefix = like_prefix(&name.to_lowercase());
    let pl = parent.as_deref().map(|p| super::like_escape(&p.to_lowercase()));
    let as_dir = pl.as_ref().map(|p| format!("%/{p}/%"));
    let as_file = pl.as_ref().map(|p| format!("%/{p}.%"));
    let as_pkg = parent.as_deref().map(|p| p.to_lowercase().replace('-', "_"));
    let mut tx = timed(pool).await?;
    let rows = sqlx::query_as::<_, SymbolHit>(&sql)
        .bind(&name)
        .bind(&lower_prefix)
        .bind(&repos)
        .bind(&q.kind)
        .bind(&q.language)
        .bind(&parent)
        .bind(limit)
        .bind(&as_dir)
        .bind(&as_file)
        .bind(&as_pkg)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

// ───────────────────────────── read ─────────────────────────────

const READ_MAX_LINES: usize = 2000;

fn indexed_commit(repo: &RepoRow) -> Result<String> {
    repo.last_indexed_commit
        .clone()
        .or_else(|| repo.head_commit.clone())
        .ok_or_else(|| anyhow::anyhow!("{} has no commit yet; sync it first", repo.slug))
}

fn clean_rel_path(path: &str) -> Result<&str> {
    let path = path.trim().trim_start_matches("./").trim_start_matches('/').trim_end_matches('/');
    if path.split('/').any(|s| s == "..") {
        bail!("invalid path");
    }
    Ok(path)
}

/// Read a file (optionally a 1-based inclusive line range) at the repo's indexed commit.
/// Directories are refused with a pointer to [`tree`].
pub async fn read(
    pool: &PgPool,
    library_root: &Path,
    repo_slug: &str,
    path: &str,
    lines: Option<(usize, usize)>,
) -> Result<FileView> {
    let _ = library_root;
    let repo = require_repo(pool, repo_slug).await?;
    let commit = indexed_commit(&repo)?;
    let path = clean_rel_path(path)?;
    let tree_hint = |p: &str| {
        format!(
            "{} is a directory in {}; list it with warehouse_tree (repo {:?}, path_prefix {:?}) and read a file inside it",
            if p.is_empty() { "/" } else { p },
            repo.slug,
            repo.slug,
            p
        )
    };
    if path.is_empty() {
        bail!(tree_hint(path));
    }
    let dir = PathBuf::from(&repo.local_path);
    let bytes = match git::show(&dir, &commit, path, READ_MAX_BYTES).await? {
        git::Blob::Data(b) => b,
        git::Blob::TooLarge(n) => bail!("{path} is {n} bytes, larger than the 8 MB read limit"),
        git::Blob::Missing => {
            let kind = git::run(Some(&dir), &["cat-file", "-t", &format!("{commit}:{path}")], git::SHORT).await.unwrap_or_default();
            if kind.trim() == "tree" {
                bail!(tree_hint(path));
            }
            bail!("{path} not found in {} at {}", repo.slug, &commit[..commit.len().min(12)])
        }
    };
    if bytes[..bytes.len().min(8000)].contains(&0) {
        bail!("{path} is binary");
    }
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    let (lo, hi) = match lines {
        Some((a, b)) => (a.max(1), b.max(a.max(1)).min(total.max(1))),
        None => (1, total.max(1)),
    };
    let hi_cap = hi.min(lo + READ_MAX_LINES - 1);
    let truncated = hi_cap < hi;
    let mut content = String::new();
    for (i, l) in all.iter().enumerate().take(hi_cap).skip(lo - 1) {
        content.push_str(&format!("{:>6}  {}\n", i + 1, l));
    }
    let language = lang::detect(path).map(|l| l.name().to_string());
    Ok(FileView {
        repo: repo.slug.clone(),
        path: path.to_string(),
        commit,
        language,
        content,
        line_start: lo,
        line_end: hi_cap.min(total),
        total_lines: total,
        truncated,
        license_spdx: repo.license_spdx.clone(),
        copyleft: repo.copyleft,
    })
}

/// Find a symbol in a repo (`name`, `Parent::name`, `crate::module::name`) and return its block.
/// Only exact names are read: with none, the error lists near candidates. Several exact matches
/// come back in `candidates` (best first; `ambiguous` when the runner-up is as good a match).
pub async fn read_symbol(pool: &PgPool, library_root: &Path, repo_slug: &str, symbol_name: &str) -> Result<SymbolView> {
    let repo = require_repo(pool, repo_slug).await?;
    let q = SymbolQuery { name: symbol_name.trim_end_matches('*').to_string(), repos: vec![repo.slug.clone()], limit: Some(50), ..Default::default() };
    let hits = symbols(pool, &q).await?;
    let mut exact: Vec<SymbolHit> = hits.iter().filter(|h| h.match_kind == "exact").cloned().collect();
    if exact.is_empty() {
        let near: Vec<String> = hits
            .iter()
            .take(8)
            .map(|h| {
                let parent = h.parent.as_deref().map(|p| format!("{p}::")).unwrap_or_default();
                format!("{parent}{} ({} {}:{})", h.name, h.kind, h.path, h.line_start)
            })
            .collect();
        if near.is_empty() {
            bail!("no symbol {symbol_name:?} in {}", repo.slug);
        }
        bail!("no symbol named exactly {symbol_name:?} in {}; candidates: {}", repo.slug, near.join(", "));
    }
    // Definitions over impl blocks (the SQL order already put source / pub first).
    exact.sort_by_key(|h| (h.kind == "impl", terms::path_kind(&h.path) != terms::PathKind::Source));
    let sym = exact.remove(0);
    let class = |h: &SymbolHit| {
        (h.kind == "impl", terms::path_kind(&h.path) != terms::PathKind::Source, vis_rank(h.signature.as_deref(), &h.name), kind_rank(&h.kind))
    };
    let ambiguous = exact.first().is_some_and(|h| class(h) == class(&sym) && (h.path != sym.path || h.parent != sym.parent));
    exact.truncate(10);
    let view = read(pool, library_root, &repo.slug, &sym.path, Some((sym.line_start as usize, sym.line_end as usize))).await?;
    Ok(SymbolView { symbol: sym, view, candidates: exact, ambiguous })
}

// ───────────────────────────── tree ─────────────────────────────

const TREE_MAX_ENTRIES: usize = 1000;

/// Directory listing at the repo's indexed commit, from git (every file, indexed or not) joined
/// with `wh.files`. Directories carry counts of everything beneath them; files not indexed carry
/// the skip reason. `depth` levels below `path_prefix` (default 2, max 6).
pub async fn tree(pool: &PgPool, library_root: &Path, repo_slug: &str, path_prefix: Option<&str>, depth: Option<usize>) -> Result<TreeView> {
    let _ = library_root;
    let repo = require_repo(pool, repo_slug).await?;
    let commit = indexed_commit(&repo)?;
    let prefix = clean_rel_path(path_prefix.unwrap_or(""))?.to_string();
    let depth = depth.unwrap_or(2).clamp(1, 6);
    let listing = git::ls_tree(Path::new(&repo.local_path), &commit).await?;
    let indexed: HashMap<String, Option<String>> = sqlx::query_as::<_, (String, Option<String>)>(
        "select path, language from wh.files where repo_id = $1::uuid and ($2 = '' or path = $2 or path like $3)",
    )
    .bind(&repo.id)
    .bind(&prefix)
    .bind(like_prefix(&format!("{prefix}/")))
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let base = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
    let mut dirs: std::collections::BTreeMap<String, TreeEntry> = std::collections::BTreeMap::new();
    let mut files: Vec<TreeEntry> = Vec::new();
    let (mut total, mut total_indexed) = (0usize, 0usize);
    for e in &listing.entries {
        let rel = if e.path == prefix && !prefix.is_empty() {
            // The prefix is a file itself.
            e.path.rsplit('/').next().unwrap_or(&e.path)
        } else if let Some(r) = e.path.strip_prefix(&base) {
            r
        } else {
            continue;
        };
        total += 1;
        let lang = indexed.get(&e.path);
        let is_indexed = lang.is_some();
        total_indexed += usize::from(is_indexed);
        let segs: Vec<&str> = rel.split('/').collect();
        for d in 1..segs.len().min(depth + 1) {
            let dpath = format!("{base}{}", segs[..d].join("/"));
            let ent = dirs.entry(dpath.clone()).or_insert_with(|| TreeEntry {
                path: dpath,
                kind: "dir".into(),
                files: 0,
                indexed_files: 0,
                bytes: 0,
                language: None,
                skipped: None,
            });
            ent.files += 1;
            ent.indexed_files += usize::from(is_indexed);
            ent.bytes += e.size;
        }
        if segs.len() <= depth {
            let skipped = if is_indexed {
                None
            } else {
                Some(lang::skip_reason(&e.path, e.size, super::index::MAX_FILE_BYTES).unwrap_or("not_indexed (binary, generated or minified)").to_string())
            };
            files.push(TreeEntry {
                path: e.path.clone(),
                kind: "file".into(),
                files: 1,
                indexed_files: usize::from(is_indexed),
                bytes: e.size,
                language: lang.cloned().flatten(),
                skipped,
            });
        }
    }
    if total == 0 {
        let top: Vec<String> = {
            let mut t: Vec<&str> = listing.entries.iter().filter_map(|e| e.path.split_once('/').map(|x| x.0)).collect();
            t.sort_unstable();
            t.dedup();
            t.into_iter().take(40).map(String::from).collect()
        };
        bail!("no directory or file {prefix:?} in {}; top-level directories: {}", repo.slug, top.join(", "));
    }
    let mut entries: Vec<TreeEntry> = dirs.into_values().chain(files).collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let truncated = entries.len() > TREE_MAX_ENTRIES;
    entries.truncate(TREE_MAX_ENTRIES);
    let skipped_by_reason = sqlx::query_scalar::<_, Option<serde_json::Value>>("select health->'skipped_by_reason' from wh.repos where id = $1::uuid")
        .bind(&repo.id)
        .fetch_one(pool)
        .await?
        .unwrap_or(serde_json::Value::Null);
    Ok(TreeView {
        repo: repo.slug.clone(),
        commit,
        path: prefix,
        depth,
        entries,
        total_files: total,
        indexed_files: total_indexed,
        truncated,
        skipped_by_reason,
    })
}

// ───────────────────────────── resolve ─────────────────────────────

#[derive(sqlx::FromRow)]
struct RefRow {
    tier: String,
    slug: String,
    description: Option<String>,
    license_spdx: Option<String>,
    copyleft: Option<bool>,
    stars: Option<i32>,
    tags: Vec<String>,
    languages: Vec<String>,
    status: Option<String>,
    score: f32,
}

/// Escape a string for a Postgres ARE.
fn regex_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if "\\.^$|?*+()[]{}-".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// Repos and components matching a name. Scores: exact slug 1.0, short name 0.95, primary
/// package name 0.9 (other package 0.85), tag 0.8, slug substring 0.6, package substring 0.55,
/// a standalone word of the description 0.4, trigram similarity of the name × 0.8.
pub async fn resolve(pool: &PgPool, name: &str, limit: usize) -> Result<Vec<RepoRef>> {
    let n = normalise_slug(name).to_lowercase();
    if n.is_empty() {
        return Ok(vec![]);
    }
    let limit = limit.clamp(1, 100) as i64;
    let trgm = match trgm_schema(pool).await? {
        Some(ns) => format!(
            "greatest({ns}.similarity(lower(r.slug), $1), {ns}.similarity(lower(split_part(r.slug, '/', -1)), $1)) * 0.8"
        ),
        None => "0.0".to_string(),
    };
    let sql = format!(
        "select * from (
           select r.tier, r.slug, coalesce(c.summary, r.description) as description, r.license_spdx, r.copyleft, r.stars,
                  r.tags, r.languages, c.status,
                  greatest(
                    case when lower(r.slug) = $1 then 1.0 else 0 end,
                    case when lower(split_part(r.slug, '/', -1)) = $1 then 0.95 else 0 end,
                    (select max(case when k.i = 1 then 0.9 else 0.85 end) from unnest(r.crates) with ordinality k(n, i)
                      where replace(lower(k.n), '_', '-') = $4 or lower(k.n) like '@%/' || $2),
                    case when exists (select 1 from unnest(r.tags || coalesce(c.tags, '{{}}')) t where lower(t) = $1) then 0.8 else 0 end,
                    case when length($1) >= 3 and lower(r.slug) like '%' || $2 || '%' then 0.6 else 0 end,
                    case when length($1) >= 3 and exists (select 1 from unnest(r.crates) k where lower(k) like '%' || $2 || '%') then 0.55 else 0 end,
                    case when length($1) >= 3 and coalesce(c.summary, r.description, '') ~* ('(^|[^-[:alnum:]_])' || $5 || '([^-[:alnum:]_]|$)') then 0.4 else 0 end,
                    {trgm}
                  )::real as score
             from wh.repos r left join wh.components c on c.repo_id = r.id) x
          where score > 0.3
          order by score desc, (status = 'retired') nulls first, stars desc nulls last, slug
          limit $3"
    );
    let pat = super::like_escape(&n);
    let rows: Vec<RefRow> = sqlx::query_as(&sql)
        .bind(&n)
        .bind(&pat)
        .bind(limit)
        .bind(n.replace('_', "-"))
        .bind(regex_escape(&n))
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| RepoRef {
            id: format!("/warehouse/{}", r.slug),
            tier: r.tier,
            slug: r.slug,
            description: r.description,
            license_spdx: r.license_spdx,
            copyleft: r.copyleft,
            stars: r.stars,
            tags: r.tags,
            languages: r.languages,
            status: r.status,
            score: r.score,
        })
        .collect())
}

// ───────────────────────────── query / card ─────────────────────────────

pub async fn card(pool: &PgPool, slug: &str) -> Result<Option<String>> {
    let Some(repo) = super::get_repo(pool, &normalise_slug(slug)).await? else { return Ok(None) };
    Ok(sqlx::query_scalar::<_, Option<String>>("select card_md from wh.repos where slug = $1")
        .bind(&repo.slug)
        .fetch_optional(pool)
        .await?
        .flatten())
}

/// The card up to (not including) the entry points / key files, at most ~600 tokens.
fn card_header(card: &str) -> String {
    let head = card.split("\n## Key files").next().unwrap_or(card);
    let head = head.split("\n## Entry points").next().unwrap_or(head);
    let mut out = String::new();
    for l in head.lines() {
        if rank::approx_tokens(&out) + rank::approx_tokens(l) > 600 {
            break;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Budgeted code context for an agent's question, optionally scoped to one repo / component.
/// Without a scope, a repo the question names (`rmcp`, `tokio`) scopes the search.
pub async fn query(
    pool: &PgPool,
    embedder: &Embedder,
    reranker: &Reranker,
    embed_model_id: i32,
    question: &str,
    scope: Option<&str>,
    max_tokens: Option<usize>,
) -> Result<CodeAnswer> {
    let max_tokens = max_tokens.unwrap_or(6000).max(200);
    let mut md = String::new();
    let mut resolved = None;
    if let Some(s) = scope.filter(|s| !s.trim().is_empty()) {
        let repo = match super::get_repo(pool, s).await? {
            Some(r) => r,
            None => {
                let refs = resolve(pool, s, 1).await?;
                match refs.first() {
                    Some(r) if r.score >= 0.85 => require_repo(pool, &r.slug).await?,
                    _ => bail!("scope {s:?} does not resolve to a warehouse repo"),
                }
            }
        };
        if let Some(c) = card(pool, &repo.slug).await? {
            md.push_str(&card_header(&c));
            md.push_str("\n\n");
        }
        resolved = Some(repo.slug);
    }
    let params = CodeSearchParams {
        query: question.to_string(),
        repos: resolved.iter().cloned().collect(),
        limit: Some(20),
        ..Default::default()
    };
    let res = search(pool, embedder, reranker, embed_model_id, &params).await?;
    if resolved.is_none()
        && let [only] = res.detected_repos.as_slice()
    {
        resolved = Some(only.clone());
    }
    let used = rank::approx_tokens(&md);
    let budget = max_tokens.saturating_sub(used).max(100);
    let hits = rank::take_budget(res.hits, budget, |h| h.tokens.max(0) as usize + 30);
    let mut citations = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        let n = i + 1;
        let sym = h.symbol.as_ref().map(|s| format!(" ({} {})", s.kind, s.name)).unwrap_or_default();
        let lic = match (&h.license_spdx, h.copyleft) {
            (Some(l), Some(true)) => format!("{l}, copyleft"),
            (Some(l), _) => l.clone(),
            (None, _) => "license unknown".to_string(),
        };
        md.push_str(&format!(
            "[{n}] {}:{}:L{}-L{}{sym} — {lic}\n```{}\n{}\n```\n\n",
            h.repo,
            h.path,
            h.line_start,
            h.line_end,
            lang::fence_for(h.language.as_deref()),
            h.content
        ));
        citations.push(Citation {
            n,
            repo: h.repo.clone(),
            path: h.path.clone(),
            line_start: h.line_start,
            line_end: h.line_end,
            symbol: h.symbol.as_ref().map(|s| s.name.clone()),
            license_spdx: h.license_spdx.clone(),
            copyleft: h.copyleft,
            score: h.score,
        });
    }
    Ok(CodeAnswer { context_md: md.trim_end().to_string(), citations, degraded: res.degraded, resolved })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(like_prefix("src/a_b%"), "src/a\\_b\\%%");
        assert_eq!(pathspec("*.rs"), ":(glob)**/*.rs");
        assert_eq!(pathspec("src/**/*.go"), ":(glob)src/**/*.go");
        assert_eq!(split_qualified("Point::new"), (Some("Point".into()), "new".into()));
        assert_eq!(split_qualified("a::b::C::f"), (Some("C".into()), "f".into()));
        assert_eq!(split_qualified("Store.load"), (Some("Store".into()), "load".into()));
        assert_eq!(split_qualified("plain"), (None, "plain".into()));
        assert_eq!(split_qualified("crate::f"), (None, "f".into()));
        assert_eq!(split_path("crate::runtime::task::spawn"), (vec!["runtime".to_string(), "task".to_string()], "spawn".into()));
        let c = "# x\n\ndesc\n\n- **License:** MIT\n\n## Entry points\n\n- `src/lib.rs`\n\n## Key files\n...";
        assert_eq!(card_header(c), "# x\n\ndesc\n\n- **License:** MIT");
        assert_eq!(prefix_range("abc"), ("abc".into(), "abd".into()));
        assert_eq!(regex_escape("a.b-c"), "a\\.b\\-c");
        assert!(bad_pattern("git grep failed: fatal: -e option, '(': Unmatched ( or \\("));
    }

    #[test]
    fn visibility_and_kinds() {
        assert_eq!(vis_rank(Some("pub fn spawn<F>(f: F)"), "spawn"), 0);
        assert_eq!(vis_rank(Some("#[track_caller] pub fn spawn<F>(f: F)"), "spawn"), 0);
        assert_eq!(vis_rank(Some("pub(crate) fn spawn()"), "spawn"), 1);
        assert_eq!(vis_rank(Some("fn spawn()"), "spawn"), 2);
        assert_eq!(vis_rank(Some("func Spawn()"), "Spawn"), 1);
        assert_eq!(vis_rank(Some("def _private(x)"), "_private"), 2);
        assert!(kind_rank("struct") < kind_rank("impl"));
    }

    #[test]
    fn repo_detection() {
        let repos = vec![
            ("mirrors/BurntSushi/ripgrep".to_string(), vec!["ripgrep".to_string(), "grep".into(), "ignore".into()], vec![]),
            ("mirrors/modelcontextprotocol/rust-sdk".to_string(), vec!["rmcp".to_string(), "rmcp-macros".into()], vec!["mcp".to_string()]),
            ("mirrors/time-rs/time".to_string(), vec!["time".to_string()], vec![]),
            ("mirrors/tokio-rs/tokio".to_string(), vec!["tokio".to_string(), "tokio-util".into()], vec![]),
        ];
        let d = detect_in("ripgrep gitignore matching", &repos);
        assert_eq!(d.strong, ["mirrors/BurntSushi/ripgrep"]);
        assert!(d.words.contains("ripgrep"));
        let d = detect_in("rmcp streamable HTTP server", &repos);
        assert_eq!(d.strong, ["mirrors/modelcontextprotocol/rust-sdk"]);
        let d = detect_in("how do I parse time strings", &repos);
        assert!(d.strong.is_empty());
        assert_eq!(d.weak, ["mirrors/time-rs/time"]);
        let d = detect_in("tokio_util codec", &repos);
        assert_eq!(d.strong, ["mirrors/tokio-rs/tokio"]);
        let d = detect_in("ignore files", &repos);
        assert!(d.strong.is_empty() && d.weak == ["mirrors/BurntSushi/ripgrep"]);
        assert_eq!(detect_in("parse a config file", &repos), Detected::default());
    }
}
