//! Retrieval quality check on real repos (all ignored by default; needs TEST_DATABASE_URL and the
//! models). Repos are registered as `own/test-wh_eval-<name>` so they never collide with real rows.
//!
//!   cargo test -p lumbridge-library-core --test warehouse_eval -- --ignored --test-threads=1 eval_setup --nocapture
//!   cargo test -p lumbridge-library-core --test warehouse_eval -- --ignored eval_questions --nocapture
//!   cargo test -p lumbridge-library-core --test warehouse_eval -- --ignored eval_cleanup
//!
//! `WH_EVAL_REPOS` overrides the repo list (comma-separated GitHub URLs).
//!
//! Rank-aware check (`eval_rank`, 12 code questions over ripgrep, rust-sdk, anyhow, tokio, axum;
//! `WH_EVAL_REPOS=` those five URLs for `eval_setup`; `WH_EVAL_DUMP=f.json` for the raw logits).
//! Results, 2026-11-08 (llama-nemotron-rerank-1b-v2, MRR@10 / hit@1 / hit@3):
//! - Before: passage `path\nsignature\n3000 chars`, f32 probabilities clamped at 1 − 1e-6:
//!   0.594 / 5 / 9, below retrieval alone (0.653 / 6 / 10). 14–25 of the 30 reranked candidates
//!   were p > 0.999 on the rmcp / tokio / axum questions (logits up to 35, f32 sigmoid = 1.0), and
//!   on the format the reranker preferred prose (GUIDE.md, CHANGELOG.md) over code.
//! - Raw logits alone: 0.646 / 6 / 9. New passage format alone: 0.699 / 7 / 9.
//! - Both (shipped): 0.750 / 8 / 9 with production-style repo names (`clean` format in the dump);
//!   0.694 / 7 / 10 end to end here, where the `test-wh_eval-` slug prefix in the passage header
//!   lowers every logit. Reranker alone on the new format: 0.790; query rewrites ("Find the
//!   implementation of …") were mixed (+0.03 on 20-line passages, −0.03 on 40) and not adopted.
//! - Miss in every variant: "rmcp streamable HTTP server" (`StreamableHttpService` never reaches
//!   the top 30, a retrieval problem), "anyhow context" (`ContextError` at fused #15).

mod warehouse_common;

use lumbridge_library_core::warehouse::{self as wh, CodeSearchParams, IndexOpts, SymbolQuery};
use std::time::Instant;
use warehouse_common::*;

const DEFAULT_REPOS: &[&str] = &[
    "https://github.com/BurntSushi/ripgrep",
    "https://github.com/modelcontextprotocol/rust-sdk",
    "https://github.com/dtolnay/anyhow",
];

/// (question, substring expected in path or symbol of a top-3 hit)
const QUESTIONS: &[(&str, &str)] = &[
    ("ripgrep gitignore matching", "ignore/src/gitignore.rs"),
    ("rmcp streamable HTTP server", "StreamableHttpService"),
    ("anyhow context", "ContextError"),
    ("sqlx query! macro expansion", "sqlx-macros-core/src/query"),
    ("tokio work stealing", "steal"),
    ("how does ripgrep walk directories in parallel", "ignore/src/walk.rs"),
    ("anyhow error downcast", "downcast"),
];

fn repos() -> Vec<String> {
    match std::env::var("WH_EVAL_REPOS") {
        Ok(s) if !s.trim().is_empty() => s.split(',').map(|x| x.trim().to_string()).collect(),
        _ => DEFAULT_REPOS.iter().map(|s| s.to_string()).collect(),
    }
}

fn slug_for(url: &str) -> (String, String) {
    let (_, name) = wh::owner_and_name(url).unwrap();
    // `test-wh_eval-`: still a test slug, but out of reach of the shared `test-wh-%` cleanup other
    // test runs do (re-embedding the eval repos takes minutes).
    let name = format!("test-wh_eval-{name}");
    (format!("own/{name}"), name)
}

fn eval_root() -> std::path::PathBuf {
    std::env::var("WH_EVAL_ROOT").map(Into::into).unwrap_or_else(|_| std::env::temp_dir().join("test-wh-eval"))
}

