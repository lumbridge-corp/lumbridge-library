//! Activity feed: `GET /v1/activity` (and the `library_activity` MCP tool) over `lib.ops_log`.
//! Every write path records who did what with a one-line summary; pulled vault commits are recorded
//! with their files added / modified / deleted (see ops::record_pulled_commits).

use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lumbridge_library_core::{db, ledger};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub struct ActivityQ {
    limit: Option<i64>,
    shelf: Option<String>,
    op: Option<String>,
    before: Option<DateTime<Utc>>,
}

pub async fn list(State(st): State<AppState>, Query(q): Query<ActivityQ>) -> Result<Json<Value>, (StatusCode, String)> {
    let rows = db::activity(&st.pool, q.limit.unwrap_or(50), q.shelf.as_deref(), q.op.as_deref(), q.before)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(serde_json::to_value(rows).unwrap_or(Value::Null)))
}

pub async fn verdict(st: &AppState, v: &ledger::VerdictOutcome, by: &str, correction: Option<&str>) {
    if !v.recorded {
        return;
    }
    let word = match v.verdict {
        1 => "right",
        0 => "partly right",
        _ => "wrong",
    };
    let summary = format!("answer marked {word}{}", if correction.is_some() { " (with correction)" } else { "" });
    db::record(&st.pool, "verdict", "library", &v.qa_id.to_string(), true, Some(by), Some(&summary), json!({"verdict": v.verdict})).await;
}
