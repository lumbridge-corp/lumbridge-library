//! End-to-end warehouse test against the scratch DB (skips without TEST_DATABASE_URL):
//! add (local path) → index → embed → search → grep → symbols → read → card → resolve → query →
//! incremental re-sync → component license rules.

mod warehouse_common;

use lumbridge_library_core::warehouse::{self as wh, CodeSearchParams, ComponentFilter, GrepParams, IndexOpts, NewComponent, SymbolQuery};
use warehouse_common::*;

const LIB_RS: &str = r#"//! Rate limiting primitives.

/// A token bucket rate limiter: `capacity` tokens, refilled at `rate` tokens per second.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    rate: f64,
    last: f64,
}

impl TokenBucket {
    /// Create a full bucket.
    pub fn new(capacity: f64, rate: f64) -> Self {
        TokenBucket { capacity, tokens: capacity, rate, last: 0.0 }
    }

    /// Take one token if available at time `now` (seconds).
    pub fn try_acquire(&mut self, now: f64) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn refill(&mut self, now: f64) {
        let elapsed = (now - self.last).max(0.0);
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last = now;
    }
}

/// Clamp a duration in milliseconds into a sane retry window.
pub fn clamp_backoff(ms: u64) -> u64 {
    ms.clamp(10, 60_000)
}
"#;

const MAIN_RS: &str = r#"use fixture::TokenBucket;

fn main() {
    let mut b = TokenBucket::new(5.0, 1.0);
    for i in 0..10 {
        println!("{i}: {}", b.try_acquire(i as f64 * 0.1));
    }
}

/// Print a banner for the demo binary with the crate name and a short description of the demo.
fn banner() {
    println!("fixture demo: a token bucket that hands out tokens and refills them over time");
}
"#;

const LRU_PY: &str = r#"from collections import OrderedDict


class LRUCache:
    """Least-recently-used cache: evicts the oldest entry when full."""

    def __init__(self, capacity: int):
        self.capacity = capacity
        self.items = OrderedDict()

    def get(self, key):
        """Return the value and mark it most recently used."""
        if key not in self.items:
            return None
        self.items.move_to_end(key)
        return self.items[key]

    def put(self, key, value):
        """Insert, evicting the least recently used item when over capacity."""
        self.items[key] = value
        self.items.move_to_end(key)
        if len(self.items) > self.capacity:
            self.items.popitem(last=False)
"#;

const RETRY_TS: &str = r#"/** Sleep for `ms` milliseconds. */
export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Retry an async operation with exponential backoff and jitter.
 */
export async function retryWithBackoff<T>(op: () => Promise<T>, attempts = 5): Promise<T> {
  let delay = 100;
  for (let i = 0; ; i++) {
    try {
      return await op();
    } catch (e) {
      if (i + 1 >= attempts) throw e;
      await sleep(delay + Math.random() * delay);
      delay *= 2;
    }
  }
}
"#;

fn make_fixture(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    write(dir, "LICENSE", MIT);
    write(dir, "Cargo.toml", "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\ndescription = \"A tiny fixture crate for warehouse tests\"\nlicense = \"MIT\"\n");
    write(dir, "Cargo.lock", "# lock\n[[package]]\nname = \"fixture\"\n");
    write(dir, "README.md", "# fixture\n\n[![ci](https://x/badge.svg)](https://x)\n\nFixture repository exercising the warehouse indexer across Rust, Python and TypeScript.\n\n## Usage\n\nRun `cargo run` to watch the token bucket.\n");
    write(dir, "src/lib.rs", LIB_RS);
    write(dir, "src/main.rs", MAIN_RS);
    write(dir, "py/lru.py", LRU_PY);
    write(dir, "web/retry.ts", RETRY_TS);
    write(dir, "vendor/dep/junk.rs", "pub fn vendored_junk() {}\n");
    std::fs::write(dir.join("logo.png"), [0x89u8, b'P', b'N', b'G', 0, 1, 2, 3]).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial"]);
}

