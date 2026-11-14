//! The design shelf in lumbridge-libraryd: add / update jobs (share the index slot with repo jobs), Gitea
//! publishing as `<component_org>/design-<slug>`, the caption loop (vision model) and the REST routes the
//! `lumbridge-warehouse design` CLI and other clients use.

use crate::rest::{client, client_or_server, require_admin};
use crate::state::AppState;
use crate::{gitops, jobs::Job};
use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lumbridge_library_core::db;
use lumbridge_library_core::warehouse::design::{self as dz, DesignFilter, DesignSearchParams, NewDesign};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::LazyLock;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// One caption round at a time (the loop and `POST /v1/designs/caption`).
static CAPTION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// The vision client, or None when `[designs] vision_base_url` is not set (captions off).
pub fn vision(st: &AppState) -> Option<dz::Vision> {
    st.cfg.designs.vision_base_url.as_deref().map(|b| dz::Vision::new(b, &st.cfg.designs.vision_model))
}

/// Queue "write + index + publish" for a design. Returns at once with a job.
pub fn add_design_job(st: &AppState, d: NewDesign) -> Job {
    let target = format!("designs/{}", d.slug);
    if let Some(j) = st.jobs.pending("design", &target) {
        return j;
    }
    let by = d.created_by.clone();
    let job = st.jobs.queue("design", &target, &by);
    let (st, id) = (st.clone(), job.id);
    tokio::spawn(async move {
        let r = async {
            let row = {
                let _slot = crate::wh::INDEX_SLOT.acquire().await;
                st.jobs.start(id);
                dz::upsert_design(&st.pool, &st.cfg.library_root, d).await?
            };
            let mut out = json!({ "design": row });
            if let (Some(g), Some(dir)) = (&st.cfg.gitea, row.local_path.as_deref()) {
                let name = format!("design-{}", row.slug);
                match gitops::publish_component(g, &name, &row.summary, std::path::Path::new(dir)).await {
                    Ok(full) => {
                        let _ = sqlx::query("update wh.repos set gitea_full_name = $2 where slug = $1")
                            .bind(format!("designs/{}", row.slug))
                            .bind(&full)
                            .execute(&st.pool)
                            .await;
                        out["gitea"] = json!(full);
                    }
                    Err(e) => out["gitea_error"] = json!(format!("{e:#}")),
                }
            }
            // Caption the new screenshots now rather than at the next tick.
            let st2 = st.clone();
            tokio::spawn(async move {
                if let Err(e) = caption_round(&st2).await {
                    warn!("design captions: {e:#}");
                }
            });
            anyhow::Ok(out)
        }
        .await;
        let summary = match &r {
            Ok(v) => format!(
                "design {} saved ({} shots, {})",
                v["design"]["slug"].as_str().unwrap_or(&target),
                v["design"]["shots"],
                v["gitea"].as_str().map(|g| format!("Gitea {g}")).unwrap_or_else(|| "not on Gitea".into())
            ),
            Err(e) => format!("design {target} failed: {e:#}"),
        };
        db::record(
            &st.pool,
            "design",
            "warehouse",
            &target,
            r.is_ok(),
            Some(&by),
            Some(&summary),
            r.as_ref().map(|v| json!({"gitea": v.get("gitea"), "gitea_error": v.get("gitea_error")})).unwrap_or_else(|e| json!({"error": format!("{e:#}")})),
        )
        .await;
        match &r {
            Ok(_) => info!(target, "design saved"),
            Err(e) => warn!("design {target}: {e:#}"),
        }
        st.jobs.finish(id, r);
    });
    job
}

/// Caption until the backlog is empty or the vision model stops answering; then embed.
pub async fn caption_round(st: &AppState) -> anyhow::Result<(usize, usize)> {
    let Some(v) = vision(st) else { return Ok((0, 0)) };
    let _g = CAPTION_LOCK.lock().await;
    let batch = st.cfg.designs.caption_batch.max(1) as i64;
    let (mut ok, mut failed) = (0, 0);
    loop {
        let (o, f) = dz::caption_pending(&st.pool, &st.cfg.library_root, &v, batch).await?;
        ok += o;
        failed += f;
        if o == 0 || o + f < batch as usize {
            break;
        }
    }
    if ok > 0 {
        info!(captioned = ok, failed, "design captions");
        let _ = crate::ops::embed_once(st).await;
    }
    Ok((ok, failed))
}

pub fn spawn_caption_loop(st: AppState) {
    let secs = st.cfg.designs.caption_secs;
    if secs == 0 || st.cfg.designs.vision_base_url.is_none() {
        return;
    }
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(secs));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            if let Err(e) = caption_round(&st).await {
                warn!("design captions: {e:#}");
            }
        }
    });
}

