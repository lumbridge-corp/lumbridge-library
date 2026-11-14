//! Code-aware retrieval against the scratch DB (skips without TEST_DATABASE_URL): chunk terms,
//! symbol leg, repo detection, noise demotion, per-file cap, symbol precision, grep caps, filter
//! validation, tree, card v2. Models are pointed at a dead port, so search runs on its lexical legs
//! (degraded) and is deterministic. Only touches its own `own/test-wh-rank` repo.

mod warehouse_common;

use lumbridge_library_core::config::{EmbedConfig, RerankConfig};
use lumbridge_library_core::models::{Embedder, Reranker};
use lumbridge_library_core::warehouse::{self as wh, CodeSearchParams, GrepParams, IndexOpts, SymbolQuery};
use sqlx::PgPool;
use warehouse_common::*;

const SLUG: &str = "own/test-wh-rank";

const LIB_RS: &str = r#"//! Widget core.

pub mod net;
pub mod gitignore;

/// A streamable HTTP widget service: accepts connections and hands each one a widget.
pub struct StreamableWidgetService {
    widgets: Vec<u32>,
}

impl StreamableWidgetService {
    /// Create an empty service.
    pub fn new() -> Self {
        StreamableWidgetService { widgets: Vec::new() }
    }

    /// Serve one request and return how many widgets were handed out.
    pub fn serve(&mut self, request: u32) -> usize {
        self.widgets.push(request);
        self.widgets.len()
    }
}

impl Drop for StreamableWidgetService {
    fn drop(&mut self) {
        self.widgets.clear();
    }
}
"#;

const NET_RS: &str = r#"use crate::StreamableWidgetService;

/// Spawn a widget task on the shared executor and return its id.
pub fn spawn(task: u32) -> u32 {
    let mut svc = StreamableWidgetService::new();
    svc.serve(task) as u32 + helper(task)
}

fn helper(task: u32) -> u32 {
    task.wrapping_mul(31).rotate_left(5)
}
"#;

const GITIGNORE_RS: &str = r#"/// Glob rules read from a .gitignore file.
pub struct IgnoreRules {
    globs: Vec<String>,
}

impl IgnoreRules {
    /// Whether a path matches one of the ignore globs (the last matching rule wins).
    pub fn matched_path(&self, path: &str) -> bool {
        self.globs.iter().rev().any(|g| path.ends_with(g.trim_start_matches('*')))
    }
}
"#;

const TEST_RS: &str = r#"/// Test double: spawn a widget task without an executor.
pub fn spawn(task: u32) -> u32 {
    task
}

#[test]
fn spawn_works() {
    assert_eq!(spawn(3), 3);
}
"#;

const EXAMPLE_RS: &str = r#"/// Example: spawn a widget task and print its id.
fn spawn(task: u32) -> u32 {
    println!("spawning widget task {task}");
    task
}

fn main() {
    spawn(1);
}
"#;