#[tokio::test]
async fn warehouse_end_to_end() {
    let Some(pool) = pool().await else {
        eprintln!("TEST_DATABASE_URL not set; skipping");
        return;
    };
    cleanup(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("library");
    let src = tmp.path().join("src/test-wh-fixture");
    make_fixture(&src);

    // ── add + index ──
    let repo = wh::add_own(&pool, &root, src.to_str().unwrap(), Some("test-wh-fixture"), &["test".into()]).await.unwrap();
    assert_eq!(repo.slug, "own/test-wh-fixture");
    assert_eq!(repo.tier, "own");
    assert_eq!(repo.license_spdx.as_deref(), Some("MIT"));
    assert_eq!(repo.copyleft, Some(false));
    assert!(repo.local_path.ends_with("warehouse/own/test-wh-fixture.git"));
    // Idempotent.
    let again = wh::add_own(&pool, &root, src.to_str().unwrap(), Some("test-wh-fixture"), &[]).await.unwrap();
    assert_eq!(again.id, repo.id);

    let slug = repo.slug.clone();
    let r = wh::sync_repo(&pool, &root, &slug, &IndexOpts::default()).await.unwrap();
    eprintln!("first sync: {r:?}");
    assert!(r.reindexed);
    assert_eq!(r.files_added, 6, "lib.rs main.rs lru.py retry.ts README.md Cargo.toml");
    assert_eq!(r.files_removed, 0);
    assert!(r.skipped_by_reason.contains_key("lockfile"));
    assert!(r.skipped_by_reason.contains_key("vendored_or_generated_dir"));
    assert!(r.skipped_by_reason.contains_key("license_text"));
    assert!(r.skipped_by_reason.contains_key("unsupported_type")); // logo.png
    assert!(r.symbols >= 10, "symbols {}", r.symbols);
    assert!(r.chunks_new >= 6);
    // No-op sync.
    let r2 = wh::sync_repo(&pool, &root, &slug, &IndexOpts::default()).await.unwrap();
    assert!(!r2.reindexed);

    let row = wh::get_repo(&pool, &slug).await.unwrap().unwrap();
    assert_eq!(row.description.as_deref(), Some("A tiny fixture crate for warehouse tests"));
    assert_eq!(row.last_indexed_commit, row.head_commit);
    assert!(row.languages.contains(&"rust".to_string()));
    assert_eq!(row.file_count, Some(6));

    // ── card ──
    let card = wh::card(&pool, &slug).await.unwrap().expect("card");
    eprintln!("{card}");
    assert!(card.contains("# own/test-wh-fixture"));
    assert!(card.contains("MIT"));
    assert!(card.contains("`src/lib.rs`"));
    assert!(card.contains("pub struct TokenBucket"));
    assert!(card.contains("## README excerpt"));

    // ── symbols ──
    let s = wh::symbols(&pool, &SymbolQuery { name: "TokenBucket".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
    assert_eq!(s[0].match_kind, "exact");
    assert!(s.iter().any(|h| h.kind == "struct" && h.path == "src/lib.rs" && h.doc.as_deref().unwrap_or("").contains("token bucket")));
    let s = wh::symbols(&pool, &SymbolQuery { name: "try_*".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
    assert!(s.iter().any(|h| h.name == "try_acquire" && h.kind == "method" && h.parent.as_deref() == Some("TokenBucket")));
    let s = wh::symbols(&pool, &SymbolQuery { name: "LRUCache.put".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].language.as_deref(), Some("python"));
    let s = wh::symbols(&pool, &SymbolQuery { name: "retryWithBackof".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
    assert!(s.iter().any(|h| h.name == "retryWithBackoff"), "prefix/fuzzy: {s:?}");
    let s = wh::symbols(&pool, &SymbolQuery { name: "TokenBuckt".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
    assert!(s.iter().any(|h| h.name == "TokenBucket" && h.match_kind == "fuzzy"), "fuzzy: {s:?}");

    // ── read ──
    let v = wh::read(&pool, &root, &slug, "src/lib.rs", Some((3, 4))).await.unwrap();
    assert_eq!(v.total_lines, LIB_RS.lines().count());
    assert!(v.content.starts_with("     3  /// A token bucket"));
    assert_eq!(v.license_spdx.as_deref(), Some("MIT"));
    let sv = wh::read_symbol(&pool, &root, &slug, "TokenBucket::try_acquire").await.unwrap();
    assert!(sv.view.content.contains("pub fn try_acquire"));
    assert!(sv.view.content.trim_end().ends_with('}'));
    assert!(wh::read(&pool, &root, &slug, "nope.rs", None).await.is_err());

    // ── grep ──
    let g = wh::grep(&pool, &root, &GrepParams { pattern: "try_acquire".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
    let paths: Vec<&str> = g.matches.iter().map(|m| m.path.as_str()).collect();
    assert!(paths.contains(&"src/lib.rs") && paths.contains(&"src/main.rs"), "{g:?}");
    assert!(!g.truncated);
    let g = wh::grep(
        &pool,
        &root,
        &GrepParams { pattern: "fn [a-z_]+\\(".into(), regex: true, path_glob: Some("*.rs".into()), repos: vec![slug.clone()], max_matches: Some(2), context: Some(1), ..Default::default() },
    )
    .await
    .unwrap();
    assert_eq!(g.matches.iter().filter(|m| !m.context).count(), 2);
    assert!(g.truncated);
    assert!(g.matches.iter().any(|m| m.context));
    assert!(g.matches.iter().all(|m| m.path.ends_with(".rs")));
    let g = wh::grep(&pool, &root, &GrepParams { pattern: "lrucache".into(), ignore_case: true, repos: vec!["test-wh-fixture".into()], ..Default::default() }).await.unwrap();
    assert!(g.matches.iter().any(|m| m.path == "py/lru.py" && m.line == 4));

    // ── resolve ──
    let refs = wh::resolve(&pool, "test-wh-fixture", 5).await.unwrap();
    assert_eq!(refs[0].slug, slug);
    assert_eq!(refs[0].id, "/warehouse/own/test-wh-fixture");
    assert!(refs[0].score >= 0.9);

    // ── embed + search + query (needs the model tunnels) ──
    let models = models();
    if let Some((embed, rerank)) = &models {
        let mid = model_id(&pool, embed).await;
        let n = wh::embed_pending(&pool, embed, mid, 100_000).await.unwrap();
        assert!(n >= r.chunks_new, "embedded {n}");
        let pending: i64 = sqlx::query_scalar("select count(*) from wh.chunks c join wh.repos r on r.id = c.repo_id where r.slug = $1 and c.embedding is null")
            .bind(&slug).fetch_one(&pool).await.unwrap();
        assert_eq!(pending, 0);

        let res = wh::search(&pool, embed, rerank, mid, &CodeSearchParams { query: "token bucket rate limiter refill".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
        assert!(!res.degraded);
        assert_eq!(res.hits[0].path, "src/lib.rs", "{:?}", res.hits.iter().map(|h| (&h.path, h.line_start, h.score)).collect::<Vec<_>>());
        // Semantic: no lexical overlap with "LRU" / "OrderedDict".
        let res = wh::search(&pool, embed, rerank, mid, &CodeSearchParams { query: "drop the stalest item once storage is full".into(), repos: vec![slug.clone()], limit: Some(3), ..Default::default() }).await.unwrap();
        assert_eq!(res.hits[0].path, "py/lru.py", "{:?}", res.hits.iter().map(|h| (&h.path, h.line_start, h.score)).collect::<Vec<_>>());
        assert!(res.hits.len() <= 3);
        // Filters.
        let res = wh::search(&pool, embed, rerank, mid, &CodeSearchParams { query: "retry with exponential backoff".into(), repos: vec![slug.clone()], languages: vec!["typescript".into()], ..Default::default() }).await.unwrap();
        assert!(!res.hits.is_empty() && res.hits.iter().all(|h| h.language.as_deref() == Some("typescript")));
        assert_eq!(res.hits[0].symbol.as_ref().map(|s| s.name.as_str()), Some("retryWithBackoff"), "{:?}", res.hits.iter().map(|h| (&h.symbol, h.line_start, h.line_end, h.score)).collect::<Vec<_>>());
        let res = wh::search(&pool, embed, rerank, mid, &CodeSearchParams { query: "token".into(), repos: vec![slug.clone()], path_prefix: Some("src/".into()), ..Default::default() }).await.unwrap();
        assert!(res.hits.iter().all(|h| h.path.starts_with("src/")));

        let ans = wh::query(&pool, embed, rerank, mid, "how do I limit request rate?", Some("test-wh-fixture"), Some(1500)).await.unwrap();
        eprintln!("{}", ans.context_md);
        assert_eq!(ans.resolved.as_deref(), Some(slug.as_str()));
        assert!(ans.context_md.starts_with("# own/test-wh-fixture"));
        assert!(ans.context_md.contains("[1] own/test-wh-fixture:"));
        assert!(ans.context_md.contains("— MIT"));
        assert!(!ans.citations.is_empty());
    } else {
        eprintln!("TEST_EMBED_URL/TEST_RERANK_URL not set; skipping semantic search");
    }

    // ── incremental re-sync ──
    let main_v2 = MAIN_RS.replace("for i in 0..10", "for i in 0..20");
    write(&src, "src/main.rs", &main_v2);
    write(&src, "py/extra.py", "def double(x):\n    \"\"\"Double a number for the incremental indexing test.\"\"\"\n    return x * 2\n");
    std::fs::remove_file(src.join("web/retry.ts")).unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "second"]);
    let lib_chunks_before: Vec<i64> = sqlx::query_scalar("select c.id from wh.chunks c join wh.repos r on r.id = c.repo_id where r.slug = $1 and c.path = 'src/lib.rs' order by 1")
        .bind(&slug).fetch_all(&pool).await.unwrap();
    let r3 = wh::sync_repo(&pool, &root, &slug, &IndexOpts::default()).await.unwrap();
    eprintln!("incremental: {r3:?}");
    assert!(r3.reindexed);
    assert_eq!((r3.files_added, r3.files_changed, r3.files_removed, r3.files_unchanged), (1, 1, 1, 4));
    let lib_chunks_after: Vec<i64> = sqlx::query_scalar("select c.id from wh.chunks c join wh.repos r on r.id = c.repo_id where r.slug = $1 and c.path = 'src/lib.rs' order by 1")
        .bind(&slug).fetch_all(&pool).await.unwrap();
    assert_eq!(lib_chunks_before, lib_chunks_after, "unchanged file untouched");
    let ts: i64 = sqlx::query_scalar("select count(*) from wh.files f join wh.repos r on r.id = f.repo_id where r.slug = $1 and f.path = 'web/retry.ts'")
        .bind(&slug).fetch_one(&pool).await.unwrap();
    assert_eq!(ts, 0);
    if models.is_some() {
        // `banner()` in main.rs did not change: its chunk keeps its embedding.
        assert!(r3.chunks_reused >= 1, "reused {}", r3.chunks_reused);
        let pending: i64 = sqlx::query_scalar("select count(*) from wh.chunks c join wh.repos r on r.id = c.repo_id where r.slug = $1 and c.embedding is null")
            .bind(&slug).fetch_one(&pool).await.unwrap();
        assert_eq!(pending as usize, r3.chunks_new - r3.chunks_reused);
    }
    assert!(wh::symbols(&pool, &SymbolQuery { name: "double".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap().iter().any(|h| h.path == "py/extra.py"));

    // ── components ──
    let gpl_src = tmp.path().join("src/test-wh-gpl");
    std::fs::create_dir_all(&gpl_src).unwrap();
    git(&gpl_src, &["init", "-q", "-b", "main"]);
    write(&gpl_src, "COPYING", GPL);
    write(&gpl_src, "parse.c", "/* Parse a line. */\nint parse_line(const char *s) { return s[0]; }\n");
    git(&gpl_src, &["add", "-A"]);
    git(&gpl_src, &["commit", "-q", "-m", "gpl"]);
    let gpl = wh::add_own(&pool, &root, gpl_src.to_str().unwrap(), Some("test-wh-gpl"), &[]).await.unwrap();
    assert_eq!(gpl.license_spdx.as_deref(), Some("GPL-3.0"));
    assert_eq!(gpl.copyleft, Some(true));

    let base = NewComponent {
        slug: "test-wh-distill-gpl".into(),
        summary: "Line parser".into(),
        languages: vec!["c".into()],
        origin: "distilled".into(),
        source_repo: Some(gpl.slug.clone()),
        source_ref: Some(format!("{}:parse.c", gpl.head_commit.clone().unwrap())),
        license_spdx: "GPL-3.0".into(),
        files: vec![("parse.c".into(), "int parse_line(const char *s) { return s[0]; }\n".into())],
        created_by: "claude-code".into(),
        ..Default::default()
    };
    let err = wh::create_component(&pool, &root, base.clone()).await.unwrap_err().to_string();
    assert!(err.contains("copyleft") && err.contains("cleanroom"), "{err}");
    assert!(!root.join("warehouse/components/test-wh-distill-gpl").exists());

    let clean = NewComponent {
        slug: "test-wh-clean".into(),
        summary: "Line parser, reimplemented from a behaviour spec".into(),
        origin: "cleanroom".into(),
        license_spdx: "MIT".into(),
        tags: vec!["parsing".into()],
        files: vec![("src/lib.rs".into(), "/// Returns the first byte of a line, or 0.\npub fn parse_line(s: &[u8]) -> u8 {\n    s.first().copied().unwrap_or(0)\n}\n".into())],
        ..base.clone()
    };
    let err = wh::create_component(&pool, &root, NewComponent { spec_md: None, ..clean.clone() }).await.unwrap_err().to_string();
    assert!(err.contains("spec"), "{err}");
    let comp = wh::create_component(
        &pool,
        &root,
        NewComponent { spec_md: Some("# parse_line\n\nGiven a line of bytes, return its first byte; 0 for an empty line.\n".into()), ..clean.clone() },
    )
    .await
    .unwrap();
    assert_eq!(comp.status, "draft");
    assert_eq!(comp.spec_path.as_deref(), Some("SPEC.md"));
    assert_eq!(comp.repo_slug.as_deref(), Some("components/test-wh-clean"));
    let cdir = root.join("warehouse/components/test-wh-clean");
    assert!(cdir.join("SPEC.md").exists() && cdir.join("component.toml").exists() && cdir.join("README.md").exists());
    let toml_text = std::fs::read_to_string(cdir.join("component.toml")).unwrap();
    assert!(toml_text.contains("origin = \"cleanroom\"") && toml_text.contains("spec = \"SPEC.md\""), "{toml_text}");
    let s = wh::symbols(&pool, &SymbolQuery { name: "parse_line".into(), repos: vec!["components/test-wh-clean".into()], ..Default::default() }).await.unwrap();
    assert!(s.iter().any(|h| h.path == "src/lib.rs"), "component indexed: {s:?}");
    assert!(wh::card(&pool, "test-wh-clean").await.unwrap().is_some());

    // Distilled from the MIT fixture: allowed, with NOTICE + upstream LICENSE; wrong license refused.
    let dist = NewComponent {
        slug: "test-wh-bucket".into(),
        summary: "Token bucket rate limiter".into(),
        languages: vec!["rust".into()],
        origin: "distilled".into(),
        source_repo: Some(slug.clone()),
        source_ref: Some("HEAD:src/lib.rs".into()),
        license_spdx: "Apache-2.0".into(),
        tags: vec!["rate-limit".into()],
        files: vec![("src/lib.rs".into(), LIB_RS.into())],
        spec_md: None,
        created_by: "claude-code".into(),
    };
    let err = wh::create_component(&pool, &root, dist.clone()).await.unwrap_err().to_string();
    assert!(err.contains("not compatible"), "{err}");
    let comp = wh::create_component(&pool, &root, NewComponent { license_spdx: "MIT".into(), ..dist }).await.unwrap();
    assert_eq!(comp.origin, "distilled");
    let bdir = root.join("warehouse/components/test-wh-bucket");
    let notice = std::fs::read_to_string(bdir.join("NOTICE")).unwrap();
    assert!(notice.contains("own/test-wh-fixture") && notice.contains("MIT"));
    assert!(bdir.join("LICENSE").exists());

    let comp = wh::approve_component(&pool, "test-wh-bucket").await.unwrap();
    assert_eq!(comp.status, "approved");
    assert!(comp.approved_at.is_some());
    let u = wh::record_usage(&pool, "test-wh-bucket", "me/racks", Some("abc123"), "claude-code").await.unwrap();
    assert_eq!(u.component, "test-wh-bucket");
    let list = wh::list_components(&pool, &ComponentFilter { tag: Some("rate-limit".into()), ..Default::default() }).await.unwrap();
    assert!(list.iter().any(|c| c.slug == "test-wh-bucket" && c.uses == 1));
    let refs = wh::resolve(&pool, "test-wh-bucket", 5).await.unwrap();
    assert_eq!(refs[0].slug, "components/test-wh-bucket");
    assert_eq!(refs[0].status.as_deref(), Some("approved"));
    wh::retire_component(&pool, "test-wh-clean").await.unwrap();
    let list = wh::list_components(&pool, &ComponentFilter::default()).await.unwrap();
    assert!(!list.iter().any(|c| c.slug == "test-wh-clean"));

    cleanup(&pool).await;
    assert!(wh::get_repo(&pool, &slug).await.unwrap().is_none());
}
