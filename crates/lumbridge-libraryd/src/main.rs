//! lumbridge-libraryd — the Lumbridge Library service: Lumbridge Docs (`lumbridge-docs`) and
//! Lumbridge Warehouse (`lumbridge-warehouse`).
//!
//! HTTP (plain; bind it to loopback or a private network you trust):
//!   GET  /health      no auth; db + embed + rerank liveness (for Prometheus blackbox)
//!   GET  /metrics     no auth; Prometheus text
//!   GET  /v1/status   bearer token; counts per shelf, serving models
//!   /v1/*             bearer token; REST for the CLIs (owner-only routes also need the admin token)
//!   POST /mcp         bearer token; MCP (streamable HTTP) for agents

mod activity;
mod clip;
mod designs;
mod gitops;
mod jobs;
mod librarian;
mod mcp;
mod ops;
mod rest;
mod state;
mod wh;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use lumbridge_library_core::config::{Config, default_config_path};
use lumbridge_library_core::{db, legacy, status};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use state::AppState;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "lumbridge-libraryd", version, about = "Lumbridge Library service: Lumbridge Docs + Lumbridge Warehouse")]
struct Cli {
    /// Config file. Else $LUMBRIDGE_LIBRARYD_CONFIG, else
    /// ~/.config/lumbridge-library/lumbridge-libraryd.toml (earlier releases' names are still read).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Apply migrations and exit.
    #[arg(long)]
    migrate_only: bool,
}

/// `RUST_LOG` if set and valid, else info for our two crates. A filter written for the crate names
/// earlier releases used keeps showing the same lines.
fn log_filter(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;
    rust_log
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| EnvFilter::try_new(legacy::log_filter(s)).ok())
        .unwrap_or_else(|| EnvFilter::new(legacy::DEFAULT_LOG_FILTER))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(std::env::var("RUST_LOG").ok().as_deref()))
        .init();
    let cli = Cli::parse();
    let path = cli.config.or_else(|| legacy::env(legacy::ENV_DAEMON_CONFIG).map(PathBuf::from)).unwrap_or_else(default_config_path);
    let cfg = Config::load(&path)?;
    lumbridge_library_core::identity::set(cfg.identity.clone());

    let pool = db::connect(&cfg.database_url, 8).await?;
    db::migrate(&pool).await?;
    info!("migrations applied");
    if cli.migrate_only {
        return Ok(());
    }

    let state = AppState::new(cfg.clone(), pool).await?;
    match state.refresh_embed_model().await {
        Ok(id) => info!(embed_model_id = id, "embed model registered"),
        Err(e) => warn!("embed model not reachable at startup: {e:#}"),
    }

    let mcp_service = {
        let st = state.clone();
        StreamableHttpService::new(
            move || Ok(mcp::LibraryMcp::new(st.clone())),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_allowed_hosts(cfg.allowed_hosts())
                .with_json_response(true),
        )
    };

    ops::spawn_loops(state.clone());
    wh::backfill_if_needed(&state);

    let protected = Router::new()
        .route("/v1/status", get(status_handler))
        .merge(rest::routes())
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(state.clone(), require_token));

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .merge(protected)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    info!("lumbridge-libraryd {} listening on {}", lumbridge_library_core::VERSION, cfg.listen);
    db::log_op(&state.pool, "start", "library", &cfg.listen.to_string(), true, serde_json::json!({"version": lumbridge_library_core::VERSION})).await;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn require_token(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| constant_time_eq(t.as_bytes(), st.token.as_bytes()))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or wrong bearer token\n").into_response()
    }
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn health_handler(State(st): State<AppState>) -> impl IntoResponse {
    let h = st.health().await;
    let code = if h.db { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(h))
}

