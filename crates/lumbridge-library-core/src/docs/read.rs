//! Read one document with its outgoing links and backlinks.

use super::strip_docs_prefix;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LinkRef {
    /// Raw link text (`target#heading`); for a backlink, the text the source used.
    pub dst_text: String,
    /// wikilink | embed
    pub kind: String,
    /// The other end's path and title (target for outgoing links, source for backlinks); None if unresolved.
    pub path: Option<String>,
    pub title: Option<String>,
    /// Entity slug the link resolved to, if any.
    pub entity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DocFull {
    pub id: Uuid,
    pub path: String,
    pub title: String,
    pub layer: String,
    pub author: String,
    pub status: String,
    pub categories: Vec<String>,
    pub frontmatter: serde_json::Value,
    /// Body without frontmatter, or the requested 1-based inclusive line range of it. Line numbers
    /// count from the end of the frontmatter: line 1 is the first line after the closing `---`, not
    /// line 1 of the file. Search hits (`DocHit::line_start`/`line_end`) use the same numbering.
    pub body: String,
    /// Lines in the body (frontmatter excluded).
    pub total_lines: usize,
    /// The body-relative range actually returned (clamped to the body), if a range was requested.
    pub lines: Option<(usize, usize)>,
    /// Entity this document declares, if any.
    pub entity: Option<String>,
    pub parse_error: Option<String>,
    pub links: Vec<LinkRef>,
    pub backlinks: Vec<LinkRef>,
}

/// `path_or_id`: vault path (with or without `.md`), `/docs/<path>`, `/docs/entity/<slug>`, or an
/// exact title / alias. `lines` is a 1-based inclusive range of the body (frontmatter excluded).
pub async fn read(pool: &PgPool, path_or_id: &str, lines: Option<(usize, usize)>) -> Result<Option<DocFull>> {
    let s = strip_docs_prefix(path_or_id);
    let row = if let Some(slug) = s.strip_prefix("entity/") {
        let Some(e) = super::facts::find_entity(pool, slug).await? else { return Ok(None) };
        let Some(doc) = e.doc_id else { return Ok(None) };
        sqlx::query("select * from kb.documents where id = $1").bind(doc).fetch_optional(pool).await?
    } else {
        sqlx::query(
            "select * from kb.documents
             where path = $1 or path = $1 || '.md' or lower(title) = lower($1)
                or exists (select 1 from unnest(aliases) a where lower(a) = lower($1))
             order by (path = $1) desc, (path = $1 || '.md') desc, (layer = 'template'), path
             limit 1",
        )
        .bind(s)
        .fetch_optional(pool)
        .await?
    };
    let Some(r) = row else { return Ok(None) };
    let id: Uuid = r.get("id");
    let body: String = r.get("body");
    let total_lines = body.lines().count();
    let (body, lines) = match lines {
        Some((a, b)) => {
            let a = a.max(1);
            let b = b.max(a).min(total_lines.max(a));
            let slice = body.lines().skip(a - 1).take(b + 1 - a).collect::<Vec<_>>().join("\n");
            (slice, Some((a, b)))
        }
        None => (body, None),
    };

    let links = sqlx::query(
        "select l.dst_text, l.kind, d.path, d.title, e.slug
         from kb.links l left join kb.documents d on d.id = l.dst_doc left join kb.entities e on e.id = l.dst_entity
         where l.src_doc = $1 order by l.id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| LinkRef { dst_text: r.get(0), kind: r.get(1), path: r.get(2), title: r.get(3), entity: r.get(4) })
    .collect();
    let backlinks = sqlx::query(
        "select distinct on (s.path) l.dst_text, l.kind, s.path, s.title, e.slug
         from kb.links l join kb.documents s on s.id = l.src_doc left join kb.entities e on e.id = l.dst_entity
         where l.src_doc <> $1
           and (l.dst_doc = $1 or l.dst_entity in (select id from kb.entities where doc_id = $1))
         order by s.path, l.id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| LinkRef { dst_text: r.get(0), kind: r.get(1), path: r.get(2), title: r.get(3), entity: r.get(4) })
    .collect();
    let entity: Option<String> = sqlx::query_scalar("select slug from kb.entities where doc_id = $1 order by slug limit 1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(Some(DocFull {
        id,
        path: r.get("path"),
        title: r.get("title"),
        layer: r.get("layer"),
        author: r.get("author"),
        status: r.get("status"),
        categories: r.get("categories"),
        frontmatter: r.get("frontmatter"),
        body,
        total_lines,
        lines,
        entity,
        parse_error: r.get("parse_error"),
        links,
        backlinks,
    }))
}