#[tokio::test]
#[ignore]
async fn eval_setup() {
    let Some(pool) = pool().await else { return };
    let root = eval_root();
    for url in repos() {
        let (slug, name) = slug_for(&url);
        let t = Instant::now();
        wh::add_own(&pool, &root, &url, Some(&name), &[]).await.unwrap();
        let r = wh::sync_repo(&pool, &root, &slug, &IndexOpts { fetch: false, ..Default::default() }).await.unwrap();
        eprintln!("{slug}: {} files, {} chunks in {:?}", r.files_added + r.files_changed, r.chunks_new, t.elapsed());
    }
    if let Some((embed, _)) = models() {
        let mid = model_id(&pool, &embed).await;
        let t = Instant::now();
        let n = wh::embed_pending(&pool, &embed, mid, 1_000_000).await.unwrap();
        eprintln!("embedded {n} chunks in {:?}", t.elapsed());
    }
}

#[tokio::test]
#[ignore]
async fn eval_questions() {
    let Some(pool) = pool().await else { return };
    let Some((embed, rerank)) = models() else { return };
    let mid = model_id(&pool, &embed).await;
    let own: Vec<String> = repos().iter().map(|u| slug_for(u).0).collect();
    let mut hit3 = 0;
    // `WH_EVAL_Q` runs one extra question (timing / exploration).
    let extra = std::env::var("WH_EVAL_Q").ok().map(|q| vec![(q, String::new())]);
    let questions: Vec<(String, String)> = extra.unwrap_or_else(|| QUESTIONS.iter().map(|(q, w)| (q.to_string(), w.to_string())).collect());
    for (q, want) in &questions {
        let t = Instant::now();
        let p = CodeSearchParams { query: q.to_string(), tiers: vec!["own".into()], limit: Some(6), ..Default::default() };
        let res = wh::search(&pool, &embed, &rerank, mid, &p).await.unwrap();
        let pos = res.hits.iter().position(|h| {
            !want.is_empty() && (h.path.contains(want.as_str()) || h.symbol.as_ref().is_some_and(|s| s.name.contains(want.as_str()) || s.signature.as_deref().unwrap_or("").contains(want.as_str())))
        });
        if pos.is_some_and(|p| p < 3) {
            hit3 += 1;
        }
        eprintln!("\n## {q:?} (want {want:?}) → {:?} in {:?} degraded={}", pos.map(|p| p + 1), t.elapsed(), res.degraded);
        for h in &res.hits {
            eprintln!(
                "  {:.3} {} {}:{}-{} {:?}",
                h.score,
                h.repo,
                h.path,
                h.line_start,
                h.line_end,
                h.symbol.as_ref().map(|s| format!("{} {}", s.kind, s.name))
            );
        }
    }
    eprintln!("\ntop-3: {hit3}/{}", questions.len());
    for name in ["rmcp", "mcp", "ripgrep", "time", "anyhow", "ignore"] {
        let r = wh::resolve(&pool, name, 3).await.unwrap();
        eprintln!("resolve {name:?}: {:?}", r.iter().map(|x| (x.slug.as_str(), x.score)).collect::<Vec<_>>());
    }
    for (name, repo) in [("Gitignore::matched", "test-wh_eval-ripgrep"), ("StreamableHttpService", "test-wh_eval-rust-sdk"), ("Context", "test-wh_eval-anyhow")] {
        if !own.iter().any(|s| s.ends_with(repo)) {
            continue;
        }
        let s = wh::symbols(&pool, &SymbolQuery { name: name.into(), repos: vec![repo.into()], limit: Some(5), ..Default::default() }).await;
        match s {
            Ok(s) => eprintln!(
                "symbols {name:?}: {:?}",
                s.iter().map(|h| format!("{} {}:{} {}", h.match_kind, h.path, h.line_start, h.kind)).collect::<Vec<_>>()
            ),
            Err(e) => eprintln!("symbols {name:?}: error {e:#}"),
        }
    }
}

/// Rank-aware code question set: (question, `a|b` = any of these substrings in path, symbol name or
/// signature).
const RANK_QUESTIONS: &[(&str, &str)] = &[
    ("ripgrep gitignore matching", "ignore/src/gitignore.rs"),
    ("how does ripgrep walk directories in parallel", "ignore/src/walk.rs"),
    ("rmcp streamable HTTP server", "StreamableHttpService"),
    ("rmcp tool macro and tool router", "tool_router|ToolRouter"),
    ("anyhow context", "ContextError"),
    ("anyhow error downcast", "downcast"),
    ("anyhow backtrace capture", "backtrace"),
    ("tokio work stealing", "steal"),
    ("tokio timer wheel", "wheel"),
    ("axum route extractors FromRequestParts", "FromRequestParts"),
    ("axum middleware with state", "from_fn_with_state"),
    ("tokio spawn blocking", "spawn_blocking"),
];