async fn status_handler(State(st): State<AppState>) -> Result<Json<state::Status>, (StatusCode, String)> {
    st.status().await.map(Json).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

async fn metrics_handler(State(st): State<AppState>) -> impl IntoResponse {
    let h = st.health().await;
    let mut out = String::new();
    let b = |v: bool| if v { 1 } else { 0 };
    out.push_str(&format!("# TYPE library_build_info gauge\nlibrary_build_info{{version=\"{}\"}} 1\n", lumbridge_library_core::VERSION));
    out.push_str(&format!("# TYPE library_db_up gauge\nlibrary_db_up {}\n", b(h.db)));
    out.push_str(&format!("# TYPE library_embed_up gauge\nlibrary_embed_up {}\n", b(h.embed)));
    out.push_str(&format!("# TYPE library_rerank_up gauge\nlibrary_rerank_up {}\n", b(h.rerank)));
    if let (Ok(d), Ok(w)) = (status::docs(&st.pool).await, status::warehouse(&st.pool).await) {
        out.push_str("# TYPE library_documents gauge\n");
        out.push_str(&format!("library_documents {}\n", d.documents));
        out.push_str("# TYPE library_chunks gauge\n");
        out.push_str(&format!("library_chunks{{shelf=\"docs\"}} {}\nlibrary_chunks{{shelf=\"warehouse\"}} {}\n", d.chunks, w.chunks));
        out.push_str("# TYPE library_unembedded_chunks gauge\n");
        out.push_str(&format!(
            "library_unembedded_chunks{{shelf=\"docs\"}} {}\nlibrary_unembedded_chunks{{shelf=\"warehouse\"}} {}\n",
            d.unembedded_chunks, w.unembedded_chunks
        ));
        out.push_str("# TYPE library_repos gauge\n");
        out.push_str(&format!("library_repos{{tier=\"mirror\"}} {}\nlibrary_repos{{tier=\"own\"}} {}\n", w.repos_mirror, w.repos_own));
        out.push_str(&format!("# TYPE library_components gauge\nlibrary_components {}\n", w.components));
    }
    out.push_str(&format!("# TYPE library_jobs_active gauge\nlibrary_jobs_active {}\n", st.jobs.active()));
    // Freshness: seconds since the last successful op of each kind (alert if vault sync goes stale).
    if let Ok(rows) = sqlx::query_as::<_, (String, String, f64)>(
        "select op, coalesce(shelf, ''), extract(epoch from now() - max(at))::float8 from lib.ops_log where ok group by op, shelf",
    )
    .fetch_all(&st.pool)
    .await
    {
        out.push_str("# TYPE library_last_op_age_seconds gauge\n");
        for (op, shelf, age) in rows {
            out.push_str(&format!("library_last_op_age_seconds{{op=\"{op}\",shelf=\"{shelf}\"}} {age:.0}\n"));
        }
    }
    if let Ok((qa, verdicts)) = sqlx::query_as::<_, (i64, i64)>("select count(*), count(verdict) from lib.qa").fetch_one(&st.pool).await {
        out.push_str(&format!("# TYPE library_qa_total gauge\nlibrary_qa_total {qa}\nlibrary_qa_verdicts_total {verdicts}\n"));
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn earlier_rust_log_still_shows_our_lines() {
        // A filter written for the crate names earlier releases used.
        let (old_daemon, old_core) = (lumbridge_library_core::legacy::LOG_TARGETS[0].0, lumbridge_library_core::legacy::LOG_TARGETS[1].0);
        let f = super::log_filter(Some(&format!("{old_daemon}=info,{old_core}=info"))).to_string();
        assert!(f.contains("lumbridge_libraryd=info") && f.contains("lumbridge_library_core=info"), "{f}");
        let d = super::log_filter(None).to_string();
        assert!(d.contains("lumbridge_libraryd=info") && d.contains("lumbridge_library_core=info"), "{d}");
        // An empty RUST_LOG is the default, not silence.
        assert_eq!(super::log_filter(Some(" ")).to_string(), d);
    }
}
