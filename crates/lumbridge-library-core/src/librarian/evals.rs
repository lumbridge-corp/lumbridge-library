//! Retrieval evals (`ledger::run_retrieval_evals`, the same grading as `POST /v1/evals/run`),
//! compared with the previous librarian run on the same embed model. A drop is an activity row with
//! `ok = false`.

use super::{ACTOR, Ctx, record};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Score drops smaller than this are noise.
const TOLERANCE: f32 = 0.005;

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EvalsReport {
    pub model: String,
    pub graded: usize,
    pub score: f32,
    pub previous: Option<f32>,
    /// Rank-aware: answer in block [1], and mean reciprocal rank; plus the previous run's MRR.
    #[serde(default)]
    pub hit_at_1: f32,
    #[serde(default)]
    pub mrr: f32,
    #[serde(default)]
    pub previous_mrr: Option<f32>,
    pub regression: bool,
    /// Items that scored 0 this run (question only).
    pub failed: Vec<String>,
}

pub async fn run(ctx: &Ctx<'_>) -> Result<EvalsReport> {
    if ctx.embed_model_id == 0 {
        anyhow::bail!("no embed model registered; evals skipped");
    }
    let r = crate::ledger::run_retrieval_evals(ctx.pool, ctx.embedder, ctx.reranker, ctx.embed_model_id, &ctx.fingerprint, None).await?;
    let prev: Option<(Option<f64>, Option<f64>)> = sqlx::query_as(
        "select (detail->>'score')::float8, (detail->>'mrr')::float8 from lib.ops_log
         where op = 'librarian.evals' and actor = $1 and detail->>'model' = $2 and (detail->>'graded')::int > 0
         order by at desc, id desc limit 1",
    )
    .bind(ACTOR)
    .bind(&r.model)
    .fetch_optional(ctx.pool)
    .await?;
    let previous = prev.and_then(|p| p.0).map(|x| x as f32);
    let previous_mrr = prev.and_then(|p| p.1).map(|x| x as f32);
    // Ranking can get worse while every answer is still somewhere in the context: watch MRR too.
    let regression = r.graded > 0
        && (previous.is_some_and(|p| r.score < p - TOLERANCE) || previous_mrr.is_some_and(|p| r.mrr < p - 0.02));
    let failed = r
        .items
        .iter()
        .filter(|i| i["score"].as_f64() == Some(0.0))
        .filter_map(|i| i["question"].as_str().map(str::to_string))
        .collect();
    let rep = EvalsReport {
        model: r.model.clone(),
        graded: r.graded,
        score: r.score,
        previous,
        hit_at_1: r.hit_at_1,
        mrr: r.mrr,
        previous_mrr,
        regression,
        failed,
    };
    let was = |p: Option<f32>| p.map(|p| format!(" (was {p:.3})")).unwrap_or_default();
    let summary = if r.graded == 0 {
        "retrieval evals: no active eval items".to_string()
    } else {
        format!(
            "retrieval evals{}: found {:.3}{}, first {:.3}, MRR {:.3}{} over {} items",
            if regression { " REGRESSED" } else { "" },
            r.score,
            was(previous),
            r.hit_at_1,
            r.mrr,
            was(previous_mrr),
            r.graded
        )
    };
    record(
        ctx,
        "librarian.evals",
        "library",
        "evals",
        !regression,
        &summary,
        json!({"model": rep.model, "graded": rep.graded, "score": rep.score, "previous": rep.previous,
               "hit_at_1": rep.hit_at_1, "mrr": rep.mrr, "previous_mrr": rep.previous_mrr, "sets": r.sets, "failed": rep.failed}),
    )
    .await;
    Ok(rep)
}
