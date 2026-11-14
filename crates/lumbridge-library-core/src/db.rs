use crate::models::ModelIdentity;
use anyhow::{Context, Result};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::fmt::Write;
use std::time::Duration;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn connect(url: &str, max: u32) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(max)
        .acquire_timeout(Duration::from_secs(10))
        .connect(url)
        .await
        .context("connecting to Postgres")
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    accept_earlier_checksums(pool).await.context("updating migration checksums")?;
    MIGRATOR.run(pool).await.context("running migrations")?;
    Ok(())
}

/// Checksums (SHA-384, as sqlx stores them) of migrations whose text changed after they were
/// first released: comments and the column defaults of `wh.repos.added_by` and
/// `wh.designs.license_spdx` (a database keeps the defaults it was created with). Applied
/// migrations are never re-run; a database that recorded one of these checksums gets the current
/// one, so sqlx does not refuse to start. Only an exact (version, old checksum) pair is changed.
const EARLIER_CHECKSUMS: &[(i64, &str)] = &[
    (1, "43463a5f71b4e30c81234ae3925101566258846e54e5761ddb14d5d1236fa3bad048ce90f458ec8882cf6b98a4b8c9f2"),
    (2, "8cbb54931cf454e82c646c687ee782cb52a0172c246ba5d985724c1e69325451b216bb41362e6fd296db3263ecfe1005"),
    (3, "fcd8378447e3500127873c499efd2c9d56c67ca5ddeaee5719ea82f1865104c6d08fd46435c897698e94dfd98bd1eadf"),
    (4, "a87e53af0bdb4786a9fd50fe2cf646af837d38716eedca373d8c6ede5b9a5e6c9252bdb7ad7a0312d5cb98842ff5fcb7"),
    (6, "7f90c1e346a9780270d69016f06f9850ac7edb06eab7b36214d83381dbe5887d6a70f00f3907a46f71e8fce99d2510c6"),
    (7, "2ad1c89fa71dfb1ffeb0e7cb05ff429c5ace71890eb057d7faf91ed64f167a9404fc326b508c1b5e778f2e07d84131f7"),
    (8, "6fff6fcc2262b3929fdc965624e671e065f2a466a49c0ee1f83cb04e328667ac532aa4939973c14064b032aff62a21de"),
];

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).filter_map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

async fn accept_earlier_checksums(pool: &PgPool) -> Result<()> {
    // sqlx keeps its table in the first schema on the search path; resolve it the same way.
    let exists: bool = sqlx::query_scalar("select to_regclass('_sqlx_migrations') is not null").fetch_one(pool).await?;
    if !exists {
        return Ok(());
    }
    for (version, old) in EARLIER_CHECKSUMS {
        let Some(m) = MIGRATOR.iter().find(|m| m.version == *version) else { continue };
        let old = unhex(old);
        if old.as_slice() == m.checksum.as_ref() {
            continue;
        }
        let r = sqlx::query("update _sqlx_migrations set checksum = $1 where version = $2 and checksum = $3")
            .bind(m.checksum.as_ref())
            .bind(version)
            .bind(&old)
            .execute(pool)
            .await?;
        if r.rows_affected() > 0 {
            tracing::info!(version, "migration checksum updated (text edited after release)");
        }
    }
    Ok(())
}

/// Upsert the serving embed model and return its id; vectors are tagged with it.
pub async fn register_embed_model(pool: &PgPool, m: &ModelIdentity) -> Result<i32> {
    let id: i32 = sqlx::query_scalar(
        "insert into lib.embed_models (name, root, dims, fingerprint) values ($1, $2, $3, $4)
         on conflict (fingerprint) do update set last_seen = now()
         returning id",
    )
    .bind(&m.name)
    .bind(&m.root)
    .bind(m.dims as i32)
    .bind(&m.fingerprint)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// pgvector text form, e.g. `[0.1,0.2]`, for binding as `$n::extensions.halfvec`.
/// Kept dependency-free (no pgvector crate) so sqlx upgrades never wait on it.
pub fn halfvec_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 9 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{x}");
    }
    s.push(']');
    s
}

pub async fn log_op(pool: &PgPool, op: &str, shelf: &str, target: &str, ok: bool, detail: serde_json::Value) {
    record(pool, op, shelf, target, ok, None, None, detail).await;
}

/// One activity-feed row: what happened (`op`), to what (`target`), by whom (`actor`: the owner,
/// an agent's MCP client name, an app, cli, schedule, or a git author), and a one-line human `summary`.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    pool: &PgPool,
    op: &str,
    shelf: &str,
    target: &str,
    ok: bool,
    actor: Option<&str>,
    summary: Option<&str>,
    detail: serde_json::Value,
) {
    let _ = sqlx::query(
        "insert into lib.ops_log (op, shelf, target, ok, actor, summary, detail) values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(op)
    .bind(shelf)
    .bind(target)
    .bind(ok)
    .bind(actor)
    .bind(summary)
    .bind(detail)
    .execute(pool)
    .await
    .map_err(|e| tracing::warn!("ops_log insert failed: {e}"));
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema, sqlx::FromRow)]
pub struct ActivityRow {
    pub id: i64,
    #[schemars(with = "String")]
    pub at: chrono::DateTime<chrono::Utc>,
    pub op: String,
    pub shelf: Option<String>,
    pub target: Option<String>,
    pub ok: bool,
    pub actor: Option<String>,
    pub summary: Option<String>,
    pub detail: serde_json::Value,
}

/// The feed, newest first. Hides the noisy `start` rows unless asked for by `op`.
pub async fn activity(
    pool: &PgPool,
    limit: i64,
    shelf: Option<&str>,
    op: Option<&str>,
    before: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<ActivityRow>> {
    Ok(sqlx::query_as::<_, ActivityRow>(
        "select id, at, op, shelf, target, ok, actor, summary, detail from lib.ops_log
         where ($1::text is null or shelf = $1)
           and (($2::text is null and op not in ('start')) or op = $2)
           and ($3::timestamptz is null or at < $3)
         order by at desc, id desc limit $4",
    )
    .bind(shelf)
    .bind(op)
    .bind(before)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn halfvec_text() {
        assert_eq!(super::halfvec_literal(&[0.5, -1.0, 2.25]), "[0.5,-1,2.25]");
    }

    #[test]
    fn migration_checksums_are_sha384_of_the_file() {
        use sha2::{Digest, Sha384};
        // An unchanged migration: our checksum of the file text is the one sqlx stores.
        let text = include_str!("../../../migrations/0005_activity.sql");
        let m = super::MIGRATOR.iter().find(|m| m.version == 5).unwrap();
        assert_eq!(Sha384::digest(text.as_bytes()).as_slice(), m.checksum.as_ref());
        // Every earlier checksum names a migration that exists, is well formed, and differs from today's.
        for (v, old) in super::EARLIER_CHECKSUMS {
            let m = super::MIGRATOR.iter().find(|m| m.version == *v).unwrap();
            let old = super::unhex(old);
            assert_eq!(old.len(), 48, "version {v}");
            assert_ne!(old.as_slice(), m.checksum.as_ref(), "version {v} did not change; drop its entry");
        }
    }
}
