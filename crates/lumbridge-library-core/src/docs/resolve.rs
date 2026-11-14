//! Context7-style resolve: turn a name into ids (`/docs/<path>`, `/docs/entity/<slug>`,
//! `/docs/category/<name>`) using pg_trgm similarity, ILIKE and full-text.

use super::truncate_chars;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row};
use uuid::Uuid;

/// Longest name `resolve` / `mentions` look at; longer input is cut (a pasted essay is not a name).
const MAX_NAME_CHARS: usize = 256;
/// Longest text `mentions` scans for names.
const MAX_MENTION_TEXT_CHARS: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Resolved {
    /// `/docs/<vault path>`, `/docs/entity/<slug>` or `/docs/category/<name>`.
    pub id: String,
    /// doc | entity | category
    pub kind: String,
    pub title: String,
    pub path: Option<String>,
    pub layer: Option<String>,
    /// First ~200 chars of the body (frontmatter removed).
    pub snippet: String,
    pub score: f32,
    /// Chunks behind this id (for a category: across its documents).
    pub chunks: i64,
}

pub(crate) fn snippet(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = truncate_chars(&flat, 200);
    if s.len() < flat.len() { format!("{s}…") } else { s.to_string() }
}

fn like_pattern(q: &str) -> String {
    let mut s = String::from("%");
    for c in q.to_lowercase().chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

/// Best matches for `name` across documents, entities and categories, best first.
pub async fn resolve(pool: &PgPool, name: &str, limit: usize) -> Result<Vec<Resolved>> {
    let name = super::strip_docs_prefix(name);
    let name = name.strip_suffix(".md").unwrap_or(name).trim();
    let name = truncate_chars(name, MAX_NAME_CHARS);
    if name.is_empty() {
        return Ok(vec![]);
    }
    let limit = limit.clamp(1, 100) as i64;
    let pat = like_pattern(name);
    let mut out: Vec<Resolved> = Vec::new();

    let docs = sqlx::query(
        r"with q as (select lower($1) as lq, websearch_to_tsquery('english', $1) as tq),
        fts as (
          select c.doc_id, max(ts_rank_cd(c.tsv, q.tq)) as r
          from kb.chunks c, q where c.tsv @@ q.tq group by c.doc_id order by r desc limit 20
        ),
        cand as (
          select d.id, d.path, d.title, d.layer, left(d.body, 800) as body,
            greatest(
              extensions.similarity(lower(d.title), q.lq),
              coalesce((select max(extensions.similarity(lower(a), q.lq)) from unnest(d.aliases) a), 0),
              0.8 * extensions.similarity(lower(regexp_replace(d.path, '\.md$', '')), q.lq)
            ) as sim,
            (lower(d.title) = q.lq or lower(regexp_replace(d.path, '^.*/|\.md$', '', 'g')) = q.lq
              or lower(regexp_replace(d.path, '\.md$', '')) = q.lq
              or exists (select 1 from unnest(d.aliases) a where lower(a) = q.lq)) as exact,
            (lower(d.title) like $2 or lower(d.path) like $2
              or exists (select 1 from unnest(d.aliases) a where lower(a) like $2)) as contains,
            coalesce(least(f.r, 1.0), 0) as fr, f.doc_id is not null as fhit
          from kb.documents d cross join q left join fts f on f.doc_id = d.id
        )
        select id, path, title, layer, body,
          (sim + case when exact then 1.0 else 0 end + case when contains then 0.3 else 0 end + 0.3 * fr)::float4 as score,
          (select count(*) from kb.chunks c where c.doc_id = cand.id) as chunks
        from cand
        where sim > 0.2 or exact or contains or fhit
        order by score desc, (layer = 'template'), path
        limit $3",
    )
    .bind(name)
    .bind(&pat)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for r in docs {
        let path: String = r.get("path");
        out.push(Resolved {
            id: format!("/docs/{path}"),
            kind: "doc".into(),
            title: r.get("title"),
            layer: Some(r.get("layer")),
            snippet: snippet(r.get::<&str, _>("body")),
            path: Some(path),
            score: r.get("score"),
            chunks: r.get("chunks"),
        });
    }

    let ents = sqlx::query(
        r"with q as (select lower($1) as lq),
        cand as (
          select e.slug, e.name, e.kind, e.doc_id,
            greatest(
              extensions.similarity(lower(e.slug), q.lq),
              extensions.similarity(lower(e.name), q.lq),
              coalesce((select max(extensions.similarity(lower(a), q.lq)) from unnest(e.aliases) a), 0)
            ) as sim,
            (lower(e.slug) = q.lq or lower(e.name) = q.lq
              or exists (select 1 from unnest(e.aliases) a where lower(a) = q.lq)) as exact,
            (lower(e.slug) like $2 or lower(e.name) like $2
              or exists (select 1 from unnest(e.aliases) a where lower(a) like $2)) as contains
          from kb.entities e cross join q
        )
        select cand.slug, cand.name, cand.kind, d.path, d.layer, left(d.body, 800) as body,
          (sim + case when exact then 1.05 else 0 end + case when contains then 0.3 else 0 end)::float4 as score,
          (select count(*) from kb.chunks c where c.doc_id = cand.doc_id) as chunks
        from cand left join kb.documents d on d.id = cand.doc_id
        where sim > 0.2 or exact or contains
        order by score desc limit $3",
    )
    .bind(name)
    .bind(&pat)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for r in ents {
        let slug: String = r.get("slug");
        let kind: String = r.get("kind");
        let body: Option<&str> = r.get("body");
        out.push(Resolved {
            id: format!("/docs/entity/{slug}"),
            kind: "entity".into(),
            title: format!("{} ({kind})", r.get::<String, _>("name")),
            path: r.get("path"),
            layer: r.get("layer"),
            snippet: body.map(snippet).unwrap_or_default(),
            score: r.get("score"),
            chunks: r.get("chunks"),
        });
    }

    let cats = sqlx::query(
        r"with q as (select lower($1) as lq),
        cats as (
          select c as name, count(distinct d.id) as docs, sum((select count(*) from kb.chunks x where x.doc_id = d.id))::int8 as chunks
          from kb.documents d, unnest(d.categories) c group by c
        )
        select name, docs, chunks,
          (extensions.similarity(lower(name), q.lq) + case when lower(name) = q.lq then 1.0 else 0 end
             + case when lower(name) like $2 then 0.3 else 0 end)::float4 as score
        from cats, q
        where extensions.similarity(lower(name), q.lq) > 0.2 or lower(name) like $2
        order by score desc limit $3",
    )
    .bind(name)
    .bind(&pat)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for r in cats {
        let cname: String = r.get("name");
        let docs: i64 = r.get("docs");
        out.push(Resolved {
            id: format!("/docs/category/{cname}"),
            kind: "category".into(),
            snippet: format!("{docs} document(s) in category {cname}"),
            title: cname,
            path: None,
            layer: None,
            score: r.get("score"),
            chunks: r.get("chunks"),
        });
    }

    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out.truncate(limit as usize);
    Ok(out)
}

/// A document whose title, alias or declared entity name appears in free text (a question).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Mention {
    pub doc_id: Uuid,
    pub path: String,
    /// The title / alias / entity name that matched.
    pub name: String,
    /// pg_trgm `strict_word_similarity(name, text)`: 1.0 = the name occurs verbatim on word boundaries.
    pub score: f32,
}

