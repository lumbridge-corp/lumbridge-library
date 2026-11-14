//! The librarian in lumbridge-libraryd: a nightly run at `[librarian] nightly_hour` (local time, only when
//! `enabled`) plus on-demand runs as background jobs (`POST /v1/librarian/run`, admin token). One run
//! at a time. The engine is `lumbridge_library_core::librarian`; everything it does lands in the activity feed.
//!
//!   POST /v1/librarian/run   {dry_run?, tasks?, paths?}  admin → the job (or the one already running)
//!   GET  /v1/librarian/last                      → {running, last: the last run-summary row}

use crate::jobs::Job;
use crate::rest::require_admin;
use crate::state::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Local, TimeZone};
use lumbridge_library_core::librarian::{self, Ctx, RunOptions, Task};
use lumbridge_library_core::llm::Llm;
use lumbridge_library_core::{db, docs};
use lumbridge_library_core::db::ActivityRow;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tracing::{info, warn};

const JOB_KIND: &str = "librarian";
const JOB_TARGET: &str = "vault";

/// Serialises runs even if two are queued back to back.
static RUN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/librarian/run", post(run_handler)).route("/v1/librarian/last", get(last_handler))
}

/// Queue a run unless one is already queued/running (then that job is returned).
pub fn run_job(st: &AppState, opts: RunOptions) -> Job {
    if let Some(j) = st.jobs.pending(JOB_KIND, JOB_TARGET) {
        return j;
    }
    let job = st.jobs.queue(JOB_KIND, JOB_TARGET, &opts.requested_by);
    let (st, id) = (st.clone(), job.id);
    tokio::spawn(async move {
        let _g = RUN_LOCK.lock().await;
        st.jobs.start(id);
        let report = run_now(&st, opts).await;
        st.jobs.finish(id, Ok(librarian::capped(&report)));
    });
    job
}

async fn run_now(st: &AppState, opts: RunOptions) -> librarian::RunReport {
    let mid = st.model_id().await;
    let fingerprint = st.embed_model.read().await.as_ref().map(|(_, m)| m.fingerprint.clone()).unwrap_or_default();
    let root = st.cfg.vault_root();
    let ctx = Ctx {
        pool: &st.pool,
        vault_root: &root,
        embedder: &st.embedder,
        reranker: &st.reranker,
        embed_model_id: mid,
        fingerprint,
        llm: Some(Llm::from_config(&st.cfg.librarian)),
        cfg: st.cfg.librarian.clone(),
    };
    let report = librarian::run(&ctx, opts).await;
    info!(summary = %report.summary, "librarian run finished");
    let edited = report.links.as_ref().map(|l| l.edited).unwrap_or(0);
    if edited > 0 && st.cfg.vault.push && st.cfg.vault.remote.is_some() {
        push(st, &root).await;
    }
    report
}

/// Push the librarian's commits (rebase once on conflict); failures are visible in the feed.
async fn push(st: &AppState, root: &std::path::Path) {
    if let Err(e) = docs::push_vault(root).await {
        warn!("librarian push failed ({e:#}); pulling and retrying");
        let r = match docs::pull_vault(root).await {
            Ok(_) => docs::push_vault(root).await.map(|_| ()),
            Err(e) => Err(e),
        };
        let line = if r.is_ok() { "pushed librarian edits to Gitea after a rebase" } else { "could not push librarian edits to Gitea (kept locally)" };
        db::record(&st.pool, "push", "docs", "vault", r.is_ok(), Some(librarian::ACTOR), Some(line), json!({"error": r.err().map(|e| format!("{e:#}"))})).await;
    }
}

/// Time until the next `hour:00` local.
fn until_next(hour: u32) -> Duration {
    let now = Local::now();
    let hour = hour.min(23);
    let today = now.date_naive().and_hms_opt(hour, 0, 0).unwrap();
    let mut next = Local.from_local_datetime(&today).earliest().unwrap_or(now);
    if next <= now {
        let tomorrow = (now.date_naive() + chrono::Duration::days(1)).and_hms_opt(hour, 0, 0).unwrap();
        next = Local.from_local_datetime(&tomorrow).earliest().unwrap_or(now + chrono::Duration::days(1));
    }
    let secs = (next - now).num_seconds().max(60) as u64;
    Duration::from_secs(secs)
}

/// The nightly loop (no-op unless `[librarian] enabled = true`).
pub fn spawn(st: AppState) {
    let c = st.cfg.librarian.clone();
    if !c.enabled {
        return;
    }
    info!(hour = c.nightly_hour, dry_run = c.dry_run, "librarian nightly run scheduled");
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(until_next(c.nightly_hour)).await;
            run_job(&st, RunOptions { dry_run: c.dry_run, tasks: vec![], requested_by: "schedule".into(), path_prefixes: vec![] });
            // don't fire twice within the same hour
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

#[derive(Deserialize, Default)]
struct RunBody {
    dry_run: Option<bool>,
    #[serde(default)]
    tasks: Vec<Task>,
    /// Only these vault path prefixes (e.g. `["References/NVIDIA B200.md"]`).
    #[serde(default)]
    paths: Vec<String>,
}

async fn run_handler(State(st): State<AppState>, h: HeaderMap, body: Option<Json<RunBody>>) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&st, &h, "running the librarian")?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let opts = RunOptions {
        dry_run: b.dry_run.unwrap_or(st.cfg.librarian.dry_run),
        tasks: b.tasks,
        requested_by: crate::rest::owner_actor(&h),
        path_prefixes: b.paths,
    };
    Ok(Json(serde_json::to_value(run_job(&st, opts)).unwrap_or(Value::Null)))
}

/// What `GET /v1/librarian/last` (and a read-only MCP tool) returns.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LibrarianStatus {
    /// The queued/running job, if any.
    pub running: Option<Job>,
    /// The last run-summary row from the feed (`detail` holds the capped report).
    pub last: Option<ActivityRow>,
    pub enabled: bool,
    pub dry_run: bool,
    pub nightly_hour: u32,
    pub model: String,
    pub max_edits_per_run: usize,
}

pub async fn status(st: &AppState) -> anyhow::Result<LibrarianStatus> {
    let c = &st.cfg.librarian;
    Ok(LibrarianStatus {
        running: st.jobs.pending(JOB_KIND, JOB_TARGET),
        last: librarian::last(&st.pool).await?,
        enabled: c.enabled,
        dry_run: c.dry_run,
        nightly_hour: c.nightly_hour,
        model: c.model.clone(),
        max_edits_per_run: c.max_edits_per_run,
    })
}

async fn last_handler(State(st): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    let s = status(&st).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(serde_json::to_value(s).unwrap_or(Value::Null)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn next_run_is_within_a_day() {
        for h in [0, 3, 23, 99] {
            let d = super::until_next(h);
            assert!(d.as_secs() >= 60 && d.as_secs() <= 25 * 3600, "{h}: {d:?}");
        }
    }
}
