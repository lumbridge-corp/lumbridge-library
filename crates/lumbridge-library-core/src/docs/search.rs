//! Hybrid search (FTS + HNSW + named-entity docs → RRF → rerank → blend → relevance floor) and
//! `query` (packed context with citations).

use super::facts::{Fact, facts, find_entity};
use super::names::{NameMismatch, find_mismatch, vault_docs};
use super::resolve::mentions;
use super::truncate_chars;
use crate::db::halfvec_literal;
use crate::models::{Embedder, Kind, Reranker};
use crate::rank;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Candidates per retriever.
const RETRIEVE_N: i64 = 50;
/// Fused candidates sent to the reranker.
const RERANK_N: usize = 30;
/// Characters of `title — heading + content` the reranker sees.
const RERANK_CHARS: usize = 2000;
/// Chunks per document in the final list.
const PER_DOC: usize = 2;
/// Query text sent to the embedder and reranker: ≈512 tokens. A pasted essay is cut, not refused.
pub const MAX_QUERY_CHARS: usize = 2048;
/// Distinct terms kept for full-text search (a 5,000-word query is an OR over thousands of lexemes).
pub const MAX_FTS_TERMS: usize = 32;
/// Every search statement runs under this Postgres `statement_timeout`.
const STATEMENT_TIMEOUT: &str = "8s";
/// Documents named in the question (title / alias / entity name, strict word similarity at least
/// this) get their chunks added as a third retrieval list. 0.9 ≈ the name occurs verbatim: at 0.75
/// "DeepSeek" in a question already pulled in "DeepSeek V4", and "Gaudi 5" pulled "Intel Gaudi 3".
const MENTION_MIN_SCORE: f32 = 0.9;
const MENTION_DOCS: usize = 2;
const MENTION_CHUNKS: i64 = 8;
/// Raw reranker probability a passage needs to be returned. Calibrated with a
/// llama-nemotron-rerank-1b-v2 reranker on a private vault of ~800 documents (Nov 2026): the
/// scores are bimodal, answer chunks 0.397–1.0, irrelevant ones below 0.001, and 0.02 sits near the
/// middle of that gap in logit space. Re-run the calibration when the reranker model changes.
pub const MIN_RERANK_PROB: f32 = 0.02;

/// Valid `layers` filter values (`kb.documents.layer`).
pub const LAYERS: [&str; 8] = ["note", "thought", "daily", "clipping", "reference", "wiki", "template", "meta"];

fn default_limit() -> usize {
    10
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// note | thought | daily | clipping | reference | wiki | template | meta (anything else is an error)
    #[serde(default)]
    pub layers: Option<Vec<String>>,
    /// Match any of these categories.
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// Vault path prefix, e.g. `Wiki/`.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Exact vault paths (set by `query` scopes).
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default = "yes")]
    pub include_private: bool,
    /// Templates are excluded unless this is set or `layers` names `template`.
    #[serde(default)]
    pub include_templates: bool,
}