/// Entity documents (references, or any document that declares an entity) named in `text`, best
/// first: highest similarity, then longest (most specific) name. Only names of 3+ characters with
/// similarity >= `min_score` count.
pub async fn mentions<'e, E>(exec: E, text: &str, min_score: f32, limit: usize) -> Result<Vec<Mention>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let text = truncate_chars(text.trim(), MAX_MENTION_TEXT_CHARS);
    if text.is_empty() || limit == 0 {
        return Ok(vec![]);
    }
    let rows = sqlx::query(
        r"with q as (select lower($1) as lq),
        ent_docs as (
          select d.id, d.title, d.aliases from kb.documents d
          where d.layer = 'reference' or exists (select 1 from kb.entities e where e.doc_id = d.id)
        ),
        names as (
          select id, title as name from ent_docs
          union all select id, a from ent_docs, unnest(aliases) a
          union all select e.doc_id, e.name from kb.entities e where e.doc_id is not null
          union all select e.doc_id, a from kb.entities e, unnest(e.aliases) a where e.doc_id is not null
        ),
        scored as (
          select n.id, n.name, extensions.strict_word_similarity(lower(n.name), q.lq) as s
          from names n, q where length(btrim(n.name)) >= 3
        )
        select distinct on (s.id) s.id, d.path, s.name, s.s::float4 as score
        from scored s join kb.documents d on d.id = s.id
        where s.s >= $2
        order by s.id, s.s desc, length(s.name) desc",
    )
    .bind(text)
    .bind(min_score)
    .fetch_all(exec)
    .await?;
    let mut out: Vec<Mention> = rows
        .into_iter()
        .map(|r| Mention { doc_id: r.get("id"), path: r.get("path"), name: r.get("name"), score: r.get("score") })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.name.chars().count().cmp(&a.name.chars().count())).then(a.path.cmp(&b.path)));
    out.truncate(limit);
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn helpers() {
        assert_eq!(super::like_pattern("50%_a"), "%50\\%\\_a%");
        assert_eq!(super::snippet("  a\n\n b  "), "a b");
        assert!(super::snippet(&"x ".repeat(300)).ends_with('…'));
    }
}
