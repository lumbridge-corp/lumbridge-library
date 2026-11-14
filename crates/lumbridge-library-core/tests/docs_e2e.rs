//! End-to-end: temp git vault → sync → embed → search (semantic + lexical) → resolve → query →
//! write / refuse the owner → facts → lint. Needs `TEST_DATABASE_URL` (scratch DB; every `kb` table is
//! truncated, other schemas are left alone because other tests share the database) plus
//! `TEST_EMBED_URL` / `TEST_RERANK_URL`; skips when any of the three is unset.

use lumbridge_library_core::config::{EmbedConfig, RerankConfig};
use lumbridge_library_core::docs::{self, Clip, FactAction, NewFact, SearchParams, WriteDoc};
use lumbridge_library_core::models::{Embedder, Reranker};
use lumbridge_library_core::{db, rank};
use std::path::Path;
use std::process::Command;

fn embed_cfg(url: &str) -> EmbedConfig {
    EmbedConfig {
        base_url: url.to_string(),
        model: "embed".into(),
        dims: 2048,
        query_prefix: "query: ".into(),
        passage_prefix: "passage: ".into(),
        batch: 32,
    }
}

fn put(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn section(title: &str, topic: &str, n: usize) -> String {
    let sentences: Vec<String> =
        (0..n).map(|i| format!("Paragraph {i} about {topic}: measurement {i} was recorded in the lab notebook.")).collect();
    format!("## {title}\n\n{}\n\n", sentences.join(" "))
}

fn build_vault(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    let mut gb10 = String::from(
        "---\ntitle: NVIDIA GB10\nauthor: agent\nstatus: reviewed\ncategories:\n  - \"[[GPUs]]\"\ncreated: 2026-10-01\nsource: https://example.invalid/gb10\nentity:\n  slug: nvidia-gb10\n  kind: chip\n  aliases: [GB10]\n---\n# NVIDIA GB10\n\n",
    );
    gb10.push_str("## Memory\n\nThe GB10 superchip has 273 GB/s of memory bandwidth from LPDDR5X.\n\n");
    gb10.push_str(&section("Compute", "tensor cores", 25));
    gb10.push_str(&section("Networking", "ConnectX ports", 25));
    gb10.push_str(&section("Thermals", "fan curves", 25));
    put(root, "References/NVIDIA GB10.md", &gb10);
    put(root, "Notes/Spark memory.md", "The DGX Spark ships with 128 GB of unified memory; see [[NVIDIA GB10|the chip]] and [[GB10#Memory]].\n");
    put(
        root,
        "Thoughts/Sourdough.md",
        "---\ncategories: [Kitchen]\n---\nMy sourdough starter needs feeding with flour and water every twelve hours; the loaf rises overnight in the fridge.\n",
    );
    put(
        root,
        "Wiki/Errors.md",
        "---\ntitle: Error codes\ncategories: [\"[[Ops]]\"]\n---\n# Error codes\n\nThe fan controller reports code ZQX7731 when the chassis overheats. See [[Nonexistent Page]].\n\n```\n[[not a link]]\n```\n",
    );
    put(root, "Wiki/Old.md", "---\ntitle: Old summary\nupdated: 2026-01-01\ncategories: [GPUs]\n---\nSummary of [[NVIDIA GB10]].\n");
    put(root, "Clippings/Broken.md", "---\ntitle: [oops\n---\nbody text of a clipping with broken frontmatter\n");
    put(root, "Templates/Note.md", "---\ncategories: []\n---\n# {{title}}\n");
    put(root, "Daily/2026-11-07.md", "Thinking about [[Spark memory]] today.\n");
    put(root, "index.md", "# Index\n\n- [[NVIDIA GB10]]\n");
    put(root, "Attachments/ignored.md", "not indexed\n");
    put(root, ".obsidian/workspace.md", "not indexed\n");
    put(root, "Wiki/Owner page.md", "---\nauthor: owner\n---\nThe owner wrote this.\n");
    // 950DT-style ranking case: the answer chunk never names the chip (only the title does), a sibling
    // part has the same spec lines, and a wiki page repeats the question's words without the answer.
    let mut dt = String::from(
        "---\ntitle: Huawei Ascend 950DT\nauthor: claude\naliases: [Ascend 950DT]\nentity:\n  slug: huawei-ascend-950dt\n  kind: chip\n---\n",
    );
    dt.push_str(&section("History", "the Da Vinci roadmap", 25));
    dt.push_str("## Key specs\n\n- **Memory:** 144 GB HiZQ 2.0\n- **Memory bandwidth:** 4 TB/s\n- **Interconnect:** UnifiedBus 2 TB/s\n");
    put(root, "References/Huawei Ascend 950DT.md", &dt);
    let mut pr = String::from("---\ntitle: Huawei Ascend 950PR\nauthor: claude\naliases: [Ascend 950PR]\n---\n");
    pr.push_str(&section("History", "the inference roadmap", 25));
    pr.push_str("## Key specs\n\n- **Memory:** 128 GB HiBL 1.0\n- **Memory bandwidth:** 1.6 TB/s\n- **Interconnect:** UnifiedBus 2 TB/s\n");
    put(root, "References/Huawei Ascend 950PR.md", &pr);
    // Look-alike name guard: the vault has Gaudi 3, questions ask about Gaudi 5.
    let mut gaudi = String::from(
        "---\ntitle: Intel Gaudi 3\nauthor: claude\naliases: [Gaudi 3]\nentity:\n  slug: intel-gaudi-3\n  kind: chip\n  aliases: [Gaudi 3]\n---\n",
    );
    gaudi.push_str(&section("History", "the Habana roadmap", 25));
    gaudi.push_str("## Key specs\n\n- **Memory:** 128 GB HBM2e\n- **Memory bandwidth:** 3.7 TB/s\n- **Power:** 900 W\n");
    put(root, "References/Intel Gaudi 3.md", &gaudi);
    put(
        root,
        "Wiki/Ascend FAQ.md",
        "---\ntitle: Ascend FAQ\n---\nPeople keep asking about Ascend 950DT memory bandwidth, Ascend 950DT memory bandwidth per watt and Ascend 950DT memory bandwidth per dollar. This page collects the questions only.\n",
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "seed"]);
}

