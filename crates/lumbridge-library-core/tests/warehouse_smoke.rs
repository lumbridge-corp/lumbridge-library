//! Real-repo smoke run (ignored by default):
//!   WH_SMOKE_URL=https://github.com/BurntSushi/memchr TEST_DATABASE_URL=... [TEST_EMBED_URL=... TEST_RERANK_URL=...] \
//!     cargo test -p lumbridge-library-core --test warehouse_smoke -- --ignored --nocapture
//! Registered as `own/test-wh-<name>` so it never collides with real rows; removed afterwards.

mod warehouse_common;

use lumbridge_library_core::warehouse::{self as wh, CodeSearchParams, GrepParams, IndexOpts, SymbolQuery};
use std::time::Instant;
use warehouse_common::*;

#[tokio::test]
#[ignore]
async fn warehouse_smoke_real_repo() {
    let (Some(pool), Ok(url)) = (pool().await, std::env::var("WH_SMOKE_URL")) else {
        eprintln!("TEST_DATABASE_URL / WH_SMOKE_URL not set; skipping");
        return;
    };
    let (_, name) = wh::owner_and_name(&url).unwrap();
    let name = format!("test-wh-{name}");
    let slug = format!("own/{name}");
    let _ = wh::remove_repo(&pool, &slug, false).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    let t = Instant::now();
    let repo = wh::add_own(&pool, &root, &url, Some(&name), &[]).await.unwrap();
    eprintln!("clone+add: {:?}  license={:?} conf={:?}", t.elapsed(), repo.license_spdx, repo.license_confidence);

    let t = Instant::now();
    let r = wh::sync_repo(&pool, &root, &slug, &IndexOpts::default()).await.unwrap();
    eprintln!("index: {:?}\n{}", t.elapsed(), serde_json::to_string_pretty(&r).unwrap());

    let t = Instant::now();
    let r2 = wh::sync_repo(&pool, &root, &slug, &IndexOpts { force: true, ..Default::default() }).await.unwrap();
    eprintln!("forced re-sync (all unchanged): {:?} unchanged={} chunks_new={}", t.elapsed(), r2.files_unchanged, r2.chunks_new);

    if let Some((embed, rerank)) = models() {
        let mid = model_id(&pool, &embed).await;
        let t = Instant::now();
        let n = wh::embed_pending(&pool, &embed, mid, 1_000_000).await.unwrap();
        let el = t.elapsed();
        eprintln!("embed: {n} chunks in {el:?} ({:.1}/s)", n as f64 / el.as_secs_f64());
        for q in ["find the first occurrence of a byte in a slice using SIMD", "reverse search from the end of the haystack"] {
            let t = Instant::now();
            let res = wh::search(&pool, &embed, &rerank, mid, &CodeSearchParams { query: q.into(), repos: vec![slug.clone()], limit: Some(5), ..Default::default() }).await.unwrap();
            eprintln!("search {q:?}: {:?} degraded={}", t.elapsed(), res.degraded);
            for h in &res.hits {
                eprintln!("  {:.3} {}:{}-{} {:?}", h.score, h.path, h.line_start, h.line_end, h.symbol.as_ref().map(|s| &s.name));
            }
        }
    }
    let t = Instant::now();
    let g = wh::grep(&pool, &root, &GrepParams { pattern: "unsafe fn".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
    eprintln!("grep 'unsafe fn': {} matches truncated={} in {:?}", g.matches.len(), g.truncated, t.elapsed());
    let t = Instant::now();
    let s = wh::symbols(&pool, &SymbolQuery { name: "memchr".into(), repos: vec![slug.clone()], ..Default::default() }).await.unwrap();
    eprintln!("symbols 'memchr': {} hits in {:?}; first {:?}", s.len(), t.elapsed(), s.first().map(|h| (&h.path, h.line_start, &h.signature)));
    let card = wh::card(&pool, &slug).await.unwrap().unwrap_or_default();
    eprintln!("── card ({} chars) ──\n{card}", card.len());

    wh::remove_repo(&pool, &slug, false).await.unwrap();
}
