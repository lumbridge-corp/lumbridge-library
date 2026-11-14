//! The embed backlog: chunks with no vector, or a vector from a different embed model.

use super::embed_text;
use crate::db::halfvec_literal;
use crate::models::{Embedder, Kind};
use anyhow::{Context, Result};
use sqlx::PgPool;

/// Rows fetched (and embedded) per round trip.
const BATCH: i64 = 64;

/// Embed up to `limit` pending chunks with `Kind::Passage`, tagging them with `embed_model_id`.
/// Returns how many were embedded. If the embed server fails, the error is returned and the
/// remaining rows stay pending (batches already written are kept).
pub async fn embed_pending(pool: &PgPool, embedder: &Embedder, embed_model_id: i32, limit: usize) -> Result<usize> {
    let mut done = 0usize;
    while done < limit {
        let take = BATCH.min((limit - done) as i64);
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
            "select c.id, d.title, c.heading, c.content
             from kb.chunks c join kb.documents d on d.id = c.doc_id
             where c.embedding is null or c.embed_model_id is distinct from $1
             order by c.id
             limit $2",
        )
        .bind(embed_model_id)
        .bind(take)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            break;
        }
        let texts: Vec<String> = rows.iter().map(|(_, t, h, c)| embed_text(t, h, c)).collect();
        let vecs = embedder
            .embed(&texts, Kind::Passage)
            .await
            .with_context(|| format!("embedding {} chunks ({done} done this run)", texts.len()))?;
        let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let lits: Vec<String> = vecs.iter().map(|v| halfvec_literal(v)).collect();
        sqlx::query(
            "update kb.chunks c set embedding = v.e::extensions.halfvec, embed_model_id = $3
             from unnest($1::bigint[], $2::text[]) as v(id, e)
             where c.id = v.id",
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
