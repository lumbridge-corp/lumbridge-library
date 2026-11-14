//! REST for the `lumbridge-docs` / `lumbridge-warehouse` CLIs (same bearer token as MCP). Thin: each route calls
//! the same engine function as the matching MCP tool.

use crate::state::AppState;
use crate::{clip, ops, wh};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::NaiveDate;
use lumbridge_library_core::{docs, ledger};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

type Res = Result<Json<Value>, (StatusCode, String)>;

fn ok<T: serde::Serialize>(v: T) -> Res {
    Ok(Json(serde_json::to_value(v).unwrap_or(Value::Null)))
}
fn err(code: StatusCode) -> impl Fn(anyhow::Error) -> (StatusCode, String) {
    move |e| (code, format!("{e:#}"))
}
const E500: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;
const E400: StatusCode = StatusCode::BAD_REQUEST;

/// Validation errors from the engines ("unknown layer …", "unknown repo …") are the caller's fault: 400.
pub fn client_or_server(e: anyhow::Error) -> (StatusCode, String) {
    let m = format!("{e:#}");
    let code = if m.starts_with("unknown ") || m.contains("valid ") { E400 } else { E500 };
    (code, m)
}

/// Owner-only routes need `x-library-admin` (the admin token) on top of the bearer token.
pub fn require_admin(st: &AppState, h: &HeaderMap, what: &str) -> Result<(), (StatusCode, String)> {
    if st.is_admin(h) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, format!("{what} is owner-only: needs the admin token (admin-token in ~/.config/lumbridge-library)\n")))
    }
}

/// The owner acting through a client, as logged: `<owner>@<client>`.
pub fn owner_actor(h: &HeaderMap) -> String {
    format!("{}@{}", lumbridge_library_core::identity::get().owner, client(h))
}

pub fn client(h: &HeaderMap) -> String {
    h.get(lumbridge_library_core::legacy::wire::HEADER_CLIENT).and_then(|v| v.to_str().ok()).unwrap_or("cli").to_string()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/docs/resolve", get(docs_resolve))
        .route("/v1/docs/query", post(docs_query))
        .route("/v1/docs/search", post(docs_search))
        .route("/v1/docs/read", get(docs_read))
        .route("/v1/docs/facts", get(docs_facts))
        .route("/v1/docs/fact", post(docs_add_fact))
        .route("/v1/docs/clip", post(docs_clip))
        .route("/v1/docs/write", post(docs_write))
        .route("/v1/docs/lint", get(docs_lint))
        .route("/v1/docs/sync", post(docs_sync))
        .route("/v1/verdict", post(verdict))
        .route("/v1/jobs", get(jobs))
        .route("/v1/export", post(export))
        .route("/v1/evals", get(evals_list).post(evals_add))
        .route("/v1/evals/run", post(evals_run))
        .route("/v1/activity", get(crate::activity::list))
        .merge(wh::routes())
        .merge(crate::designs::routes())
        .merge(crate::librarian::routes())
}

#[derive(Deserialize)]
struct NameQ {
    name: String,
    limit: Option<usize>,
}

async fn docs_resolve(State(st): State<AppState>, Query(q): Query<NameQ>) -> Res {
    ok(docs::resolve(&st.pool, &q.name, q.limit.unwrap_or(8).clamp(1, 30)).await.map_err(err(E500))?)
}

#[derive(Deserialize)]
struct QueryBody {
    question: String,
    scope: Option<String>,
    max_tokens: Option<usize>,
}

async fn docs_query(State(st): State<AppState>, h: HeaderMap, Json(b): Json<QueryBody>) -> Res {
    let mid = st.model_id().await;
    let ans = docs::query(&st.pool, &st.embedder, &st.reranker, mid, &b.question, b.scope.as_deref(), b.max_tokens)
        .await
        .map_err(err(E500))?;
    let qa_id = ledger::log_qa(
        &st.pool,
        ledger::QaLog {
            shelf: "docs",
            asked_by: client(&h),
            question: b.question,
            resolved_id: b.scope,
            cited: serde_json::to_value(&ans.citations).unwrap_or_default(),
            ..Default::default()
        },
    )
    .await;
    let mut v = serde_json::to_value(&ans).unwrap_or_default();
    v["qa_id"] = json!(qa_id);
    Ok(Json(v))
}

async fn docs_search(State(st): State<AppState>, Json(mut p): Json<docs::SearchParams>) -> Res {
    p.limit = p.limit.clamp(1, 50);
    let mid = st.model_id().await;
    ok(docs::search(&st.pool, &st.embedder, &st.reranker, mid, &p).await.map_err(client_or_server)?)
}

#[derive(Deserialize)]
struct ReadQ {
    path: String,
    start: Option<usize>,
    end: Option<usize>,
}

async fn docs_read(State(st): State<AppState>, Query(q): Query<ReadQ>) -> Res {
    let lines = match (q.start, q.end) {
        (None, None) => None,
        (s, e) => Some((s.unwrap_or(1), e.unwrap_or(usize::MAX))),
    };
    match docs::read(&st.pool, &q.path, lines).await.map_err(err(E500))? {
        Some(d) => ok(d),
        None => Err((StatusCode::NOT_FOUND, format!("no document {:?}", q.path))),
    }
}

#[derive(Deserialize)]
struct FactsQ {
    entity: String,
    attribute: Option<String>,
    as_of: Option<NaiveDate>,
    #[serde(default)]
    history: bool,
}

async fn docs_facts(State(st): State<AppState>, Query(q): Query<FactsQ>) -> Res {
    ok(docs::facts(&st.pool, &q.entity, q.attribute.as_deref(), q.as_of, q.history).await.map_err(err(E500))?)
}

