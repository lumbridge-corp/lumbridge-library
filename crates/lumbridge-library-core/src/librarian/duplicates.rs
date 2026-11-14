//! Near-duplicate clippings: same source URL, near-identical titles (pg_trgm), or near-identical
//! opening chunk (cosine on the first chunk's embedding, same embed model only). Reported, never
//! merged: which copy to keep is the owner's call.

use super::{Ctx, record_finding};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Pairs kept in the report.
const MAX_PAIRS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DuplicatePair {
    pub a: String,
    pub b: String,
    pub title_similarity: f32,
    /// Cosine similarity of the first chunks (None when either is unembedded).
    pub cosine: Option<f32>,
    pub same_source: bool,
    /// same-source | title | content
    pub why: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DuplicatesReport {
    pub clippings: i64,
    pub pairs: Vec<DuplicatePair>,
    /// Pairs recorded in the feed this run (the rest were reported in the last week).
    pub new_findings: usize,
}

fn tokens(t: &str) -> std::collections::HashSet<String> {
    t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

/// Templated titles that differ in a model / config / date token on *both* sides
/// (`B200 vs H100` / `B200 vs H200`, `Roundup 1-27-2022` / `Roundup 2-1-2022`) are different posts.
pub fn swapped_numbers(a: &str, b: &str) -> bool {
    let (ta, tb) = (tokens(a), tokens(b));
    let digit = |w: &&String| w.chars().any(|c| c.is_ascii_digit());
    ta.difference(&tb).any(|w| digit(&w)) && tb.difference(&ta).any(|w| digit(&w))
}

/// Is this pair a duplicate, and why? Thresholds are deliberately strict.
pub fn classify(a: &str, b: &str, title_sim: f32, cosine: Option<f32>, same_source: bool) -> Option<&'static str> {
    if same_source {
        return Some("same-source");
    }
    if swapped_numbers(a, b) {
        return None;
    }
    let cos = cosine.unwrap_or(0.0);
    if title_sim >= 0.95 || (title_sim >= 0.85 && cos >= 0.9) {
        return Some("title");
    }
    if cos >= 0.97 {
        return Some("content");
    }
    None
}

pub async fn run(ctx: &Ctx<'_>, scope: Option<&[String]>) -> Result<DuplicatesReport> {
    let mut rep = DuplicatesReport {
        clippings: sqlx::query_scalar("select count(*) from kb.documents where layer = 'clipping'").fetch_one(ctx.pool).await?,
        ..Default::default()
    };
    let rows = sqlx::query_as::<_, (String, String, f32, Option<f64>, bool)>(
        r"with c as (
            select d.id, d.path, lower(d.title) as t,
                   nullif(rtrim(coalesce(d.frontmatter->>'source', d.frontmatter->>'url', ''), '/'), '') as src,
                   ch.embedding
            from kb.documents d
            left join kb.chunks ch on ch.doc_id = d.id and ch.ord = 0 and ch.embed_model_id = $1
            where d.layer = 'clipping'
          )
          select a.path, b.path, extensions.similarity(a.t, b.t)::float4 as ts,
                 case when a.embedding is not null and b.embedding is not null
                      then 1 - (a.embedding operator(extensions.<=>) b.embedding) end::float8 as cos,
                 coalesce(a.src = b.src, false) as same_src
          from c a join c b on a.path < b.path
          where coalesce(a.src = b.src, false)
             or extensions.similarity(a.t, b.t) >= 0.6
             or (a.embedding is not null and b.embedding is not null
                 and (a.embedding operator(extensions.<=>) b.embedding) <= 0.03)
          order by same_src desc, ts desc, a.path",
    )
    .bind(ctx.embed_model_id)
    .fetch_all(ctx.pool)
    .await?;
    for (a, b, ts, cos, same) in rows {
        let cos = cos.map(|c| c as f32);
        if !super::in_scope(scope, &a) && !super::in_scope(scope, &b) {
            continue;
        }
        let Some(why) = classify(crate::docs::stem(&a), crate::docs::stem(&b), ts, cos, same) else { continue };
        if rep.pairs.len() >= MAX_PAIRS {
            break;
        }
        rep.pairs.push(DuplicatePair { a, b, title_similarity: ts, cosine: cos, same_source: same, why: why.into() });
    }
    for p in &rep.pairs {
        let summary = format!("possible duplicate clippings ({}): {} ≈ {}", p.why, crate::docs::stem(&p.a), crate::docs::stem(&p.b));
        let target = format!("{} | {}", p.a, p.b);
        if record_finding(ctx, "librarian.duplicate", &target, &summary, json!(p)).await {
            rep.new_findings += 1;
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn thresholds() {
        assert_eq!(classify("a", "b", 0.1, None, true), Some("same-source"));
        assert_eq!(classify("x y", "x y z", 0.96, None, false), Some("title"));
        assert_eq!(classify("x y", "x y z", 0.9, Some(0.95), false), Some("title"));
        assert_eq!(classify("x y", "x y z", 0.9, Some(0.5), false), None);
        assert_eq!(classify("x y", "q r", 0.2, Some(0.98), false), Some("content"));
        assert_eq!(classify("x y", "q r", 0.2, Some(0.9), false), None);
        assert_eq!(classify("InferenceX - B200 vs H100", "InferenceX - B200 vs H200", 0.8, Some(0.98), false), None);
        assert_eq!(classify("Semiconductor Roundup - 1-27-2022", "Semiconductor Roundup – 2-1-2022", 0.9, Some(0.6), false), None);
        assert_eq!(classify("H200 NVL Live Inventory (h200-nvl)", "H200 NVL Live Inventory", 1.0, Some(0.96), false), Some("title"));
    }
}