fn wants(want: &str, path: &str, sym: Option<&str>, sig: Option<&str>) -> bool {
    want.split('|').any(|w| path.contains(w) || sym.is_some_and(|s| s.contains(w)) || sig.is_some_and(|s| s.contains(w)))
}

fn rank_repos() -> Vec<String> {
    match std::env::var("WH_EVAL_REPOS") {
        Ok(_) => repos(),
        _ => [
            "https://github.com/BurntSushi/ripgrep",
            "https://github.com/modelcontextprotocol/rust-sdk",
            "https://github.com/dtolnay/anyhow",
            "https://github.com/tokio-rs/tokio",
            "https://github.com/tokio-rs/axum",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    }
}

/// End-to-end MRR / hit@1 / hit@3 (top 10) of [`RANK_QUESTIONS`] through `warehouse::search`.
/// With `WH_EVAL_DUMP=file.json`, also writes each question's reranked head (fused score, noise,
/// match flag, candidate fields) and the raw logits for several passage formats × query wordings,
/// for offline blend experiments.
#[tokio::test]
#[ignore]
async fn eval_rank() {
    let Some(pool) = pool().await else { return };
    let Some((embed, rerank)) = models() else { return };
    let mid = model_id(&pool, &embed).await;
    let own: Vec<String> = rank_repos().iter().map(|u| slug_for(u).0).collect();
    let dump = std::env::var("WH_EVAL_DUMP").ok();
    let (mut rr, mut h1, mut h3) = (0.0f64, 0, 0);
    let mut out = Vec::new();
    for (q, want) in RANK_QUESTIONS {
        let p = CodeSearchParams { query: q.to_string(), repos: own.clone(), limit: Some(10), ..Default::default() };
        let (res, trace) = wh::search_traced(&pool, &embed, &rerank, mid, &p).await.unwrap();
        let pos = res.hits.iter().position(|h| {
            wants(want, &h.path, h.symbol.as_ref().map(|s| s.name.as_str()), h.symbol.as_ref().and_then(|s| s.signature.as_deref()))
        });
        if let Some(p) = pos {
            rr += 1.0 / (p as f64 + 1.0);
            h1 += (p == 0) as usize;
            h3 += (p < 3) as usize;
        }
        let logits: Vec<f64> = trace.head.iter().filter_map(|c| c.logit).collect();
        let sat = logits.iter().filter(|l| **l > 6.9).count(); // p > 0.999
        let (lo, hi) = logits.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| (a.min(x), b.max(x)));
        eprintln!(
            "{:<48} pos={:<5} head={} p>0.999: {sat:>2}  logits {lo:>6.2}..{hi:>6.2}  top1 {}:{}",
            q,
            pos.map(|p| (p + 1).to_string()).unwrap_or("-".into()),
            trace.head.len(),
            res.hits.first().map(|h| h.path.as_str()).unwrap_or(""),
            res.hits.first().and_then(|h| h.symbol.as_ref()).map(|s| s.name.as_str()).unwrap_or("")
        );
        if dump.is_some() {
            out.push(dump_question(&rerank, q, want, &trace).await);
        }
    }
    let n = RANK_QUESTIONS.len() as f64;
    eprintln!("\nMRR@10 {:.3}  hit@1 {h1}/{}  hit@3 {h3}/{}", rr / n, RANK_QUESTIONS.len(), RANK_QUESTIONS.len());
    if let Some(f) = dump {
        std::fs::write(&f, serde_json::to_string(&out).unwrap()).unwrap();
        eprintln!("wrote {f}");
    }
}