impl SearchParams {
    pub fn new(q: impl Into<String>) -> Self {
        Self {
            q: q.into(),
            limit: default_limit(),
            layers: None,
            categories: None,
            path_prefix: None,
            paths: None,
            author: None,
            include_private: true,
            include_templates: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DocHit {
    pub doc_id: Uuid,
    pub path: String,
    pub title: String,
    pub layer: String,
    pub author: String,
    pub status: String,
    pub heading: String,
    pub ord: i32,
    pub content: String,
    /// Blended rank score, min-max normalised within this result (the top hit is always ≈1.0, so
    /// it says nothing about absolute relevance; use `rerank_prob` for that).
    pub score: f32,
    /// The reranker's raw probability that this passage answers the query (absolute, 0..1).
    /// None when the reranker was unavailable.
    #[serde(default)]
    pub rerank_prob: Option<f32>,
    pub tokens: i32,
    /// 1-based rank in the full-text list.
    pub fts_rank: Option<usize>,
    /// 1-based rank in the vector list.
    pub vec_rank: Option<usize>,
    /// 1-based inclusive body line range this chunk covers, numbered like `docs_read` (line 1 is the
    /// first line after the frontmatter), so `read(path, (line_start, line_end))` shows it in place.
    /// None for chunks indexed before line tracking (re-sync fills them).
    #[serde(default)]
    pub line_start: Option<i32>,
    #[serde(default)]
    pub line_end: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchResult {
    pub hits: Vec<DocHit>,
    /// What was skipped and why (embedder down → FTS only; reranker down → fused order).
    pub degraded: Vec<String>,
    /// True when no passage cleared the reranker's relevance floor: the vault probably does not
    /// answer this. `hits` is then empty; `note` names the best rejected candidate.
    #[serde(default)]
    pub no_good_match: bool,
    #[serde(default)]
    pub note: Option<String>,
    /// The question names a specific model/version the vault has no page for (`Intel Gaudi 5`) while
    /// a sibling exists (`Intel Gaudi 3`): the hits are kept, but they are about that sibling. `note`
    /// says so in words.
    #[serde(default)]
    pub name_mismatch: Option<NameMismatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Citation {
    pub n: usize,
    pub path: String,
    pub title: String,
    pub heading: String,
    pub score: f32,
    #[serde(default)]
    pub rerank_prob: Option<f32>,
    /// Body line range of the cited chunk (see [`DocHit::line_start`]).
    #[serde(default)]
    pub line_start: Option<i32>,
    #[serde(default)]
    pub line_end: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QueryAnswer {
    /// Markdown blocks headed `[n] Title — heading (path)`, facts first when scoped to an entity.
    pub context_md: String,
    pub citations: Vec<Citation>,
    /// Current facts of the scoped entity, if the scope is an entity.
    pub facts: Vec<Fact>,
    pub degraded: Vec<String>,
    /// True when no passage cleared the relevance floor (facts, if any, are still included).
    #[serde(default)]
    pub no_good_match: bool,
    #[serde(default)]
    pub note: Option<String>,
    /// See [`SearchResult::name_mismatch`]; `context_md` then starts with the warning.
    #[serde(default)]
    pub name_mismatch: Option<NameMismatch>,
}

/// Shared WHERE fragment over `d` (kb.documents), params $2..$8.
const FILTER: &str = "($2::text[] is null or d.layer = any($2))
  and ($3::text[] is null or d.categories && $3)
  and ($4::text is null or starts_with(d.path, $4))
  and ($5::text is null or d.author = $5)
  and ($6::bool or not d.private)
  and ($7::bool or d.layer <> 'template')
  and ($8::text[] is null or d.path = any($8))";

/// The OR-able full-text query built from `$1` (AND-matches rank first).
const TSQ: &str = "with q as (select websearch_to_tsquery('english', $1) as qa),
     q2 as (select qa, case when position('!' in qa::text) > 0 or qa::text = '' then qa
                             else replace(qa::text, ' & ', ' | ')::tsquery end as qo from q)";

fn bind_filters<'q>(
    q: sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    p: &'q SearchParams,
) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments> {
    let templates = p.include_templates || p.layers.as_ref().is_some_and(|l| l.iter().any(|x| x == "template"));
    q.bind(&p.layers)
        .bind(&p.categories)
        .bind(p.path_prefix.as_deref().filter(|s| !s.is_empty()))
        .bind(p.author.as_deref().filter(|s| !s.is_empty()))
        .bind(p.include_private)
        .bind(templates)
        .bind(&p.paths)
}

/// A transaction whose statements are cut off after [`STATEMENT_TIMEOUT`].
async fn timed_tx(pool: &PgPool) -> Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("select set_config('statement_timeout', $1, true)").bind(STATEMENT_TIMEOUT).execute(&mut *tx).await?;
    Ok(tx)
}

/// Reject `layers` values that are not layers (they would silently match nothing).
fn check_layers(p: &SearchParams) -> Result<()> {
    if let Some(ls) = &p.layers {
        let bad: Vec<&str> = ls.iter().map(String::as_str).filter(|l| !LAYERS.contains(l)).collect();
        if !bad.is_empty() {
            bail!("unknown layer(s) {bad:?}; valid layers: {}", LAYERS.join(", "));
        }
    }
    Ok(())
}

/// Words that carry no search signal (Postgres drops them too, but only after they used up the cap).
const STOPWORDS: [&str; 40] = [
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "is", "are", "was", "were", "be", "it", "its",
    "this", "that", "these", "those", "with", "as", "at", "by", "from", "what", "which", "who", "how", "when",
    "where", "why", "does", "do", "did", "can", "about", "into", "than",
];

/// The text given to `websearch_to_tsquery`. Short queries pass through unchanged (quotes and `-`
/// keep their meaning); long ones become their first [`MAX_FTS_TERMS`] distinct non-stopword terms.
pub fn fts_text(q: &str) -> String {
    let q = truncate_chars(q.trim(), MAX_QUERY_CHARS * 4);
    if q.split_whitespace().count() <= MAX_FTS_TERMS {
        return q.to_string();
    }
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for w in q.split_whitespace() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if w.is_empty() || STOPWORDS.contains(&w.as_str()) || !seen.insert(w.clone()) {
            continue;
        }
        out.push(w);
        if out.len() >= MAX_FTS_TERMS {
            break;
        }
    }
    out.join(" ")
}

/// What the reranker reads for a chunk: the document title and heading breadcrumb, then the text.
/// Reference chunks often never name their subject ("Memory: 144 GB"); the title does.
pub fn rerank_text(title: &str, heading: &str, content: &str) -> String {
    let head = if heading.is_empty() || heading == title {
        title.to_string()
    } else if heading.starts_with(title) {
        heading.to_string()
    } else {
        format!("{title} — {heading}")
    };
    let text = format!("{head}\n{content}");
    truncate_chars(&text, RERANK_CHARS).to_string()
}

async fn fts_ids(pool: &PgPool, p: &SearchParams, fts_q: &str) -> Result<Vec<i64>> {
    // AND-matches first, then OR-matches (so a natural-language question still finds partial hits).
    let sql = format!(
        "{TSQ}
         select c.id from kb.chunks c join kb.documents d on d.id = c.doc_id, q2
         where c.tsv @@ q2.qo and {FILTER}
         order by (c.tsv @@ q2.qa) desc, ts_rank_cd(c.tsv, q2.qo) desc, c.id
         limit {RETRIEVE_N}"
    );
    let mut tx = timed_tx(pool).await?;
    let ids = bind_filters(sqlx::query_scalar(&sql).bind(fts_q), p).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(ids)
}

async fn vec_ids(pool: &PgPool, p: &SearchParams, qvec: &[f32], model_id: i32) -> Result<Vec<i64>> {
    let sql = format!(
        "select id from (
           select c.id, c.embedding operator(extensions.<=>) $1::extensions.halfvec as dist
           from kb.chunks c join kb.documents d on d.id = c.doc_id
           where c.embed_model_id = $9 and c.embedding is not null and {FILTER}
           order by dist limit {RETRIEVE_N}
         ) s order by dist"
    );
    let mut tx = timed_tx(pool).await?;
    sqlx::query("select set_config('hnsw.ef_search', '100', true), set_config('hnsw.iterative_scan', 'relaxed_order', true)")
        .execute(&mut *tx)
        .await?;
    let lit = halfvec_literal(qvec);
    let ids = bind_filters(sqlx::query_scalar(&sql).bind(&lit), p).bind(model_id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(ids)
}

/// Chunks of the documents the question names (see [`mentions`]), best full-text match first.
async fn mention_ids(pool: &PgPool, p: &SearchParams, text: &str, fts_q: &str) -> Result<Vec<i64>> {
    let mut tx = timed_tx(pool).await?;
    let docs: Vec<Uuid> =
        mentions(&mut *tx, text, MENTION_MIN_SCORE, MENTION_DOCS).await?.into_iter().map(|m| m.doc_id).collect();
    if docs.is_empty() {
        tx.commit().await?;
        return Ok(vec![]);
    }
    let sql = format!(
        "{TSQ}
         select c.id from kb.chunks c join kb.documents d on d.id = c.doc_id, q2
         where c.doc_id = any($9) and {FILTER}
         order by ts_rank_cd(c.tsv, q2.qo) desc, c.ord
         limit {MENTION_CHUNKS}"
    );
    let ids = bind_filters(sqlx::query_scalar(&sql).bind(fts_q), p).bind(&docs).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(ids)
}

/// One fused candidate after reranking, before the relevance floor and the per-document cap.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub hit: DocHit,
    /// RRF score (fused order is the order of the returned list).
    pub fused: f32,
    /// 1-based rank in the named-entity list, if the question named this chunk's document.
    pub mention_rank: Option<usize>,
}

/// Retrieve, fuse and rerank: candidates in fused order with `rerank_prob` filled when the reranker
/// answered. Exposed for calibration runs; [`search`] is the API.
#[doc(hidden)]
pub async fn candidates(
    pool: &PgPool,
    embedder: &Embedder,
    reranker: &Reranker,
    embed_model_id: i32,
    params: &SearchParams,
    degraded: &mut Vec<String>,
) -> Result<Vec<Candidate>> {
    if params.q.trim().is_empty() {
        bail!("empty query");
    }
    check_layers(params)?;
    let q_model = truncate_chars(params.q.trim(), MAX_QUERY_CHARS).to_string();
    if q_model.len() < params.q.trim().len() {
        degraded.push(format!("query cut to {MAX_QUERY_CHARS} characters for the models"));
    }
    let fts_q = fts_text(&params.q);

    let fts = fts_ids(pool, params, &fts_q).await?;
    let ment = match mention_ids(pool, params, &q_model, &fts_q).await {
        Ok(ids) => ids,
        Err(e) => {
            degraded.push(format!("entity lookup failed: {e:#}"));
            vec![]
        }
    };
    let vec = match embedder.embed(std::slice::from_ref(&q_model), Kind::Query).await {
        Ok(v) => match vec_ids(pool, params, &v[0], embed_model_id).await {
            Ok(ids) => ids,
            Err(e) => {
                degraded.push(format!("vector search failed: {e:#}"));
                vec![]
            }
        },
        Err(e) => {
            degraded.push(format!("embedder unavailable, full-text only: {e:#}"));
            vec![]
        }
    };

    let fused = rank::rrf(&[&fts, &vec, &ment]);
    let fused: Vec<(i64, f32)> = fused.into_iter().take(RERANK_N).collect();
    if fused.is_empty() {
        return Ok(vec![]);
    }
    let ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
    let mut tx = timed_tx(pool).await?;
    let rows = sqlx::query(
        "select c.id, c.doc_id, d.path, d.title, d.layer, d.author, d.status, c.heading, c.ord, c.content, c.tokens,
                c.line_start, c.line_end
         from kb.chunks c join kb.documents d on d.id = c.doc_id where c.id = any($1)",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let pos = |l: &[i64]| -> HashMap<i64, usize> { l.iter().enumerate().map(|(i, id)| (*id, i + 1)).collect() };
    let (fts_pos, vec_pos, ment_pos) = (pos(&fts), pos(&vec), pos(&ment));
    let mut by_id: HashMap<i64, DocHit> = rows
        .into_iter()
        .map(|r| {
            let id: i64 = r.get("id");
            (
                id,
                DocHit {
                    doc_id: r.get("doc_id"),
                    path: r.get("path"),
                    title: r.get("title"),
                    layer: r.get("layer"),
                    author: r.get("author"),
                    status: r.get("status"),
                    heading: r.get("heading"),
                    ord: r.get("ord"),
                    content: r.get("content"),
                    score: 0.0,
                    rerank_prob: None,
                    tokens: r.get("tokens"),
                    fts_rank: fts_pos.get(&id).copied(),
                    vec_rank: vec_pos.get(&id).copied(),
                    line_start: r.get("line_start"),
                    line_end: r.get("line_end"),
                },
            )
        })
        .collect();
    // Keep fused order, dropping ids that vanished between queries.
    let mut cands: Vec<Candidate> = fused
        .into_iter()
        .filter_map(|(id, f)| by_id.remove(&id).map(|hit| Candidate { hit, fused: f, mention_rank: ment_pos.get(&id).copied() }))
        .collect();

    let docs: Vec<String> = cands.iter().map(|c| rerank_text(&c.hit.title, &c.hit.heading, &c.hit.content)).collect();
    match reranker.rerank(&q_model, &docs).await {
        Ok(hits) if hits.len() == docs.len() => {
            for h in hits {
                if let Some(c) = cands.get_mut(h.index) {
                    c.hit.rerank_prob = Some(h.relevance_score.clamp(0.0, 1.0));
                }
            }
        }
        other => {
            let why = match other {
                Err(e) => format!("{e:#}"),
                Ok(h) => format!("returned {} scores for {} documents", h.len(), docs.len()),
            };
            degraded.push(format!("reranker unavailable, fused order, no relevance floor: {why}"));
        }
    }
    Ok(cands)
}

/// Hybrid search over chunks. Never fails because a model is down: it degrades and says so.
/// With the reranker up, passages under [`MIN_RERANK_PROB`] are dropped; if none is left the result
/// is empty with `no_good_match` set.
pub async fn search(
    pool: &PgPool,
    embedder: &Embedder,
    reranker: &Reranker,
    embed_model_id: i32,
    params: &SearchParams,
) -> Result<SearchResult> {
    let mut degraded = Vec::new();
    let cands = candidates(pool, embedder, reranker, embed_model_id, params, &mut degraded).await?;
    if cands.is_empty() {
        return Ok(SearchResult {
            hits: vec![],
            degraded,
            no_good_match: true,
            note: Some("nothing in the vault matched the query".into()),
            name_mismatch: None,
        });
    }
    let scores: Vec<f32> = cands.iter().map(|c| c.fused).collect();
    let reranked = cands.iter().all(|c| c.hit.rerank_prob.is_some());
    let order: Vec<(usize, f32)> = if reranked {
        let probs: Vec<f32> = cands.iter().map(|c| c.hit.rerank_prob.unwrap_or(0.0)).collect();
        rank::blend(&scores, &probs)
    } else {
        rank::normalise(&scores).into_iter().enumerate().collect()
    };

    let mut slots: Vec<Option<DocHit>> = cands.into_iter().map(|c| Some(c.hit)).collect();
    let mut best_rejected: Option<DocHit> = None;
    let mut per_doc: HashMap<Uuid, usize> = HashMap::new();
    let mut hits = Vec::new();
    for (i, score) in order {
        let Some(mut h) = slots[i].take() else { continue };
        h.score = score;
        if reranked && h.rerank_prob.unwrap_or(0.0) < MIN_RERANK_PROB {
            if best_rejected.as_ref().is_none_or(|b| b.rerank_prob < h.rerank_prob) {
                best_rejected = Some(h);
            }
            continue;
        }
        let n = per_doc.entry(h.doc_id).or_default();
        if *n >= PER_DOC {
            continue;
        }
        *n += 1;
        hits.push(h);
        if hits.len() >= params.limit.max(1) {
            break;
        }
    }
    let (no_good_match, note) = match (hits.is_empty(), best_rejected) {
        (true, Some(b)) => (
            true,
            Some(format!(
                "no passage cleared the relevance floor ({MIN_RERANK_PROB}); the vault probably does not answer this. \
                 Closest: {} — {} (rerank_prob {:.4})",
                b.path,
                if b.heading.is_empty() { &b.title } else { &b.heading },
                b.rerank_prob.unwrap_or(0.0)
            )),
        ),
        (true, None) => (true, Some("nothing in the vault matched the query".into())),
        _ => (false, None),
    };
    let mut res = SearchResult { hits, degraded, no_good_match, note, name_mismatch: None };
    if !res.hits.is_empty() {
        match name_check(pool, &params.q, &res.hits).await {
            Ok(Some(m)) => {
                res.note = Some(m.note());
                res.name_mismatch = Some(m);
            }
            Ok(None) => {}
            Err(e) => res.degraded.push(format!("name check failed: {e:#}")),
        }
    }
    Ok(res)
}

/// Look-alike name guard (see [`super::names`]): does the question name a version the vault lacks
/// while these hits are about a sibling?
async fn name_check(pool: &PgPool, question: &str, hits: &[DocHit]) -> Result<Option<NameMismatch>> {
    let question = truncate_chars(question.trim(), MAX_QUERY_CHARS);
    let mut tx = timed_tx(pool).await?;
    let docs = vault_docs(&mut *tx, question).await?;
    tx.commit().await?;
    if docs.is_empty() {
        return Ok(None);
    }
    let texts: Vec<String> = hits.iter().map(|h| format!("{}\n{}\n{}", h.title, h.heading, h.content)).collect();
    let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
    Ok(find_mismatch(question, &docs, &texts))
}

/// What a `query` scope turned into.
struct Scope {
    params: SearchParams,
    entity: Option<String>,
}

async fn apply_scope(pool: &PgPool, scope: &str, mut p: SearchParams) -> Result<Scope> {
    let s = super::strip_docs_prefix(scope);
    let entity_paths = |id: Uuid, doc: Option<Uuid>| async move {
        sqlx::query_scalar::<_, String>(
            "select d.path from kb.documents d where d.id = $2
             union select d.path from kb.links l join kb.documents d on d.id = l.src_doc where l.dst_entity = $1",
        )
        .bind(id)
        .bind(doc)
        .fetch_all(pool)
        .await
    };
    if let Some(slug) = s.strip_prefix("entity/") {
        let Some(e) = find_entity(pool, slug).await? else { bail!("unknown entity {slug:?}") };
        let paths = entity_paths(e.id, e.doc_id).await?;
        p.paths = (!paths.is_empty()).then_some(paths);
        return Ok(Scope { params: p, entity: Some(e.slug) });
    }
    if let Some(cat) = s.strip_prefix("category/") {
        p.categories = Some(vec![cat.to_string()]);
        return Ok(Scope { params: p, entity: None });
    }
    // A document: exact path, path + .md, or exact title/alias.
    let doc: Option<String> = sqlx::query_scalar(
        "select path from kb.documents
         where path = $1 or path = $1 || '.md' or lower(title) = lower($1)
            or exists (select 1 from unnest(aliases) a where lower(a) = lower($1))
         order by (path = $1) desc, (path = $1 || '.md') desc, (layer = 'template'), path limit 1",
    )
    .bind(s)
    .fetch_optional(pool)
    .await?;
    if let Some(path) = doc {
        p.paths = Some(vec![path]);
        return Ok(Scope { params: p, entity: None });
    }
    let folder = format!("{}/", s.trim_end_matches('/'));
    let any_under: bool = sqlx::query_scalar("select exists (select 1 from kb.documents where starts_with(path, $1))")
        .bind(&folder)
        .fetch_one(pool)
        .await?;
    if any_under {
        p.path_prefix = Some(folder);
        return Ok(Scope { params: p, entity: None });
    }
    if let Some(e) = find_entity(pool, s).await? {
        let paths = entity_paths(e.id, e.doc_id).await?;
        p.paths = (!paths.is_empty()).then_some(paths);
        return Ok(Scope { params: p, entity: Some(e.slug) });
    }
    let cat: Option<String> = sqlx::query_scalar(
        "select c from kb.documents, unnest(categories) c where lower(c) = lower($1) limit 1",
    )
    .bind(s)
    .fetch_optional(pool)
    .await?;
    if let Some(c) = cat {
        p.categories = Some(vec![c]);
        return Ok(Scope { params: p, entity: None });
    }
    bail!("unknown scope {scope:?} (use an id from resolve)")
}

fn block_header(n: usize, title: &str, heading: &str, path: &str) -> String {
    if heading.is_empty() || heading == title {
        format!("[{n}] {title} ({path})")
    } else {
        format!("[{n}] {title} — {heading} ({path})")
    }
}

/// Answer-ready context: search (optionally scoped by a `resolve` id), then pack the best chunks
/// under `max_tokens` (default 4000) as cited markdown blocks. Passages under the relevance floor
/// are never packed; `no_good_match` says when none were left.
pub async fn query(
    pool: &PgPool,
    embedder: &Embedder,
    reranker: &Reranker,
    embed_model_id: i32,
    question: &str,
    scope: Option<&str>,
    max_tokens: Option<usize>,
) -> Result<QueryAnswer> {
    let max_tokens = max_tokens.unwrap_or(4000).max(1);
    let mut params = SearchParams::new(question);
    params.limit = RERANK_N;
    let mut entity = None;
    if let Some(s) = scope.filter(|s| !s.trim().is_empty()) {
        let sc = apply_scope(pool, s, params).await?;
        params = sc.params;
        entity = sc.entity;
    }
    let res = search(pool, embedder, reranker, embed_model_id, &params).await?;

    let mut md = String::new();
    if let (Some(_), Some(note)) = (&res.name_mismatch, &res.note) {
        md.push_str(&format!("> **Warning:** {note}\n\n"));
    }
    let mut used = rank::approx_tokens(&md);
    let mut fact_rows = Vec::new();
    if let Some(slug) = &entity {
        fact_rows = facts(pool, slug, None, None, false).await?;
        if !fact_rows.is_empty() {
            md.push_str(&format!("## Facts: {}\n\n", fact_rows[0].entity_name));
            for f in &fact_rows {
                md.push_str(&f.to_markdown());
                md.push('\n');
            }
            md.push('\n');
            used = rank::approx_tokens(&md);
        }
    }

    let budget = max_tokens.saturating_sub(used);
    let packed = if budget == 0 && !fact_rows.is_empty() {
        vec![]
    } else {
        rank::take_budget(res.hits, budget.max(1), |h| h.tokens as usize + 20)
    };
    let mut citations = Vec::with_capacity(packed.len());
    for (i, h) in packed.into_iter().enumerate() {
        let n = i + 1;
        md.push_str(&block_header(n, &h.title, &h.heading, &h.path));
        md.push_str("\n\n");
        md.push_str(h.content.trim_end());
        md.push_str("\n\n");
        citations.push(Citation {
            n,
            path: h.path,
            title: h.title,
            heading: h.heading,
            score: h.score,
            rerank_prob: h.rerank_prob,
            line_start: h.line_start,
            line_end: h.line_end,
        });
    }
    if let (true, Some(note)) = (res.no_good_match, &res.note) {
        md.push_str(&format!("_No good match in the vault: {note}_\n"));
    }
    Ok(QueryAnswer {
        context_md: md.trim_end().to_string() + "\n",
        citations,
        facts: fact_rows,
        degraded: res.degraded,
        no_good_match: res.no_good_match,
        note: res.note,
        name_mismatch: res.name_mismatch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header() {
        assert_eq!(block_header(1, "GB10", "GB10 > Memory", "References/GB10.md"), "[1] GB10 — GB10 > Memory (References/GB10.md)");
        assert_eq!(block_header(2, "A", "", "a.md"), "[2] A (a.md)");
    }

    #[test]
    fn rerank_text_leads_with_title() {
        assert_eq!(rerank_text("Huawei Ascend 950DT", "Key specs", "- Memory: 144 GB"), "Huawei Ascend 950DT — Key specs\n- Memory: 144 GB");
        assert_eq!(rerank_text("GB10", "GB10 > Memory", "x"), "GB10 > Memory\nx");
        assert_eq!(rerank_text("Note", "", "x"), "Note\nx");
        assert_eq!(rerank_text("T", "h", &"y".repeat(5000)).chars().count(), RERANK_CHARS);
    }

    #[test]
    fn fts_terms_are_capped_and_deduped() {
        assert_eq!(fts_text("  \"exact phrase\" -minus  "), "\"exact phrase\" -minus");
        let long = "The memory bandwidth of the chip, the MEMORY bandwidth! ".repeat(100) + &(0..100).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let t = fts_text(&long);
        let terms: Vec<&str> = t.split(' ').collect();
        assert_eq!(&terms[..3], &["memory", "bandwidth", "chip"]);
        assert_eq!(terms.len(), MAX_FTS_TERMS);
        assert_eq!(terms.iter().collect::<HashSet<_>>().len(), terms.len());
    }

    #[test]
    fn unknown_layers_are_errors() {
        let mut p = SearchParams::new("x");
        p.layers = Some(vec!["wiki".into(), "refernce".into()]);
        let e = check_layers(&p).unwrap_err().to_string();
        assert!(e.contains("refernce") && e.contains("reference, wiki"), "{e}");
        p.layers = Some(vec!["wiki".into(), "template".into()]);
        assert!(check_layers(&p).is_ok());
    }
}
