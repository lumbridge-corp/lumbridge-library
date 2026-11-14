//! Warehouse glue between the engine (`lumbridge_library_core::warehouse`) and lumbridge-libraryd: background mirror /
//! index jobs (one at a time — indexing is the memory-heavy part), component publishing to Gitea,
//! and the REST routes the `lumbridge-warehouse` CLI uses.

use crate::rest::client;
use crate::state::AppState;
use crate::{gitops, jobs::Job};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use lumbridge_library_core::db;
use lumbridge_library_core::warehouse::{self as w, IndexOpts};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::LazyLock;
use tokio::sync::Semaphore;
use tracing::{info, warn};

/// One indexing job at a time: the shipped unit caps lumbridge-libraryd at MemoryHigh=512M /
/// MemoryMax=1G, next to the models.
pub(crate) static INDEX_SLOT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(1));

/// For interactive callers (component create, remove): wait at most 60 s for the index slot.
async fn acquire_slot() -> anyhow::Result<tokio::sync::SemaphorePermit<'static>> {
    tokio::time::timeout(std::time::Duration::from_secs(60), INDEX_SLOT.acquire())
        .await
        .map_err(|_| anyhow::anyhow!("the indexer is busy (a mirror or sync job is running); try again in a few minutes"))?
        .map_err(|e| anyhow::anyhow!("{e}"))
}