/// Approve / retire: database, design.toml, Gitea.
pub async fn set_status(st: &AppState, slug: &str, status: &str, actor: &str) -> anyhow::Result<Value> {
    let row = dz::set_status(&st.pool, slug, status).await?;
    let file = match dz::write_status_file(&row).await {
        Ok(true) => {
            let dir = row.local_path.as_deref().map(std::path::Path::new);
            match dir {
                Some(d) if st.cfg.gitea.is_some() && gitops::git(d, &["remote", "get-url", "gitea"]).await.is_ok() => {
                    match gitops::git(d, &["push", "-q", "gitea", "HEAD:main"]).await {
                        Ok(_) => json!("committed and pushed to Gitea"),
                        Err(e) => json!(format!("committed; push failed: {e:#}")),
                    }
                }
                _ => json!("committed (no Gitea remote)"),
            }
        }
        Ok(false) => json!("unchanged"),
        Err(e) => json!(format!("status file: {e:#}")),
    };
    let _ = crate::wh::sync_job(st, format!("designs/{}", row.slug), true, lumbridge_library_core::legacy::wire::ACTOR.into());
    let verb = if status == "approved" { "approved" } else { "retired" };
    db::record(&st.pool, verb, "warehouse", &format!("designs/{}", row.slug), true, Some(actor), Some(&format!("{verb} design {}", row.slug)), json!({"status_file": file})).await;
    Ok(json!({ "design": row, "status_file": file }))
}

// ───────────────────────────── REST ─────────────────────────────

type Res = Result<Json<Value>, (StatusCode, String)>;
fn ok<T: serde::Serialize>(v: T) -> Res {
    Ok(Json(serde_json::to_value(v).unwrap_or(Value::Null)))
}
fn e400(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, format!("{e:#}"))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/designs", get(list).post(add))
        .route("/v1/designs/search", post(search))
        .route("/v1/designs/caption", post(caption))
        .route("/v1/designs/{slug}", get(detail))
        .route("/v1/designs/{slug}/file", get(file))
        .route("/v1/designs/{slug}/shots/{id}", get(shot))
        .route("/v1/designs/{slug}/{action}", post(action))
}

async fn list(State(st): State<AppState>, Query(f): Query<DesignFilter>) -> Res {
    ok(dz::list_designs(&st.pool, &f).await.map_err(client_or_server)?)
}

async fn add(State(st): State<AppState>, h: HeaderMap, Json(mut d): Json<NewDesign>) -> Res {
    if d.created_by.trim().is_empty() {
        // The CLI is the owner at a terminal; other clients are named as themselves.
        let c = client(&h);
        d.created_by = if c == "cli" { lumbridge_library_core::identity::get().owner.clone() } else { c };
    }
    ok(add_design_job(&st, d))
}

async fn search(State(st): State<AppState>, Json(p): Json<DesignSearchParams>) -> Res {
    let mid = st.model_id().await;
    let mid = (mid != 0).then_some(mid);
    ok(dz::search(&st.pool, &st.embedder, &st.reranker, mid, &p).await.map_err(client_or_server)?)
}

async fn caption(State(st): State<AppState>) -> Res {
    let (captioned, failed) = caption_round(&st).await.map_err(e400)?;
    ok(json!({ "captioned": captioned, "failed": failed }))
}

async fn detail(State(st): State<AppState>, AxPath(slug): AxPath<String>) -> Res {
    match dz::design_detail(&st.pool, &slug).await.map_err(client_or_server)? {
        Some(d) => ok(d),
        None => Err((StatusCode::NOT_FOUND, format!("no design {slug:?}"))),
    }
}

#[derive(Deserialize)]
struct FileQ {
    path: String,
}
async fn file(State(st): State<AppState>, AxPath(slug): AxPath<String>, Query(q): Query<FileQ>) -> Res {
    let content = dz::read_design_file(&st.pool, &slug, &q.path).await.map_err(e400)?;
    ok(json!({ "design": slug, "path": q.path, "content": content }))
}

async fn shot(State(st): State<AppState>, AxPath((slug, id)): AxPath<(String, i64)>) -> Response {
    let path = match dz::shot_file(&st.pool, &st.cfg.library_root, &slug, id).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::NOT_FOUND, format!("{e:#}")).into_response(),
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "private, max-age=86400")],
            Body::from(bytes),
        )
            .into_response(),
        Err(e) => (StatusCode::NOT_FOUND, format!("{}: {e}", path.display())).into_response(),
    }
}

/// approve | retire (owner-only, admin token).
async fn action(State(st): State<AppState>, h: HeaderMap, AxPath((slug, action)): AxPath<(String, String)>) -> Res {
    require_admin(&st, &h, "approving or retiring a design")?;
    let status = match action.as_str() {
        "approve" => "approved",
        "retire" => "retired",
        _ => return Err((StatusCode::NOT_FOUND, format!("unknown action {action}"))),
    };
    ok(set_status(&st, &slug, status, &crate::rest::owner_actor(&h)).await.map_err(e400)?)
}