/// Passage formats tried for the reranker (name, builder).
fn passage(fmt: &str, c: &wh::TraceCand) -> String {
    let repo = c.repo.rsplit('/').next().unwrap_or(&c.repo).trim_start_matches("test-wh_eval-");
    let lines = |n: usize| c.content.lines().take(n).collect::<Vec<_>>().join("\n");
    let head = || {
        let mut s = format!("{repo} {}", c.path);
        if let (Some(k), Some(n)) = (&c.sym_kind, &c.sym_name) {
            s.push_str(&format!(" :: {k} "));
            if let Some(p) = &c.parent {
                s.push_str(p);
                s.push_str("::");
            }
            s.push_str(n);
        }
        s
    };
    let doc = || c.doc.as_deref().map(|d| format!("{}\n", d.chars().take(600).collect::<String>())).unwrap_or_default();
    match fmt {
        "live" => c.rerank_doc.clone(),
        // As production sees it: the eval slugs carry a `test-wh_eval-` prefix that real repos do
        // not, and it lowers the logits of every passage.
        "clean" => c.rerank_doc.replacen("test-wh_eval-", "", 1),
        // Before the fix: path, signature, 3000 chars of the chunk.
        "old" => format!("{}\n{}{}", c.path, c.signature.as_deref().map(|s| format!("{s}\n")).unwrap_or_default(), c.content.chars().take(3000).collect::<String>()),
        "head40" => format!("{}\n{}{}", head(), doc(), lines(40)),
        "head20" => format!("{}\n{}{}", head(), doc(), lines(20)),
        "head80" => format!("{}\n{}{}", head(), doc(), lines(80)),
        "sigonly" => format!("{}\n{}{}", head(), doc(), c.signature.clone().unwrap_or_else(|| lines(3))),
        _ => unreachable!(),
    }
}

async fn dump_question(rerank: &lumbridge_library_core::models::Reranker, q: &str, want: &str, t: &wh::SearchTrace) -> serde_json::Value {
    let mut scores = serde_json::Map::new();
    let queries = [
        ("raw", q.to_string()),
        ("find", format!("Find the implementation of {q}")),
        ("code", format!("Which Rust code implements {q}?")),
    ];
    for fmt in ["live", "clean", "old", "head40", "head20", "head80", "sigonly"] {
        let docs: Vec<String> = t.head.iter().map(|c| passage(fmt, c)).collect();
        for (qn, qt) in &queries {
            let hits = rerank.rerank(qt, &docs).await.unwrap();
            let mut l = vec![0.0f64; docs.len()];
            for h in hits {
                l[h.index] = h.logit;
            }
            scores.insert(format!("{fmt}/{qn}"), serde_json::json!(l));
        }
    }
    let cands: Vec<serde_json::Value> = t
        .head
        .iter()
        .map(|c| {
            serde_json::json!({
                "repo": c.repo, "path": c.path, "sym": c.sym_name, "kind": c.sym_kind, "fused": c.fused, "noise": c.noise,
                "logit": c.logit, "hit": wants(want, &c.path, c.sym_name.as_deref(), c.signature.as_deref()),
                "chars": c.content.len(),
            })
        })
        .collect();
    serde_json::json!({ "q": q, "want": want, "cands": cands, "scores": scores })
}

/// What production runs once after migration 0007 (`backfill_ranking`), limited to the eval repos.
#[tokio::test]
#[ignore]
async fn eval_backfill() {
    let Some(pool) = pool().await else { return };
    let t = Instant::now();
    let r = wh::reindex_search_text(&pool).await.unwrap();
    eprintln!("reindex_search_text: {} chunks in {} batches, {:?}", r.chunks, r.batches, t.elapsed());
    for url in repos() {
        let (slug, _) = slug_for(&url);
        let t = Instant::now();
        let r = wh::sync_repo(&pool, &eval_root(), &slug, &IndexOpts { fetch: false, force: true, ..Default::default() }).await.unwrap();
        let row = wh::get_repo(&pool, &slug).await.unwrap().unwrap();
        eprintln!("{slug}: reindexed={} unchanged={} in {:?}; crates={:?}", r.reindexed, r.files_unchanged, t.elapsed(), &row.crates[..row.crates.len().min(8)]);
        eprintln!("  description: {:?}", row.description);
    }
}

#[tokio::test]
#[ignore]
async fn eval_cleanup() {
    let Some(pool) = pool().await else { return };
    for url in repos() {
        let (slug, _) = slug_for(&url);
        let _ = wh::remove_repo(&pool, &slug, false).await;
    }
    let _ = std::fs::remove_dir_all(eval_root());
}

/// `WH_SQL='select ...'` prints each row as JSON; `WH_EXEC` runs statements first (no psql here).
#[tokio::test]
#[ignore]
async fn eval_sql() {
    let Some(pool) = pool().await else { return };
    if let Ok(stmt) = std::env::var("WH_EXEC") {
        let r = sqlx::raw_sql(&stmt).execute(&pool).await.unwrap();
        println!("{} rows", r.rows_affected());
    }
    let Ok(sql) = std::env::var("WH_SQL") else { return };
    let rows: Vec<String> = sqlx::query_scalar(&format!("select to_jsonb(t)::text from ({sql}) t")).fetch_all(&pool).await.unwrap();
    for r in rows {
        println!("{r}");
    }
}
