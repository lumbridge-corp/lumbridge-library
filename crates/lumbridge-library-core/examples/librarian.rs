//! Run the librarian by hand against a database + vault checkout (default: dry run).
//!
//! ```text
//! set -a; source ./test.env; set +a   # TEST_DATABASE_URL=... TEST_EMBED_URL=... TEST_RERANK_URL=...
//! cargo run -p lumbridge-library-core --example librarian -- --vault /tmp/vault-copy \
//!     [--llm http://127.0.0.1:8000/v1] [--tasks facts,links] [--paths References/,Clippings/X.md] [--max-edits 40] [--apply] [--out report.json]
//! ```
//! Uses TEST_DATABASE_URL / TEST_EMBED_URL / TEST_RERANK_URL (never production).

use lumbridge_library_core::config::{EmbedConfig, LibrarianConfig, RerankConfig};
use lumbridge_library_core::librarian::{self, Ctx, RunOptions, Task};
use lumbridge_library_core::llm::Llm;
use lumbridge_library_core::models::{Embedder, Reranker};
use lumbridge_library_core::db;
use std::path::PathBuf;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let vault = PathBuf::from(arg(&args, "--vault").expect("--vault PATH"));
    let db_url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    let pool = db::connect(&db_url, 4).await?;
    if args.iter().any(|a| a == "--stats") {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("select layer, count(*) from kb.documents group by layer order by layer").fetch_all(&pool).await?;
        println!("{rows:?}");
        let n: i64 = sqlx::query_scalar("select count(*) from kb.facts").fetch_one(&pool).await?;
        println!("facts: {n}");
        return Ok(());
    }
    let embedder = Embedder::new(EmbedConfig {
        base_url: std::env::var("TEST_EMBED_URL")?,
        model: "embed".into(),
        dims: 2048,
        query_prefix: "query: ".into(),
        passage_prefix: "passage: ".into(),
        batch: 32,
    });
    let reranker = Reranker::new(RerankConfig { base_url: std::env::var("TEST_RERANK_URL")?, model: "rerank".into() });
    let ident = embedder.identity().await?;
    let mid = db::register_embed_model(&pool, &ident).await?;

    let mut cfg = LibrarianConfig::default();
    if let Some(u) = arg(&args, "--llm") {
        cfg.base_url = u;
    }
    if let Some(n) = arg(&args, "--max-edits") {
        cfg.max_edits_per_run = n.parse()?;
    }
    if let Some(n) = arg(&args, "--max-facts") {
        cfg.max_facts_per_run = n.parse()?;
    }
    if let Some(n) = arg(&args, "--max-llm") {
        cfg.max_llm_calls = n.parse()?;
    }
    let tasks: Vec<Task> = arg(&args, "--tasks")
        .map(|t| t.split(',').map(|x| serde_json::from_value(serde_json::json!(x.trim())).expect("task name")).collect())
        .unwrap_or_default();
    let llm = (!args.iter().any(|a| a == "--no-llm")).then(|| Llm::from_config(&cfg));
    let ctx = Ctx {
        pool: &pool,
        vault_root: &vault,
        embedder: &embedder,
        reranker: &reranker,
        embed_model_id: mid,
        fingerprint: ident.fingerprint.clone(),
        llm,
        cfg,
    };
    let dry = !args.iter().any(|a| a == "--apply");
    let path_prefixes = arg(&args, "--paths").map(|p| p.split(',').map(str::to_string).collect()).unwrap_or_default();
    let report = librarian::run(&ctx, RunOptions { dry_run: dry, tasks, requested_by: "cli-example".into(), path_prefixes }).await;
    let text = serde_json::to_string_pretty(&report)?;
    match arg(&args, "--out") {
        Some(p) => std::fs::write(&p, &text)?,
        None => println!("{text}"),
    }
    println!("{}", report.summary);
    Ok(())
}