pub async fn embed_pending(st: &AppState, mid: i32, limit: usize) -> anyhow::Result<usize> {
    w::embed_pending(&st.pool, &st.embedder, mid, limit).await
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AddTier {
    Mirror,
    Own,
}

/// Queue "clone + index" for a repo URL (or a local path for `own`). Returns at once with a job.
pub fn add_repo_job(st: &AppState, url: String, tier: AddTier, name: Option<String>, tags: Vec<String>, by: String) -> Job {
    let kind = match tier {
        AddTier::Mirror => "mirror",
        AddTier::Own => "own",
    };
    if let Some(j) = st.jobs.pending(kind, &url) {
        return j;
    }
    let job = st.jobs.queue(kind, &url, &by);
    let (st, id) = (st.clone(), job.id);
    tokio::spawn(async move {
        let _slot = INDEX_SLOT.acquire().await;
        st.jobs.start(id);
        let r: anyhow::Result<Value> = async {
            let root = &st.cfg.library_root;
            let repo = match tier {
                AddTier::Mirror => w::add_mirror(&st.pool, root, &url, &tags, &by).await?,
                AddTier::Own => w::add_own(&st.pool, root, &url, name.as_deref(), &tags).await?,
            };
            let rep = w::sync_repo(&st.pool, root, &repo.slug, &IndexOpts { fetch: false, ..Default::default() }).await?;
            Ok(json!({ "slug": repo.slug, "license": repo.license_spdx, "copyleft": repo.copyleft, "report": rep }))
        }
        .await;
        let summary = match &r {
            Ok(v) => format!(
                "added {} ({}, {} files, {} chunks)",
                v["slug"].as_str().unwrap_or(&url),
                v["license"].as_str().unwrap_or("license unknown"),
                v["report"]["files_added"],
                v["report"]["chunks_new"]
            ),
            Err(e) => format!("failed to add {url}: {e:#}"),
        };
        db::record(&st.pool, kind, "warehouse", &url, r.is_ok(), Some(&by), Some(&summary), r.as_ref().map(|v| v.clone()).unwrap_or_else(|e| json!({"error": format!("{e:#}")}))).await;
        match &r {
            Ok(v) => info!(slug = %v["slug"], "warehouse: added"),
            Err(e) => warn!("warehouse add {url}: {e:#}"),
        }
        st.jobs.finish(id, r);
    });
    job
}

/// Queue a fetch + incremental re-index of one repo.
pub fn sync_job(st: &AppState, slug: String, force: bool, by: String) -> Job {
    if let Some(j) = st.jobs.pending("sync", &slug) {
        return j;
    }
    let job = st.jobs.queue("sync", &slug, &by);
    let (st, id) = (st.clone(), job.id);
    tokio::spawn(async move {
        let _slot = INDEX_SLOT.acquire().await;
        st.jobs.start(id);
        let r = w::sync_repo(&st.pool, &st.cfg.library_root, &slug, &IndexOpts { force, ..Default::default() })
            .await
            .map(|rep| serde_json::to_value(rep).unwrap_or_default());
        let summary = match &r {
            Ok(v) if v["reindexed"].as_bool() == Some(true) => format!(
                "synced {slug}: +{} ~{} -{} files",
                v["files_added"], v["files_changed"], v["files_removed"]
            ),
            Ok(_) => format!("synced {slug}: no upstream changes"),
            Err(e) => format!("sync {slug} failed: {e:#}"),
        };
        db::record(&st.pool, "sync", "warehouse", &slug, r.is_ok(), Some(&by), Some(&summary), r.as_ref().map(|v| v.clone()).unwrap_or_else(|e| json!({"error": format!("{e:#}")}))).await;
        st.jobs.finish(id, r);
    });
    job
}

/// The nightly loop: fetch every repo and re-index what moved.
pub async fn sync_all_job(st: &AppState, by: &str) {
    if st.jobs.pending("sync-all", "*").is_some() {
        return;
    }
    let job = st.jobs.queue("sync-all", "*", by);
    let _slot = INDEX_SLOT.acquire().await;
    st.jobs.start(job.id);
    let r = w::sync_all(&st.pool, &st.cfg.library_root, &IndexOpts::default()).await.map(|outs| {
        let failed: Vec<_> = outs.iter().filter(|o| o.error.is_some()).map(|o| json!({"slug": o.slug, "error": o.error})).collect();
        let moved = outs.iter().filter(|o| o.report.as_ref().is_some_and(|r| r.reindexed)).count();
        json!({"repos": outs.len(), "reindexed": moved, "failed": failed})
    });
    let summary = match &r {
        Ok(v) => format!("nightly sync: {} repos, {} re-indexed, {} failed", v["repos"], v["reindexed"], v["failed"].as_array().map_or(0, |a| a.len())),
        Err(e) => format!("nightly sync failed: {e:#}"),
    };
    db::record(&st.pool, "sync-all", "warehouse", "*", r.is_ok(), Some(by), Some(&summary), r.as_ref().map(|v| v.clone()).unwrap_or_default()).await;
    st.jobs.finish(job.id, r);
}

/// Create a draft component, index it, and (if Gitea is configured) publish it as a private repo
/// `warehouse/<slug>`. Publishing failures don't fail the create; they're reported.
pub async fn create_component(st: &AppState, c: w::NewComponent) -> anyhow::Result<Value> {
    let actor = c.created_by.clone();
    let row = {
        let _slot = acquire_slot().await?;
        w::create_component(&st.pool, &st.cfg.library_root, c).await?
    };
    let mut out = json!({ "component": row });
    if let (Some(g), Some(dir)) = (&st.cfg.gitea, row.local_path.as_deref()) {
        match gitops::publish_component(g, &row.slug, &row.summary, std::path::Path::new(dir)).await {
            Ok(full) => {
                let _ = sqlx::query("update wh.repos set gitea_full_name = $2 where slug = $1")
                    .bind(format!("components/{}", row.slug))
                    .bind(&full)
                    .execute(&st.pool)
                    .await;
                out["gitea"] = json!(full);
            }
            Err(e) => out["gitea_error"] = json!(format!("{e:#}")),
        }
    }
    let summary = format!("drafted component {} ({}, {})", row.slug, row.origin, row.license_spdx);
    db::record(&st.pool, "component", "warehouse", &row.slug, true, Some(&actor), Some(&summary), json!({"origin": row.origin, "gitea": out.get("gitea")})).await;
    Ok(out)
}

/// Keep `component.toml`'s `status` in step with the database, commit, and push to Gitea, so the
/// component repo itself says whether it's approved. Returns what happened (for the activity row).
async fn sync_component_status(st: &AppState, row: &w::ComponentRow) -> Value {
    let Some(dir) = row.local_path.as_deref().map(std::path::Path::new) else { return json!("no local repo") };
    let toml_path = dir.join("component.toml");
    let Ok(text) = std::fs::read_to_string(&toml_path) else { return json!("no component.toml") };
    let mut changed = false;
    let new: String = text
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("status") && l.contains('=') {
                changed = true;
                format!("status = \"{}\"", row.status)
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    if !changed || new == text {
        return json!("unchanged");
    }
    if let Err(e) = std::fs::write(&toml_path, &new) {
        return json!(format!("write failed: {e}"));
    }
    let msg = format!("status: {}", row.status);
    let commit = async {
        let id = lumbridge_library_core::identity::get();
        let (name, email) = (format!("user.name={}", id.git_name), format!("user.email={}", id.git_email));
        gitops::git(dir, &["-c", &name, "-c", &email, "commit", "-q", "-am", &msg]).await?;
        if st.cfg.gitea.is_some() && gitops::git(dir, &["remote", "get-url", "gitea"]).await.is_ok() {
            gitops::git(dir, &["push", "-q", "gitea", "HEAD:main"]).await?;
            return Ok::<_, anyhow::Error>("committed and pushed to Gitea");
        }
        Ok("committed (no Gitea remote)")
    }
    .await;
    // Re-index so the warehouse copy shows the new status.
    let _ = sync_job(st, format!("components/{}", row.slug), true, lumbridge_library_core::legacy::wire::ACTOR.into());
    match commit {
        Ok(s) => json!(s),
        Err(e) => json!(format!("commit/push failed: {e:#}")),
    }
}

// ───────────────────────────── REST ─────────────────────────────

type Res = Result<Json<Value>, (StatusCode, String)>;
fn ok<T: serde::Serialize>(v: T) -> Res {
    Ok(Json(serde_json::to_value(v).unwrap_or(Value::Null)))
}
fn e400(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, format!("{e:#}"))
}
fn e500(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/warehouse/repos", get(repos).post(add_repo))
        .route("/v1/warehouse/sync", post(sync))
        .route("/v1/warehouse/resolve", get(resolve))
        .route("/v1/warehouse/query", post(query))
        .route("/v1/warehouse/search", post(search))
        .route("/v1/warehouse/grep", post(grep))
        .route("/v1/warehouse/symbols", post(symbols))
        .route("/v1/warehouse/read", get(read))
        .route("/v1/warehouse/card", get(card))
        .route("/v1/warehouse/tree", get(tree))
        .route("/v1/warehouse/components", get(components).post(component_create))
        .route("/v1/warehouse/components/{slug}/{action}", post(component_action))
        .route("/v1/warehouse/remove", post(remove))
}

#[derive(Deserialize)]
struct TierQ {
    tier: Option<String>,
}
async fn repos(State(st): State<AppState>, Query(q): Query<TierQ>) -> Res {
    ok(w::list_repos(&st.pool, q.tier.as_deref()).await.map_err(e500)?)
}

#[derive(Deserialize)]
struct AddBody {
    url: String,
    #[serde(default = "mirror")]
    tier: AddTier,
    name: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}
fn mirror() -> AddTier {
    AddTier::Mirror
}
async fn add_repo(State(st): State<AppState>, h: HeaderMap, Json(b): Json<AddBody>) -> Res {
    ok(add_repo_job(&st, b.url, b.tier, b.name, b.tags, client(&h)))
}

#[derive(Deserialize)]
struct SyncBody {
    slug: Option<String>,
    #[serde(default)]
    force: bool,
}
async fn sync(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SyncBody>) -> Res {
    match b.slug {
        Some(s) => ok(sync_job(&st, s, b.force, client(&h))),
        None => {
            let st2 = st.clone();
            let by = client(&h);
            tokio::spawn(async move { sync_all_job(&st2, &by).await });
            ok(json!({"queued": "sync-all"}))
        }
    }
}

#[derive(Deserialize)]
struct NameQ {
    name: String,
    limit: Option<usize>,
}
async fn resolve(State(st): State<AppState>, Query(q): Query<NameQ>) -> Res {
    ok(w::resolve(&st.pool, &q.name, q.limit.unwrap_or(8).clamp(1, 30)).await.map_err(e500)?)
}

#[derive(Deserialize)]
struct QueryBody {
    question: String,
    scope: Option<String>,
    max_tokens: Option<usize>,
}
async fn query(State(st): State<AppState>, h: HeaderMap, Json(b): Json<QueryBody>) -> Res {
    let mid = st.model_id().await;
    let ans = w::query(&st.pool, &st.embedder, &st.reranker, mid, &b.question, b.scope.as_deref(), b.max_tokens)
        .await
        .map_err(e500)?;
    let qa_id = log_query(&st, &client(&h), &b.question, b.scope.clone(), &ans).await;
    let mut v = serde_json::to_value(&ans).unwrap_or_default();
    v["qa_id"] = json!(qa_id);
    Ok(Json(v))
}

pub async fn log_query(st: &AppState, by: &str, q: &str, scope: Option<String>, ans: &w::CodeAnswer) -> Option<uuid::Uuid> {
    lumbridge_library_core::ledger::log_qa(
        &st.pool,
        lumbridge_library_core::ledger::QaLog {
            shelf: "warehouse",
            asked_by: by.into(),
            question: q.into(),
            resolved_id: ans.resolved.clone().or(scope),
            cited: serde_json::to_value(&ans.citations).unwrap_or_default(),
            ..Default::default()
        },
    )
    .await
}

async fn search(State(st): State<AppState>, Json(p): Json<w::CodeSearchParams>) -> Res {
    let mid = st.model_id().await;
    ok(w::search(&st.pool, &st.embedder, &st.reranker, mid, &p).await.map_err(e500)?)
}
async fn grep(State(st): State<AppState>, Json(p): Json<w::GrepParams>) -> Res {
    ok(w::grep(&st.pool, &st.cfg.library_root, &p).await.map_err(e400)?)
}
async fn symbols(State(st): State<AppState>, Json(q): Json<w::SymbolQuery>) -> Res {
    ok(w::symbols(&st.pool, &q).await.map_err(e400)?)
}

#[derive(Deserialize)]
struct ReadQ {
    repo: String,
    path: Option<String>,
    symbol: Option<String>,
    start: Option<usize>,
    end: Option<usize>,
}
async fn read(State(st): State<AppState>, Query(q): Query<ReadQ>) -> Res {
    let root = &st.cfg.library_root;
    if let Some(sym) = q.symbol {
        return ok(w::read_symbol(&st.pool, root, &q.repo, &sym).await.map_err(e400)?);
    }
    let path = q.path.ok_or((StatusCode::BAD_REQUEST, "path or symbol required".into()))?;
    let lines = match (q.start, q.end) {
        (None, None) => None,
        (s, e) => Some((s.unwrap_or(1), e.unwrap_or(usize::MAX))),
    };
    ok(w::read(&st.pool, root, &q.repo, &path, lines).await.map_err(e400)?)
}

#[derive(Deserialize)]
struct TreeQ {
    repo: String,
    path: Option<String>,
    depth: Option<usize>,
}
async fn tree(State(st): State<AppState>, Query(q): Query<TreeQ>) -> Res {
    ok(w::tree(&st.pool, &st.cfg.library_root, &q.repo, q.path.as_deref(), q.depth).await.map_err(e400)?)
}

/// After migration 0007: fill `wh.chunk_terms` and refresh package names + v2 cards, once, in the
/// background (only when chunks are missing terms). Nothing is re-parsed or re-embedded.
pub fn backfill_if_needed(st: &AppState) {
    let st = st.clone();
    tokio::spawn(async move {
        let missing: i64 = sqlx::query_scalar(
            "select count(*) from wh.chunks c where not exists (select 1 from wh.chunk_terms t where t.chunk_id = c.id)",
        )
        .fetch_one(&st.pool)
        .await
        .unwrap_or(0);
        if missing == 0 {
            return;
        }
        let job = st.jobs.queue("backfill", "warehouse ranking", lumbridge_library_core::legacy::wire::ACTOR);
        let _slot = INDEX_SLOT.acquire().await;
        st.jobs.start(job.id);
        let r = w::backfill_ranking(&st.pool, &st.cfg.library_root).await.map(|(terms, synced)| {
            json!({"terms": terms, "repos": synced.len(), "failed": synced.iter().filter(|o| o.error.is_some()).count()})
        });
        let summary = match &r {
            Ok(v) => format!("warehouse ranking backfill: {missing} chunks got search terms; {} repos refreshed (cards v2)", v["repos"]),
            Err(e) => format!("warehouse ranking backfill failed: {e:#}"),
        };
        db::record(&st.pool, "backfill", "warehouse", "chunk_terms", r.is_ok(), Some(lumbridge_library_core::legacy::wire::ACTOR), Some(&summary), r.as_ref().map(|v| v.clone()).unwrap_or_default()).await;
        st.jobs.finish(job.id, r);
    });
}

#[derive(Deserialize)]
struct SlugQ {
    slug: String,
}
async fn card(State(st): State<AppState>, Query(q): Query<SlugQ>) -> Res {
    match w::card(&st.pool, &q.slug).await.map_err(e500)? {
        Some(c) => ok(json!({"slug": q.slug, "card_md": c})),
        None => Err((StatusCode::NOT_FOUND, format!("no card for {:?}", q.slug))),
    }
}

async fn components(State(st): State<AppState>, Query(f): Query<w::ComponentFilter>) -> Res {
    ok(w::list_components(&st.pool, &f).await.map_err(e500)?)
}
async fn component_create(State(st): State<AppState>, Json(c): Json<w::NewComponent>) -> Res {
    ok(create_component(&st, c).await.map_err(e400)?)
}

#[derive(Deserialize, Default)]
struct ActionBody {
    project: Option<String>,
    commit: Option<String>,
}
/// approve | retire | use — approve/retire are CLI-only (the owner, admin token), never exposed over MCP.
async fn component_action(
    State(st): State<AppState>,
    h: HeaderMap,
    AxPath((slug, action)): AxPath<(String, String)>,
    body: Option<Json<ActionBody>>,
) -> Res {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    if action != "use" {
        crate::rest::require_admin(&st, &h, "approving or retiring a component")?;
    }
    let actor = if action == "use" { client(&h) } else { crate::rest::owner_actor(&h) };
    match action.as_str() {
        "approve" | "retire" => {
            let row = if action == "approve" {
                w::approve_component(&st.pool, &slug).await.map_err(e400)?
            } else {
                w::retire_component(&st.pool, &slug).await.map_err(e400)?
            };
            let pushed = sync_component_status(&st, &row).await;
            let summary = format!("{} component {}", if action == "approve" { "approved" } else { "retired" }, row.slug);
            db::record(&st.pool, &action, "warehouse", &row.slug, true, Some(&actor), Some(&summary), json!({"status_file": pushed})).await;
            ok(json!({"component": row, "status_file": pushed}))
        }
        "use" => {
            let project = b.project.ok_or((StatusCode::BAD_REQUEST, "project required".into()))?;
            let u = w::record_usage(&st.pool, &slug, &project, b.commit.as_deref(), &actor).await.map_err(e400)?;
            db::record(&st.pool, "use", "warehouse", &slug, true, Some(&actor), Some(&format!("{slug} copied into {project}")), json!({"commit": b.commit})).await;
            ok(u)
        }
        _ => Err((StatusCode::NOT_FOUND, format!("unknown action {action}"))),
    }
}

#[derive(Deserialize)]
struct RemoveBody {
    slug: String,
    #[serde(default)]
    delete_files: bool,
}
async fn remove(State(st): State<AppState>, h: HeaderMap, Json(b): Json<RemoveBody>) -> Res {
    crate::rest::require_admin(&st, &h, "removing a repo")?;
    let _slot = acquire_slot().await.map_err(e400)?;
    let removed = w::remove_repo_in(&st.pool, &st.cfg.library_root, &b.slug, b.delete_files).await.map_err(e400)?;
    if removed {
        let summary = format!("removed {}{}", b.slug, if b.delete_files { " (and its files)" } else { " (index only, files kept)" });
        db::record(&st.pool, "remove", "warehouse", &b.slug, true, Some(&crate::rest::owner_actor(&h)), Some(&summary), json!({"delete_files": b.delete_files})).await;
    }
    ok(json!({"removed": removed}))
}