async fn docs_add_fact(State(st): State<AppState>, Json(f): Json<docs::NewFact>) -> Res {
    ok(docs::add_fact(&st.pool, f).await.map_err(err(E400))?)
}

#[derive(Deserialize)]
struct ClipBody {
    url: String,
    title: Option<String>,
    body: Option<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    message: Option<String>,
    author: Option<String>,
}

/// CLI clip: with no `body`, the server fetches and extracts the URL (HTML via trafilatura, PDF via
/// pdftotext). MCP clips must carry their text: agents get no web access through the library, on purpose.
async fn docs_clip(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ClipBody>) -> Res {
    if b.body.is_none() {
        require_admin(&st, &h, "fetching a URL on the server")?;
    }
    let (title, body, tags) = match b.body {
        Some(body) => (b.title.unwrap_or_else(|| b.url.clone()), body, b.tags.clone()),
        None => {
            let cfg = st.cfg.clipper.as_ref().ok_or((E400, "no [clipper] configured; send body".to_string()))?;
            let x = clip::extract(cfg, &b.url).await.map_err(err(E400))?;
            let mut header = String::new();
            for (k, v) in [("Author", &x.author), ("Date", &x.date), ("Site", &x.sitename)] {
                if let Some(v) = v {
                    header.push_str(&format!("- {k}: {v}\n"));
                }
            }
            if !header.is_empty() {
                header.push('\n');
            }
            let mut tags = b.tags.clone();
            if x.kind == "pdf" && !tags.iter().any(|t| t == "pdf") {
                tags.push("pdf".into());
            }
            (b.title.or(x.title).unwrap_or_else(|| b.url.clone()), format!("{header}{}", x.markdown), tags)
        }
    };
    let c = docs::Clip {
        url: b.url,
        title,
        body,
        author: b.author.unwrap_or_else(|| {
            let id = lumbridge_library_core::identity::get();
            if id.is_agent("claude") { "claude".into() } else { id.default_agent().to_string() }
        }),
        categories: b.categories,
        tags,
        message: b.message,
    };
    let msg = c.message.clone().unwrap_or_else(|| format!("clip {}", c.url));
    let out = docs::clip(&st.pool, &st.cfg.vault_root(), c).await.map_err(err(E400))?;
    ops::after_vault_write(&st, &out, "clip", &client(&h), Some(&msg));
    ok(out)
}

async fn docs_write(State(st): State<AppState>, h: HeaderMap, Json(w): Json<docs::WriteDoc>) -> Res {
    let msg = w.message.clone();
    let out = docs::write_doc(&st.pool, &st.cfg.vault_root(), w).await.map_err(err(E400))?;
    ops::after_vault_write(&st, &out, "write", &client(&h), msg.as_deref());
    ok(out)
}

async fn docs_lint(State(st): State<AppState>) -> Res {
    ok(docs::lint(&st.pool, &st.cfg.vault_root()).await.map_err(err(E500))?)
}

async fn docs_sync(State(st): State<AppState>) -> Res {
    ok(ops::vault_once(&st).await.map_err(err(E500))?)
}

#[derive(Deserialize)]
struct VerdictBody {
    qa_id: Uuid,
    verdict: i16,
    correction: Option<String>,
    answer: Option<String>,
}

async fn verdict(State(st): State<AppState>, h: HeaderMap, Json(b): Json<VerdictBody>) -> Res {
    require_admin(&st, &h, "recording a verdict from the CLI")?;
    let by = owner_actor(&h);
    let v = ledger::verdict(&st.pool, b.qa_id, b.verdict, b.correction.as_deref(), b.answer.as_deref(), &by)
        .await
        .map_err(err(E400))?;
    crate::activity::verdict(&st, &v, &by, b.correction.as_deref()).await;
    ok(v)
}

async fn jobs(State(st): State<AppState>) -> Res {
    ok(st.jobs.recent(50))
}

#[derive(Deserialize)]
struct ExportBody {
    kind: ledger::ExportKind,
    #[serde(default)]
    include_paid: bool,
}

async fn export(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ExportBody>) -> Res {
    require_admin(&st, &h, "exporting a dataset")?;
    ok(ledger::export(&st.pool, &st.cfg.library_root, b.kind, b.include_paid).await.map_err(err(E500))?)
}

#[derive(Deserialize)]
struct SetQ {
    set: Option<String>,
}

async fn evals_list(State(st): State<AppState>, Query(q): Query<SetQ>) -> Res {
    ok(ledger::evals(&st.pool, q.set.as_deref()).await.map_err(err(E500))?)
}

#[derive(Deserialize)]
struct EvalBody {
    set: String,
    question: String,
    expected: String,
    #[serde(default = "retrieval")]
    grader: String,
    #[serde(default)]
    tags: Vec<String>,
}
fn retrieval() -> String {
    "retrieval".into()
}

async fn evals_add(State(st): State<AppState>, Json(b): Json<EvalBody>) -> Res {
    ok(json!({"id": ledger::add_eval(&st.pool, &b.set, &b.question, &b.expected, &b.grader, &b.tags).await.map_err(err(E400))?}))
}

/// Retrieval evals (see `ledger::run_retrieval_evals`; the librarian runs the same thing nightly).
async fn evals_run(State(st): State<AppState>, Json(q): Json<SetQ>) -> Res {
    let mid = st.model_id().await;
    let fp = st.embed_model.read().await.as_ref().map(|(_, m)| m.fingerprint.clone()).unwrap_or_default();
    ok(ledger::run_retrieval_evals(&st.pool, &st.embedder, &st.reranker, mid, &fp, q.set.as_deref()).await.map_err(err(E500))?)
}
