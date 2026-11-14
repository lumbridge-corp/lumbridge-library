//! Read-only health report for the vault index (Karpathy's "lint the wiki" pass).

use super::sync::{ParseError, list_markdown};
use super::{MAX_FILE_BYTES, sha256_hex};
use anyhow::Result;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BrokenLink {
    pub src_path: String,
    pub dst_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DuplicateTitle {
    pub title: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StaleWiki {
    pub path: String,
    /// `updated` (or `created`) of the wiki page.
    pub wiki_date: NaiveDate,
    pub source_path: String,
    pub source_date: NaiveDate,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExpiredFact {
    pub id: Uuid,
    pub entity: String,
    pub attribute: String,
    pub valid_to: NaiveDate,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LintReport {
    /// Wikilinks that resolve to neither a document nor an entity (attachment embeds excluded).
    pub broken_links: Vec<BrokenLink>,
    /// Documents with no links in or out (Daily, meta and templates excluded).
    pub orphans: Vec<String>,
    /// Documents without categories (Daily, meta and templates excluded).
    pub missing_categories: Vec<String>,
    pub duplicate_titles: Vec<DuplicateTitle>,
    pub parse_errors: Vec<ParseError>,
    /// Wiki pages dated before the newest Reference/Clipping they link to.
    pub stale_wiki: Vec<StaleWiki>,
    /// Current facts whose `valid_to` has passed.
    pub expired_facts: Vec<ExpiredFact>,
    pub unembedded_chunks: i64,
    /// Files on disk that are new or changed since the last sync.
    pub unsynced: Vec<String>,
    /// Indexed documents whose file is gone.
    pub missing_files: Vec<String>,
}

const NO_LINT_LAYERS: &str = "('daily', 'meta', 'template')";
const ATTACHMENT_RE: &str = r"\.(png|jpe?g|gif|svg|webp|avif|bmp|pdf|mp4|mov|webm|mp3|wav|m4a|ogg|canvas|base|excalidraw)$";

pub async fn lint(pool: &PgPool, vault_root: &Path) -> Result<LintReport> {
    let broken_links = sqlx::query_as::<_, (String, String)>(&format!(
        "select d.path, l.dst_text from kb.links l join kb.documents d on d.id = l.src_doc
         where l.dst_doc is null and l.dst_entity is null
           and split_part(l.dst_text, '#', 1) !~* '{ATTACHMENT_RE}'
         order by d.path, l.id"
    ))
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(src_path, dst_text)| BrokenLink { src_path, dst_text })
    .collect();

    let orphans = sqlx::query_scalar(&format!(
        "select d.path from kb.documents d
         where d.layer not in {NO_LINT_LAYERS}
           and not exists (select 1 from kb.links l where l.src_doc = d.id)
           and not exists (select 1 from kb.links l where l.dst_doc = d.id)
           and not exists (select 1 from kb.links l join kb.entities e on e.id = l.dst_entity where e.doc_id = d.id)
         order by d.path"
    ))
    .fetch_all(pool)
    .await?;

    let missing_categories = sqlx::query_scalar(&format!(
        "select path from kb.documents where layer not in {NO_LINT_LAYERS} and cardinality(categories) = 0 order by path"
    ))
    .fetch_all(pool)
    .await?;

    let duplicate_titles = sqlx::query_as::<_, (String, Vec<String>)>(
        "select min(title), array_agg(path order by path) from kb.documents
         where layer <> 'template' group by lower(title) having count(*) > 1 order by 1",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(title, paths)| DuplicateTitle { title, paths })
    .collect();

    let parse_errors = sqlx::query_as::<_, (String, String)>(
        "select path, parse_error from kb.documents where parse_error is not null order by path",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(path, error)| ParseError { path, error })
    .collect();

    let stale_wiki = sqlx::query_as::<_, (String, NaiveDate, String, NaiveDate)>(
        r"with w as (
           select id, path, coalesce(
             case when frontmatter->>'updated' ~ '^\d{4}-\d{2}-\d{2}' then left(frontmatter->>'updated', 10)::date end,
             created) as wdate
           from kb.documents where layer = 'wiki'
         )
         select distinct on (w.path) w.path, w.wdate, s.path, s.created
         from w join kb.links l on l.src_doc = w.id
         join kb.documents s on s.id = l.dst_doc
         where s.layer in ('reference', 'clipping') and w.wdate is not null and s.created > w.wdate
         order by w.path, s.created desc",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(path, wiki_date, source_path, source_date)| StaleWiki { path, wiki_date, source_path, source_date })
    .collect();

    let expired_facts = sqlx::query_as::<_, (Uuid, String, String, NaiveDate)>(
        "select f.id, e.slug, f.attribute, f.valid_to from kb.facts f join kb.entities e on e.id = f.entity_id
         where f.superseded_by is null and f.valid_to < current_date order by e.slug, f.attribute",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(id, entity, attribute, valid_to)| ExpiredFact { id, entity, attribute, valid_to })
    .collect();

    let unembedded_chunks: i64 =
        sqlx::query_scalar("select count(*) from kb.chunks where embedding is null").fetch_one(pool).await?;

    // Disk vs index (hashes only; nothing is written).
    let mut known: HashMap<String, String> = sqlx::query_as::<_, (String, String)>("select path, sha256 from kb.documents")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
    let mut unsynced = Vec::new();
    if vault_root.is_dir() {
        let root = vault_root.to_path_buf();
        let (paths, _) = tokio::task::spawn_blocking(move || list_markdown(&root)).await?;
        for rel in paths {
            let prev = known.remove(&rel);
            let full = vault_root.join(&rel);
            match tokio::fs::metadata(&full).await {
                Ok(m) if m.len() <= MAX_FILE_BYTES => {}
                _ => continue,
            }
            let Ok(bytes) = tokio::fs::read(&full).await else { continue };
            if prev.as_deref() != Some(sha256_hex(&bytes).as_str()) {
                unsynced.push(rel);
            }
        }
    }
    let mut missing_files: Vec<String> = known.into_keys().collect();
    missing_files.sort();

    Ok(LintReport {
        broken_links,
        orphans,
        missing_categories,
        duplicate_titles,
        parse_errors,
        stale_wiki,
        expired_facts,
        unembedded_chunks,
        unsynced,
        missing_files,
    })
}