#[tokio::test]
async fn docs_end_to_end() {
    let (Ok(url), Ok(embed_url), Ok(rerank_url)) =
        (std::env::var("TEST_DATABASE_URL"), std::env::var("TEST_EMBED_URL"), std::env::var("TEST_RERANK_URL"))
    else {
        eprintln!("TEST_DATABASE_URL / TEST_EMBED_URL / TEST_RERANK_URL unset; skipping docs e2e");
        return;
    };
    let pool = db::connect(&url, 4).await.unwrap();

    // As deploy/db-setup.sql does: `lib` must exist first so `_sqlx_migrations` lands there (the first
    // existing schema on search_path; `extensions` is not writable). Other branches testing against
    // the same database may have applied migrations this tree lacks, hence ignore_missing. Only `kb`
    // is reset: warehouse tests use `wh`/`lib` concurrently.
    sqlx::query("create schema if not exists lib").execute(&pool).await.unwrap();
    let mut migrator = sqlx::migrate!("../../migrations");
    migrator.set_ignore_missing(true);
    migrator.run(&pool).await.unwrap();
    sqlx::query("truncate kb.documents, kb.chunks, kb.links, kb.entities, kb.facts, kb.observations, kb.sources restart identity cascade")
        .execute(&pool)
        .await
        .unwrap();

    let embedder = Embedder::new(embed_cfg(&embed_url));
    let reranker = Reranker::new(RerankConfig { base_url: rerank_url, model: "rerank".into() });
    let model_id = db::register_embed_model(&pool, &embedder.identity().await.unwrap()).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_vault(root);

    // ── sync ──
    let r = docs::sync_vault(&pool, root).await.unwrap();
    println!("sync 1: {r:?}");
    assert_eq!(r.scanned, 14);
    assert_eq!(r.added, 14);
    assert_eq!(r.parse_errors.len(), 1);
    assert_eq!(r.parse_errors[0].path, "Clippings/Broken.md");
    assert!(r.chunks_new >= 10);
    let r2 = docs::sync_vault(&pool, root).await.unwrap();
    assert_eq!((r2.unchanged, r2.added, r2.updated, r2.deleted, r2.chunks_new), (14, 0, 0, 0, 0));

    let gb10_chunks: i64 = sqlx::query_scalar(
        "select count(*) from kb.chunks c join kb.documents d on d.id = c.doc_id where d.path = 'References/NVIDIA GB10.md'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(gb10_chunks >= 3, "GB10 should span several chunks, got {gb10_chunks}");

    // Layers, authors, defaults.
    let row: (String, String, String, Vec<String>) =
        sqlx::query_as("select layer, author, title, categories from kb.documents where path = 'Notes/Spark memory.md'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, ("note".into(), "owner".into(), "Spark memory".into(), vec![]));
    let row: (String, String, Vec<String>) =
        sqlx::query_as("select layer, status, categories from kb.documents where path = 'References/NVIDIA GB10.md'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, ("reference".into(), "reviewed".into(), vec!["GPUs".to_string()]));

    // ── links ──
    let spark = docs::read(&pool, "Notes/Spark memory", None).await.unwrap().unwrap();
    assert_eq!(spark.links.len(), 2);
    for l in &spark.links {
        assert_eq!(l.path.as_deref(), Some("References/NVIDIA GB10.md"), "{l:?}");
        assert_eq!(l.entity.as_deref(), Some("nvidia-gb10"));
    }
    assert_eq!(spark.links[1].dst_text, "GB10#Memory");
    // Chunks carry title + aliases (full-text weight A) and body line ranges.
    let (ctitle, ls, le): (String, Option<i32>, Option<i32>) = sqlx::query_as(
        "select c.title, c.line_start, c.line_end from kb.chunks c join kb.documents d on d.id = c.doc_id
         where d.path = 'Notes/Spark memory.md'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((ctitle.as_str(), ls, le), ("Spark memory", Some(1), Some(1)));
    let gb10 = docs::read(&pool, "/docs/entity/nvidia-gb10", Some((1, 3))).await.unwrap().unwrap();
    assert_eq!(gb10.path, "References/NVIDIA GB10.md");
    assert_eq!(gb10.lines, Some((1, 3)));
    assert!(gb10.body.starts_with("# NVIDIA GB10"));
    let back: Vec<&str> = gb10.backlinks.iter().filter_map(|b| b.path.as_deref()).collect();
    assert!(back.contains(&"Notes/Spark memory.md") && back.contains(&"index.md") && back.contains(&"Wiki/Old.md"), "{back:?}");
    let errors = docs::read(&pool, "Error codes", None).await.unwrap().unwrap();
    assert_eq!(errors.links.len(), 1, "code-fenced link must be ignored");
    assert!(errors.links[0].path.is_none());

    // ── embed ──
    let total: i64 = sqlx::query_scalar("select count(*) from kb.chunks").fetch_one(&pool).await.unwrap();
    let n = docs::embed_pending(&pool, &embedder, model_id, 10_000).await.unwrap();
    assert_eq!(n as i64, total);
    assert_eq!(docs::embed_pending(&pool, &embedder, model_id, 10_000).await.unwrap(), 0);
    let down = Embedder::new(embed_cfg("http://127.0.0.1:9/v1"));

    // Changing one section re-embeds only that chunk.
    let mut text = std::fs::read_to_string(root.join("References/NVIDIA GB10.md")).unwrap();
    text.push_str(&section("Pricing", "list prices", 25));
    put(root, "References/NVIDIA GB10.md", &text);
    git(root, &["commit", "-qam", "pricing"]);
    let r3 = docs::sync_vault(&pool, root).await.unwrap();
    println!("sync 3: {r3:?}");
    assert_eq!(r3.updated, 1);
    assert!(r3.chunks_reused >= 3, "{r3:?}");
    assert!((1..=2).contains(&r3.chunks_new), "{r3:?}");
    assert!(docs::embed_pending(&pool, &down, model_id, 100).await.is_err(), "embed server down must error");
    let pending: i64 = sqlx::query_scalar("select count(*) from kb.chunks where embedding is null").fetch_one(&pool).await.unwrap();
    assert_eq!(pending as usize, r3.chunks_new, "rows stay pending when the embedder is down");
    assert_eq!(docs::embed_pending(&pool, &embedder, model_id, 100).await.unwrap(), r3.chunks_new);

    // ── search ──
    let sem = docs::search(&pool, &embedder, &reranker, model_id, &SearchParams::new("how frequently to nourish a fermented yeast culture"))
        .await
        .unwrap();
    println!("semantic: {:?}", sem.hits.iter().map(|h| (&h.path, h.score, h.fts_rank, h.vec_rank)).collect::<Vec<_>>());
    assert!(sem.degraded.is_empty(), "{:?}", sem.degraded);
    assert_eq!(sem.hits[0].path, "Thoughts/Sourdough.md");
    assert_eq!(sem.hits[0].fts_rank, None);
    assert!(sem.hits[0].vec_rank.is_some());
    assert!(!sem.no_good_match && sem.note.is_none());
    assert!(sem.hits.iter().all(|h| h.rerank_prob.is_some_and(|p| p >= docs::search::MIN_RERANK_PROB)), "floor applies to every hit");

    let lex = docs::search(&pool, &embedder, &reranker, model_id, &SearchParams::new("ZQX7731")).await.unwrap();
    println!("lexical: {:?}", lex.hits.iter().map(|h| (&h.path, h.score, h.fts_rank, h.vec_rank)).collect::<Vec<_>>());
    assert_eq!(lex.hits[0].path, "Wiki/Errors.md");
    assert_eq!(lex.hits[0].fts_rank, Some(1));

    // Title-aware ranking: the spec chunk only says "Memory bandwidth: 4 TB/s"; the chip is named by
    // the title (in the tsvector at weight A and in the reranker text) and the question names the
    // entity, so its reference chunks are pulled in too.
    let dt = docs::search(&pool, &embedder, &reranker, model_id, &SearchParams::new("Ascend 950DT memory bandwidth")).await.unwrap();
    println!("950DT: {:?}", dt.hits.iter().map(|h| (&h.path, &h.heading, h.score, h.rerank_prob, h.fts_rank, h.vec_rank)).collect::<Vec<_>>());
    let top = &dt.hits[0];
    assert_eq!((top.path.as_str(), top.heading.as_str()), ("References/Huawei Ascend 950DT.md", "Key specs"));
    assert!(top.content.contains("4 TB/s") && !top.content.contains("950DT"));
    assert!(top.rerank_prob.unwrap() > 0.5, "{top:?}");
    // Hit → read range: the line numbers point at the chunk in docs_read's numbering.
    let (a, b) = (top.line_start.unwrap() as usize, top.line_end.unwrap() as usize);
    let part = docs::read(&pool, &top.path, Some((a, b))).await.unwrap().unwrap();
    assert!(part.body.starts_with("## Key specs") && part.body.contains("**Memory bandwidth:** 4 TB/s"), "{}", part.body);
    let ans = docs::query(&pool, &embedder, &reranker, model_id, "Ascend 950DT memory bandwidth", None, Some(400)).await.unwrap();
    assert!(ans.context_md.starts_with("[1] Huawei Ascend 950DT — Key specs (References/Huawei Ascend 950DT.md)"), "{}", ans.context_md);
    assert!(dt.name_mismatch.is_none() && dt.note.is_none() && ans.name_mismatch.is_none(), "{:?}", dt.note);

    // Look-alike names: Gaudi 5 is not in the vault, Gaudi 3 is. The hits stay (the reranker likes
    // the Gaudi 3 page) but they are flagged, and query's context opens with the warning.
    let g5 = docs::search(&pool, &embedder, &reranker, model_id, &SearchParams::new("Intel Gaudi 5 memory bandwidth")).await.unwrap();
    println!("gaudi 5: {:?} {:?}", g5.note, g5.hits.iter().map(|h| (&h.path, h.rerank_prob)).collect::<Vec<_>>());
    assert!(!g5.hits.is_empty() && !g5.no_good_match);
    let m = g5.name_mismatch.as_ref().expect("Gaudi 5 must be flagged");
    assert_eq!(m.asked, "Intel Gaudi 5");
    assert_eq!(m.closest[0].path, "References/Intel Gaudi 3.md");
    assert!(g5.note.as_deref().unwrap().starts_with("The vault has no page for Intel Gaudi 5; closest: Intel Gaudi 3 (References/Intel Gaudi 3.md)."));
    let ans = docs::query(&pool, &embedder, &reranker, model_id, "Intel Gaudi 5 memory bandwidth", None, None).await.unwrap();
    assert!(ans.context_md.starts_with("> **Warning:** The vault has no page for Intel Gaudi 5;"), "{}", ans.context_md);
    assert!(ans.name_mismatch.is_some() && !ans.citations.is_empty());
    // The exact entity, spelled any way: no warning.
    for q in ["Intel Gaudi 3 memory bandwidth", "gaudi3 memory bandwidth", "GB10 memory bandwidth 273 GB/s"] {
        let r = docs::search(&pool, &embedder, &reranker, model_id, &SearchParams::new(q)).await.unwrap();
        assert!(r.name_mismatch.is_none(), "{q}: {:?}", r.note);
        assert!(!r.hits.is_empty(), "{q}");
    }
    let ans = docs::query(&pool, &embedder, &reranker, model_id, "Intel Gaudi 3 memory bandwidth", None, None).await.unwrap();
    assert!(ans.context_md.starts_with("[1] Intel Gaudi 3"), "{}", ans.context_md);
    assert_eq!((ans.citations[0].line_start, ans.citations[0].line_end), (top.line_start, top.line_end));

    // Nothing in the vault answers these: no junk at 1.0, an explicit no_good_match instead.
    for q in ["What is the boiling point of mercury?", "Sam's verdict on the Cudy router"] {
        let r = docs::search(&pool, &embedder, &reranker, model_id, &SearchParams::new(q)).await.unwrap();
        println!("unanswerable {q:?}: {r:?}");
        assert!(r.no_good_match, "{q}: {:?}", r.hits.iter().map(|h| (&h.path, h.rerank_prob)).collect::<Vec<_>>());
        assert!(r.hits.is_empty());
        assert!(r.note.as_deref().is_some_and(|n| n.contains("relevance floor") || n.contains("nothing")), "{:?}", r.note);
    }
    let ans = docs::query(&pool, &embedder, &reranker, model_id, "What is the boiling point of mercury?", None, None).await.unwrap();
    assert!(ans.no_good_match && ans.citations.is_empty() && ans.note.is_some());
    assert!(ans.context_md.contains("No good match"), "{}", ans.context_md);

    // Unknown layer names are an error that lists the valid ones, not an empty result.
    let mut bad = SearchParams::new("GB10");
    bad.layers = Some(vec!["refernce".into()]);
    let err = docs::search(&pool, &embedder, &reranker, model_id, &bad).await.unwrap_err().to_string();
    assert!(err.contains("refernce") && err.contains("reference"), "{err}");

    // Runaway queries are cut: a 5,000-word question answers quickly.
    let essay = (0..5000).map(|i| format!("sourdough{} starter flour water", i % 900)).collect::<Vec<_>>().join(" ");
    let t = std::time::Instant::now();
    let long = docs::search(&pool, &embedder, &reranker, model_id, &SearchParams::new(essay)).await.unwrap();
    println!("5,000-word query: {:?} {:?}", t.elapsed(), long.degraded);
    assert!(t.elapsed() < std::time::Duration::from_secs(30));
    assert!(long.degraded.iter().any(|d| d.contains("query cut")));

    // At most two chunks per document; templates excluded; filters apply.
    let many = docs::search(&pool, &embedder, &reranker, model_id, &SearchParams::new("measurement recorded in the lab notebook"))
        .await
        .unwrap();
    let gb10_hits = many.hits.iter().filter(|h| h.path == "References/NVIDIA GB10.md").count();
    assert!((1..=2).contains(&gb10_hits));
    assert!(many.hits.iter().all(|h| h.layer != "template"));
    let mut only_wiki = SearchParams::new("GB10 memory bandwidth");
    only_wiki.layers = Some(vec!["wiki".into()]);
    let w = docs::search(&pool, &embedder, &reranker, model_id, &only_wiki).await.unwrap();
    assert!(w.hits.iter().all(|h| h.layer == "wiki"));

    // Degraded: embedder down → FTS only, still answers.
    let deg = docs::search(&pool, &down, &reranker, model_id, &SearchParams::new("ZQX7731")).await.unwrap();
    assert!(!deg.degraded.is_empty());
    assert_eq!(deg.hits[0].path, "Wiki/Errors.md");
    let down_rr = Reranker::new(RerankConfig { base_url: "http://127.0.0.1:9/v1".into(), model: "rerank".into() });
    let deg2 = docs::search(&pool, &embedder, &down_rr, model_id, &SearchParams::new("ZQX7731")).await.unwrap();
    assert!(deg2.degraded.iter().any(|d| d.contains("reranker")));
    assert_eq!(deg2.hits[0].path, "Wiki/Errors.md");

    // ── resolve ──
    let res = docs::resolve(&pool, "gb10", 10).await.unwrap();
    println!("resolve gb10: {:?}", res.iter().map(|r| (&r.id, r.score)).collect::<Vec<_>>());
    let ids: Vec<&str> = res.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"/docs/entity/nvidia-gb10"));
    assert!(ids.contains(&"/docs/References/NVIDIA GB10.md"));
    assert!(ids[..2].contains(&"/docs/entity/nvidia-gb10"));
    let res = docs::resolve(&pool, "gpus", 10).await.unwrap();
    assert_eq!(res[0].id, "/docs/category/GPUs");
    assert!(res[0].chunks > 0);
    let res = docs::resolve(&pool, "Sourdough", 5).await.unwrap();
    assert_eq!(res[0].id, "/docs/Thoughts/Sourdough.md");
    assert!(res[0].snippet.starts_with("My sourdough starter"));

    // ── facts ──
    let gb = || NewFact {
        entity: "GB10".into(),
        attribute: "memory_bandwidth".into(),
        value_num: Some(273.0),
        unit: Some("GB/s".into()),
        valid_from: chrono::NaiveDate::from_ymd_opt(2025, 1, 1),
        source_uri: Some("https://example.invalid/gb10".into()),
        verified_by: Some("vendor".into()),
        ..Default::default()
    };
    let a = docs::add_fact(&pool, gb()).await.unwrap();
    assert_eq!(a.action, FactAction::Added);
    let b = docs::add_fact(&pool, gb()).await.unwrap();
    assert_eq!((b.action, b.id), (FactAction::Noop, a.id));
    let mut newer = gb();
    newer.value_num = Some(301.0);
    newer.valid_from = chrono::NaiveDate::from_ymd_opt(2026, 6, 1);
    let c = docs::add_fact(&pool, newer).await.unwrap();
    assert_eq!(c.action, FactAction::Updated);
    assert_eq!(c.superseded, Some(a.id));
    let cur = docs::facts(&pool, "nvidia-gb10", None, None, false).await.unwrap();
    assert_eq!(cur.len(), 1);
    assert_eq!(cur[0].value_num, Some(301.0));
    assert_eq!(cur[0].source_uri.as_deref(), Some("https://example.invalid/gb10"));
    let hist = docs::facts(&pool, "GB10", Some("memory_bandwidth"), None, true).await.unwrap();
    assert_eq!(hist.len(), 2);
    let then = docs::facts(&pool, "gb10", None, chrono::NaiveDate::from_ymd_opt(2026, 1, 1), false).await.unwrap();
    assert_eq!(then.len(), 1);
    assert_eq!(then[0].value_num, Some(273.0));
    let sources: i64 = sqlx::query_scalar("select count(*) from kb.sources where uri = 'https://example.invalid/gb10'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sources, 1, "sources are deduped by uri");
    assert!(docs::add_fact(&pool, NewFact { entity: "Brand New Thing".into(), attribute: "x".into(), value_num: Some(1.0), ..Default::default() }).await.is_err());
    let made = docs::add_fact(
        &pool,
        NewFact {
            entity: "Old Promo".into(),
            entity_kind: Some("provider".into()),
            attribute: "price_usd_per_gpu_hr".into(),
            value_num: Some(0.99),
            valid_to: chrono::NaiveDate::from_ymd_opt(2020, 1, 1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(made.action, FactAction::Added);

    // ── query ──
    let ans = docs::query(&pool, &embedder, &reranker, model_id, "What is the memory bandwidth?", Some("/docs/entity/nvidia-gb10"), Some(1500))
        .await
        .unwrap();
    println!("query:\n{}", ans.context_md);
    assert!(ans.context_md.contains("## Facts: NVIDIA GB10"));
    assert!(ans.context_md.contains("301 GB/s"));
    assert!(ans.context_md.contains("[1] "));
    assert!(!ans.citations.is_empty());
    assert!(ans.citations.iter().all(|c| c.path == "References/NVIDIA GB10.md" || c.path == "Notes/Spark memory.md" || c.path == "index.md" || c.path == "Wiki/Old.md"));
    assert!(rank::approx_tokens(&ans.context_md) <= 1500 + 200);
    let ans = docs::query(&pool, &embedder, &reranker, model_id, "error code overheating", Some("/docs/Wiki/"), None).await.unwrap();
    assert!(ans.citations.iter().all(|c| c.path.starts_with("Wiki/")));
    assert!(docs::query(&pool, &embedder, &reranker, model_id, "x", Some("/docs/nothing-like-this"), None).await.is_err());

    // ── writes ──
    let out = docs::write_doc(
        &pool,
        root,
        WriteDoc {
            path: "Wiki/GB10 summary".into(),
            title: Some("GB10 summary".into()),
            author: "agent".into(),
            frontmatter: Some(serde_json::json!({"categories": ["[[GPUs]]"], "tags": ["hardware"]}).as_object().unwrap().clone()),
            body: "# GB10 summary\n\nBandwidth per [[NVIDIA GB10]] is 301 GB/s.".into(),
            message: Some("summarise GB10".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(out.path, "Wiki/GB10 summary.md");
    assert!(out.created);
    let commit = out.commit.clone().unwrap();
    assert_eq!(git(root, &["log", "-1", "--format=%H %an <%ae> %s"]).trim(), format!("{commit} lumbridge-library <lumbridge-library@localhost> summarise GB10"));
    let log = std::fs::read_to_string(root.join("log.md")).unwrap();
    assert!(log.contains("write | Wiki/GB10 summary.md — summarise GB10"), "{log}");
    let page = docs::read(&pool, "Wiki/GB10 summary.md", None).await.unwrap().unwrap();
    assert_eq!(page.author, "agent");
    assert_eq!(page.categories, vec!["GPUs"]);
    assert_eq!(page.links[0].path.as_deref(), Some("References/NVIDIA GB10.md"));
    assert!(git(root, &["status", "--porcelain"]).trim().is_empty());
    // Rewrite keeps `created`, reports created=false.
    let again = docs::write_doc(
        &pool,
        root,
        WriteDoc { path: "Wiki/GB10 summary.md".into(), title: None, author: "claude".into(), frontmatter: None, body: "Updated.".into(), message: None },
    )
    .await
    .unwrap();
    assert!(!again.created);
    let page = docs::read(&pool, "Wiki/GB10 summary.md", None).await.unwrap().unwrap();
    assert_eq!((page.author.as_str(), page.categories.clone()), ("claude", vec!["GPUs".to_string()]), "rewrite keeps unspecified frontmatter");

    // Refusals.
    let owner_before = std::fs::read_to_string(root.join("Wiki/Owner page.md")).unwrap();
    let err = docs::write_doc(
        &pool,
        root,
        WriteDoc { path: "Wiki/Owner page.md".into(), title: None, author: "agent".into(), frontmatter: None, body: "overwritten".into(), message: None },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not agent-authored"), "{err}");
    // Case-only collision with an existing file is refused (APFS on the Mac would merge them).
    let err = docs::write_doc(
        &pool,
        root,
        WriteDoc { path: "Wiki/owner page.md".into(), title: None, author: "agent".into(), frontmatter: None, body: "x".into(), message: None },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("differs only by case"), "{err}");
    assert_eq!(std::fs::read_to_string(root.join("Wiki/Owner page.md")).unwrap(), owner_before);
    let wd = |path: &str, author: &str| WriteDoc { path: path.into(), title: None, author: author.into(), frontmatter: None, body: "x".into(), message: None };
    assert!(docs::write_doc(&pool, root, wd("Wiki/New.md", "owner")).await.is_err());
    assert!(docs::write_doc(&pool, root, wd("Notes/New.md", "agent")).await.is_err());
    assert!(docs::write_doc(&pool, root, wd("Wiki/../Notes/New.md", "agent")).await.is_err());
    assert!(!root.join("Wiki/New.md").exists() && !root.join("Notes/New.md").exists());

    let clipped = docs::clip(
        &pool,
        root,
        Clip {
            url: "https://example.invalid/post".into(),
            title: "A post: about GB10?".into(),
            body: "Clipped text.".into(),
            author: "agent".into(),
            categories: vec!["GPUs".into()],
            tags: vec![],
            message: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(clipped.path, "Clippings/A post- about GB10.md");
    let c = docs::read(&pool, &clipped.path, None).await.unwrap().unwrap();
    assert_eq!(c.status, "raw");
    assert_eq!(c.frontmatter["source"], "https://example.invalid/post");

    // ── lint ──
    std::fs::remove_file(root.join("Daily/2026-11-07.md")).unwrap();
    let lint = docs::lint(&pool, root).await.unwrap();
    println!("lint: {}", serde_json::to_string_pretty(&lint).unwrap());
    assert!(lint.broken_links.iter().any(|b| b.dst_text == "Nonexistent Page" && b.src_path == "Wiki/Errors.md"));
    assert!(lint.parse_errors.iter().any(|p| p.path == "Clippings/Broken.md"));
    assert!(lint.stale_wiki.iter().any(|s| s.path == "Wiki/Old.md" && s.source_path == "References/NVIDIA GB10.md"));
    assert!(lint.expired_facts.iter().any(|f| f.id == made.id));
    assert!(lint.missing_categories.contains(&"Notes/Spark memory.md".to_string()));
    assert!(lint.orphans.contains(&"Thoughts/Sourdough.md".to_string()));
    assert!(lint.unembedded_chunks > 0);
    assert_eq!(lint.missing_files, vec!["Daily/2026-11-07.md".to_string()]);

    let r4 = docs::sync_vault(&pool, root).await.unwrap();
    assert_eq!(r4.deleted, 1);
    // The link from the deleted daily note is gone; Spark memory's own links survive.
    assert_eq!(docs::read(&pool, "Notes/Spark memory.md", None).await.unwrap().unwrap().backlinks.len(), 0);
}
