use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ShelfCounts {
    pub documents: i64,
    pub chunks: i64,
    pub unembedded_chunks: i64,
    pub entities: i64,
    pub facts: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WarehouseCounts {
    pub repos_mirror: i64,
    pub repos_own: i64,
    pub components: i64,
    pub designs: i64,
    pub design_shots: i64,
    pub design_shots_captioned: i64,
    pub files: i64,
    pub symbols: i64,
    pub chunks: i64,
    pub unembedded_chunks: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LedgerCounts {
    pub qa: i64,
    pub qa_with_verdict: i64,
    pub evals: i64,
    pub datasets: i64,
}

async fn n(pool: &PgPool, sql: &str) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await?)
}

pub async fn docs(pool: &PgPool) -> Result<ShelfCounts> {
    Ok(ShelfCounts {
        documents: n(pool, "select count(*) from kb.documents").await?,
        chunks: n(pool, "select count(*) from kb.chunks").await?,
        unembedded_chunks: n(pool, "select count(*) from kb.chunks where embedding is null").await?,
        entities: n(pool, "select count(*) from kb.entities").await?,
        facts: n(pool, "select count(*) from kb.facts where superseded_by is null").await?,
    })
}

pub async fn warehouse(pool: &PgPool) -> Result<WarehouseCounts> {
    Ok(WarehouseCounts {
        repos_mirror: n(pool, "select count(*) from wh.repos where tier = 'mirror'").await?,
        repos_own: n(pool, "select count(*) from wh.repos where tier = 'own'").await?,
        components: n(pool, "select count(*) from wh.components where status <> 'retired'").await?,
        designs: n(pool, "select count(*) from wh.designs where status <> 'retired'").await?,
        design_shots: n(pool, "select count(*) from wh.design_shots").await?,
        design_shots_captioned: n(pool, "select count(*) from wh.design_shots where caption is not null").await?,
        files: n(pool, "select count(*) from wh.files").await?,
        symbols: n(pool, "select count(*) from wh.symbols").await?,
        chunks: n(pool, "select count(*) from wh.chunks").await?,
        unembedded_chunks: n(pool, "select count(*) from wh.chunks where embedding is null").await?,
    })
}

pub async fn ledger(pool: &PgPool) -> Result<LedgerCounts> {
    Ok(LedgerCounts {
        qa: n(pool, "select count(*) from lib.qa").await?,
        qa_with_verdict: n(pool, "select count(*) from lib.qa where verdict is not null").await?,
        evals: n(pool, "select count(*) from lib.evals where active").await?,
        datasets: n(pool, "select count(*) from lib.datasets").await?,
    })
}