fn make_fixture(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    write(dir, "LICENSE", MIT);
    write(dir, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    write(
        dir,
        "README.md",
        "# Widgets\n\n[![ci](https://x/badge.svg)](https://x)\n\nA **toolkit** for [widgets](https://example.com). It is:\n\n* **Fast**: very fast.\n* **Small**: it fits\n  anywhere.\n\n## Usage\n\nSee the examples.\n",
    );
    write(dir, "crates/wcore/Cargo.toml", "[package]\nname = \"widgetcore\"\nversion = \"0.1.0\"\ndescription = \"Core widgets\"\n");
    write(dir, "crates/wcore/src/lib.rs", LIB_RS);
    write(dir, "crates/wcore/src/net.rs", NET_RS);
    write(dir, "crates/wcore/src/gitignore.rs", GITIGNORE_RS);
    write(dir, "crates/wcore/tests/net_test.rs", TEST_RS);
    write(dir, "crates/wcore/examples/demo.rs", EXAMPLE_RS);
    write(dir, "crates/widgetcore-tests/Cargo.toml", "[package]\nname = \"widgetcore-tests\"\nversion = \"0.1.0\"\n");
    write(dir, "crates/widgetcore-tests/src/lib.rs", "pub fn fixture_only() -> u32 {\n    42\n}\n");
    let mut big = String::new();
    for i in 0..3000 {
        big.push_str(&format!("// filler line {i:05} xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n"));
    }
    write(dir, "crates/wcore/src/filler.rs", &big);
    std::fs::write(dir.join("logo.png"), [0x89u8, b'P', b'N', b'G', 0, 1, 2, 3]).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial"]);
}

fn dead_models() -> (Embedder, Reranker) {
    let e = Embedder::new(EmbedConfig {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "embed".into(),
        dims: 2048,
        query_prefix: "query: ".into(),
        passage_prefix: "passage: ".into(),
        batch: 32,
    });
    (e, Reranker::new(RerankConfig { base_url: "http://127.0.0.1:9/v1".into(), model: "rerank".into() }))
}

async fn search(pool: &PgPool, q: &str, repos: Vec<String>) -> wh::CodeSearchResult {
    let (e, r) = dead_models();
    wh::search(pool, &e, &r, 0, &CodeSearchParams { query: q.into(), repos, limit: Some(10), ..Default::default() }).await.unwrap()
}

fn sym(h: &wh::CodeHit) -> Option<&str> {
    h.symbol.as_ref().map(|s| s.name.as_str())
}

#[tokio::test]
async fn warehouse_ranking_and_precision() {
    let Some(pool) = pool().await else {
        eprintln!("TEST_DATABASE_URL not set; skipping");
        return;
    };
    let _ = wh::remove_repo(&pool, SLUG, false).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("library");
    let src = tmp.path().join("src/test-wh-rank");
    make_fixture(&src);
    wh::add_own(&pool, &root, src.to_str().unwrap(), Some("test-wh-rank"), &[]).await.unwrap();
    let r = wh::sync_repo(&pool, &root, SLUG, &IndexOpts::default()).await.unwrap();
    assert!(r.reindexed);
    let row = wh::get_repo(&pool, SLUG).await.unwrap().unwrap();

    // ── package names + description ──
    assert_eq!(row.crates, ["widgetcore"], "test packages are dropped");
    assert_eq!(row.description.as_deref(), Some("A toolkit for widgets. It is: Fast: very fast; Small: it fits anywhere."));

    // ── chunk terms: written by the indexer, refilled by sync when missing ──
    let count_terms = || async {
        sqlx::query_scalar::<_, i64>(
            "select count(*) from wh.chunks c join wh.repos r on r.id = c.repo_id
              where r.slug = $1 and not exists (select 1 from wh.chunk_terms t where t.chunk_id = c.id)",
        )
        .bind(SLUG)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(count_terms().await, 0);
    let text: String = sqlx::query_scalar(
        "select t.search_text from wh.chunk_terms t join wh.chunks c on c.id = t.chunk_id join wh.symbols s on s.id = c.symbol_id
          join wh.repos r on r.id = c.repo_id where r.slug = $1 and s.name = 'StreamableWidgetService' and s.kind = 'struct'",
    )
    .bind(SLUG)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(text.split(' ').any(|w| w == "streamable") && text.split(' ').any(|w| w == "widget"), "{text}");
    sqlx::query("delete from wh.chunk_terms t using wh.chunks c, wh.repos r where c.id = t.chunk_id and r.id = c.repo_id and r.slug = $1")
        .bind(SLUG)
        .execute(&pool)
        .await
        .unwrap();
    assert!(count_terms().await > 0);
    let r = wh::sync_repo(&pool, &root, SLUG, &IndexOpts::default()).await.unwrap();
    assert!(!r.reindexed);
    assert_eq!(count_terms().await, 0, "sync_repo refills missing chunk terms");

    // ── lexical legs: camelCase / path words, symbol names, noise, per-file cap ──
    let slug = vec![SLUG.to_string()];
    let res = search(&pool, "streamable widget service", slug.clone()).await;
    assert!(res.degraded);
    assert_eq!(sym(&res.hits[0]), Some("StreamableWidgetService"), "{:?}", res.hits.iter().map(|h| (&h.path, sym(h))).collect::<Vec<_>>());
    let res = search(&pool, "gitignore matching", slug.clone()).await;
    assert_eq!(res.hits[0].path, "crates/wcore/src/gitignore.rs", "{:?}", res.hits.iter().map(|h| (&h.path, sym(h))).collect::<Vec<_>>());
    let res = search(&pool, "spawn a widget task", slug.clone()).await;
    let first_spawn = res.hits.iter().find(|h| sym(h) == Some("spawn")).unwrap();
    assert_eq!(first_spawn.path, "crates/wcore/src/net.rs", "source before tests/examples: {:?}", res.hits.iter().map(|h| (&h.path, sym(h))).collect::<Vec<_>>());
    let res = search(&pool, "example of spawning a widget task", slug.clone()).await;
    assert!(res.hits.iter().take(3).any(|h| h.path.contains("examples/")), "asked for examples: {:?}", res.hits.iter().map(|h| &h.path).collect::<Vec<_>>());
    let res = search(&pool, "widget", slug.clone()).await;
    let mut per: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for h in &res.hits {
        *per.entry(h.path.as_str()).or_default() += 1;
    }
    assert!(per.values().all(|n| *n <= 2), "per-file cap: {per:?}");

    // ── repo detection: a package name in the question scopes the search ──
    let res = search(&pool, "widgetcore spawn", vec![]).await;
    assert_eq!(res.detected_repos, [SLUG]);
    assert!(res.hits.iter().all(|h| h.repo == SLUG));
    assert_eq!(res.hits[0].path, "crates/wcore/src/net.rs");

    // ── resolve by package name ──
    let refs = wh::resolve(&pool, "widgetcore", 3).await.unwrap();
    assert_eq!(refs[0].slug, SLUG);
    assert!(refs[0].score >= 0.9);
    let refs = wh::resolve(&pool, "idgetcor", 3).await.unwrap();
    assert!(refs.iter().any(|r| r.slug == SLUG), "package substring");

    // ── filter validation ──
    let (e, rr) = dead_models();
    let bad = |p: CodeSearchParams| {
        let (e, rr) = (&e, &rr);
        let pool = &pool;
        async move { wh::search(pool, e, rr, 0, &p).await.unwrap_err().to_string() }
    };
    let err = bad(CodeSearchParams { query: "x".into(), tiers: vec!["bogus".into()], ..Default::default() }).await;
    assert!(err.contains("valid tiers: mirror, own, component"), "{err}");
    let err = bad(CodeSearchParams { query: "x".into(), languages: vec!["klingon".into()], ..Default::default() }).await;
    assert!(err.contains("indexed languages") && err.contains("rust"), "{err}");
    let err = bad(CodeSearchParams { query: "x".into(), repos: vec!["test-wh-no-such-repo".into()], ..Default::default() }).await;
    assert!(err.contains("unknown repo") && err.contains(SLUG), "{err}");
    // A very long query is capped, not rejected.
    let long = "widget ".repeat(5000);
    assert!(!search(&pool, &long, slug.clone()).await.hits.is_empty());

    // ── symbols: precision ──
    let q = |name: &str| SymbolQuery { name: name.into(), repos: slug.clone(), ..Default::default() };
    let err = wh::symbols(&pool, &q("*")).await.unwrap_err().to_string();
    assert!(err.contains("empty"), "{err}");
    let s = wh::symbols(&pool, &q("spawn")).await.unwrap();
    assert_eq!(s[0].path, "crates/wcore/src/net.rs", "real source first: {s:?}");
    assert!(s.iter().all(|h| !h.name.is_empty()));
    for path in ["widgetcore::spawn", "crate::net::spawn", "net::spawn"] {
        let s = wh::symbols(&pool, &q(path)).await.unwrap();
        assert!(!s.is_empty() && s[0].path == "crates/wcore/src/net.rs", "{path}: {s:?}");
    }
    let s = wh::symbols(&pool, &q("nosuchmodule::spawn")).await.unwrap();
    assert!(s.is_empty(), "{s:?}");
    let s = wh::symbols(&pool, &SymbolQuery { name: "s*".into(), repos: slug.clone(), limit: Some(10_000), ..Default::default() }).await.unwrap();
    assert!(s.len() <= 200);

    // read_symbol: exact only, candidates listed.
    let sv = wh::read_symbol(&pool, &root, SLUG, "spawn").await.unwrap();
    assert_eq!(sv.symbol.path, "crates/wcore/src/net.rs");
    assert!(sv.candidates.iter().any(|c| c.path.contains("tests/")), "{:?}", sv.candidates);
    assert!(!sv.ambiguous);
    let err = wh::read_symbol(&pool, &root, SLUG, "spaw").await.unwrap_err().to_string();
    assert!(err.contains("candidates") && err.contains("spawn"), "{err}");
    let sv = wh::read_symbol(&pool, &root, SLUG, "StreamableWidgetService::serve").await.unwrap();
    assert!(sv.view.content.contains("pub fn serve"));

    // ── grep caps and errors ──
    let err = wh::grep(&pool, &root, &GrepParams { pattern: "(".into(), regex: true, ..Default::default() }).await.unwrap_err().to_string();
    assert!(err.contains("invalid regex"), "{err}");
    assert_eq!(err.matches("invalid").count(), 1, "{err}");
    let g = wh::grep(&pool, &root, &GrepParams { pattern: "filler".into(), repos: slug.clone(), max_matches: Some(2000), ..Default::default() }).await.unwrap();
    assert!(g.truncated);
    let bytes: usize = g.matches.iter().map(|m| m.text.len() + m.path.len()).sum();
    assert!(bytes <= 64 * 1024, "{bytes}");
    assert!(g.matches.len() < 2000);
    let err = wh::grep(&pool, &root, &GrepParams { pattern: "x".into(), repos: vec!["test-wh-no-such-repo".into()], ..Default::default() }).await.unwrap_err().to_string();
    assert!(err.contains("unknown repo"), "{err}");

    // ── tree + read on a directory ──
    let t = wh::tree(&pool, &root, SLUG, None, Some(2)).await.unwrap();
    let crates_dir = t.entries.iter().find(|e| e.path == "crates/wcore").expect("crates/wcore listed");
    assert_eq!(crates_dir.kind, "dir");
    assert!(crates_dir.files >= 6 && crates_dir.indexed_files >= 5, "{crates_dir:?}");
    let logo = t.entries.iter().find(|e| e.path == "logo.png").unwrap();
    assert_eq!(logo.skipped.as_deref(), Some("unsupported_type"));
    assert!(t.entries.iter().any(|e| e.path == "LICENSE" && e.skipped.as_deref() == Some("license_text")));
    assert!(t.skipped_by_reason.get("unsupported_type").is_some(), "{}", t.skipped_by_reason);
    let t = wh::tree(&pool, &root, SLUG, Some("crates/wcore/src"), Some(1)).await.unwrap();
    assert!(t.entries.iter().all(|e| e.path.starts_with("crates/wcore/src/") && e.kind == "file"));
    assert!(t.entries.iter().any(|e| e.path == "crates/wcore/src/net.rs" && e.language.as_deref() == Some("rust")));
    let err = wh::tree(&pool, &root, SLUG, Some("nope"), None).await.unwrap_err().to_string();
    assert!(err.contains("top-level") && err.contains("crates"), "{err}");
    let err = wh::read(&pool, &root, SLUG, "crates/wcore", None).await.unwrap_err().to_string();
    assert!(err.contains("directory") && err.contains("warehouse_tree"), "{err}");
    let err = wh::read(&pool, &root, SLUG, "", None).await.unwrap_err().to_string();
    assert!(err.contains("warehouse_tree"), "{err}");

    // ── card v2 ──
    let card = wh::card(&pool, SLUG).await.unwrap().unwrap();
    eprintln!("{card}");
    assert!(card.contains("## Modules"));
    assert!(card.contains("- `crates/wcore` — crate `widgetcore`"), "{card}");
    assert!(!card.contains("crates/widgetcore-tests"), "test packages are not modules / entry points / key files");
    let key = card.split("## Key files").nth(1).and_then(|s| s.split("\n## README").next()).unwrap_or("");
    assert!(!key.contains("tests/") && !key.contains("examples/"), "{key}");
    assert!(!key.contains("fn drop"), "{key}");
    assert!(key.contains("pub struct StreamableWidgetService"));
    let entries = card.split("## Entry points").nth(1).map(|s| s.split("\n## ").next().unwrap_or("")).unwrap_or("");
    assert!(!entries.contains("tests/"), "{entries}");

    wh::remove_repo(&pool, SLUG, false).await.unwrap();
}
