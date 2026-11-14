//! Librarian end-to-end on a temp git vault, against the shared scratch DB. Gated: runs only with
//! `LIBRARIAN_E2E=1` plus `TEST_DATABASE_URL` / `TEST_EMBED_URL` / `TEST_RERANK_URL` (and
//! `TEST_LLM_URL` for the LLM steps, e.g. http://127.0.0.1:8000/v1). Other agents use the same DB,
//! so this test never runs `sync_vault` (which would delete their documents): it indexes its own
//! `zzlib*` files with `sync_file`, scopes every librarian task to them, and cleans up after itself.

use lumbridge_library_core::config::{EmbedConfig, LibrarianConfig, RerankConfig};
use lumbridge_library_core::docs::{self, NewFact};
use lumbridge_library_core::librarian::{self, Ctx, RunOptions, Task};
use lumbridge_library_core::llm::Llm;
use lumbridge_library_core::models::{Embedder, Reranker};
use lumbridge_library_core::db;
use std::path::Path;
use std::process::Command;

const SLUG: &str = "zzlib-test-chip";

fn put(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

const FILES: [&str; 6] = [
    "References/zzlib Test Chip.md",
    "References/zzlib Owner Chip.md",
    "Wiki/zzlib Prices Overview.md",
    "Clippings/zzlib Test Chip launch review.md",
    "Clippings/zzlib Dup Post.md",
    "Clippings/zzlib Dup Post 2.md",
];

fn build(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    put(root, "log.md", "# Log\n");
    put(
        root,
        FILES[0],
        "---\ntitle: zzlib Test Chip\nauthor: claude\nstatus: draft\ncategories:\n- \"[[GPUs]]\"\ncreated: 2026-11-07\nsource: https://example.invalid/zzlib-catalog\nvendor: Zzlib\nentity:\n  slug: zzlib-test-chip\n  kind: chip\n  aliases: [ZZ1]\n---\n\
The zzlib Test Chip is a fictional accelerator used by the librarian test.\n\n## Key specs\n\n- **Vendor:** Zzlib\n- **Memory:** 144 GB HBM3e\n- **Memory bandwidth:** 4.8 TB/s\n- **Power:** 700 W\n\n\
## Prices\n\n- **New:** ~$23-30k (U)\n- **Rental:** Vultr $2.59/GPU-hr, on-demand, min 8 GPU ([pricing](https://example.invalid/vultr), as of 2026-11-08)\n- **Rent:** $5.99 Daytona / $2.95 TensorWave contract\n\n\
## Related\n\n- [[GPU Rental and Hardware Prices]]\n",
    );
    put(
        root,
        FILES[1],
        "---\ntitle: zzlib Owner Chip\nauthor: owner\ncreated: 2026-11-07\nentity:\n  slug: zzlib-owner-chip\n  kind: chip\n---\nThe owner's own note about the zzlib Test Chip launch.\n\n## Key specs\n\n- **Memory:** 96 GB\n",
    );
    put(
        root,
        FILES[2],
        "---\ntitle: zzlib Prices Overview\nauthor: claude\ncreated: 2026-09-01\n---\nzzlib rentals were $1.99/hr (as of 2026-08-01). Prices are dated. Re-check before acting.\n",
    );
    put(
        root,
        FILES[3],
        "---\ntitle: zzlib Test Chip launch review\nauthor: claude\nstatus: raw\ncreated: 2099-01-01\nsource: https://example.invalid/zzlib-review\n---\n\
A hands-on review of the zzlib Test Chip accelerator at launch: we measured its 144 GB of HBM3e memory, 4.8 TB/s bandwidth \
and 700 W power draw, and compared rental prices across clouds. The zzlib Test Chip is the main subject of this whole article.\n",
    );
    for f in &FILES[4..] {
        put(
            root,
            f,
            "---\ntitle: zzlib Dup Post\nauthor: claude\nstatus: raw\ncreated: 2026-01-01\nsource: https://example.invalid/dup-post\n---\nThe same zzlib post saved twice by accident.\n",
        );
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "seed"]);
}

async fn cleanup(pool: &sqlx::PgPool) {
    let _ = sqlx::query("delete from kb.facts where entity_id in (select id from kb.entities where slug like 'zzlib-%')").execute(pool).await;
    let _ = sqlx::query("delete from kb.entities where slug like 'zzlib-%'").execute(pool).await;
    let _ = sqlx::query("delete from kb.documents where path = any($1)").bind(FILES.map(String::from).to_vec()).execute(pool).await;
    let _ = sqlx::query("delete from kb.sources where uri like 'https://example.invalid/%' and not exists (select 1 from kb.facts f where f.source_id = kb.sources.id) and not exists (select 1 from kb.documents d where d.source_id = kb.sources.id)").execute(pool).await;
}

#[tokio::test]
async fn librarian_end_to_end() {
    if std::env::var("LIBRARIAN_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping: set LIBRARIAN_E2E=1 (shared scratch DB)");
        return;
    }
    let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else { return };
    let pool = db::connect(&db_url, 4).await.unwrap();
    // other branches may have applied newer migrations to the shared DB; the schema we need exists either way
    let _ = db::migrate(&pool).await;
    cleanup(&pool).await;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build(root);
    for f in FILES {
        docs::sync_file(&pool, root, f).await.unwrap();
    }
    docs::sync::resolve_links(&pool).await.unwrap();

    let embedder = Embedder::new(EmbedConfig {
        base_url: std::env::var("TEST_EMBED_URL").unwrap(),
        model: "embed".into(),
        dims: 2048,
        query_prefix: "query: ".into(),
        passage_prefix: "passage: ".into(),
        batch: 32,
    });
    let reranker = Reranker::new(RerankConfig { base_url: std::env::var("TEST_RERANK_URL").unwrap(), model: "rerank".into() });
    let ident = embedder.identity().await.unwrap();
    let mid = db::register_embed_model(&pool, &ident).await.unwrap();
    // Embed just our chunks (the shared backlog is someone else's business).
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "select c.id, d.title, c.heading, c.content from kb.chunks c join kb.documents d on d.id = c.doc_id where d.path = any($1)",
    )
    .bind(FILES.map(String::from).to_vec())
    .fetch_all(&pool)
    .await
    .unwrap();
    let texts: Vec<String> = rows.iter().map(|(_, t, h, c)| docs::embed_text(t, h, c)).collect();
    let vecs = embedder.embed(&texts, lumbridge_library_core::models::Kind::Passage).await.unwrap();
    for ((id, ..), v) in rows.iter().zip(vecs) {
        sqlx::query("update kb.chunks set embedding = $2::extensions.halfvec, embed_model_id = $3 where id = $1")
            .bind(id)
            .bind(db::halfvec_literal(&v))
            .bind(mid)
            .execute(&pool)
            .await
            .unwrap();
    }

    // A fact from another source that disagrees with the note: must be reported, not overwritten.
    docs::add_fact(
        &pool,
        NewFact {
            entity: SLUG.into(),
            entity_kind: Some("chip".into()),
            attribute: "power".into(),
            value_num: Some(1000.0),
            unit: Some("W".into()),
            source_uri: Some("https://example.invalid/other".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let llm_url = std::env::var("TEST_LLM_URL").ok();
    let mut cfg = LibrarianConfig { max_edits_per_run: 2, ..Default::default() };
    if let Some(u) = &llm_url {
        cfg.base_url = u.clone();
    }
    let owner_before = std::fs::read_to_string(root.join(FILES[1])).unwrap();
    let ctx = Ctx {
        pool: &pool,
        vault_root: root,
        embedder: &embedder,
        reranker: &reranker,
        embed_model_id: mid,
        fingerprint: ident.fingerprint.clone(),
        llm: llm_url.as_ref().map(|_| Llm::from_config(&cfg)),
        cfg: cfg.clone(),
    };
    let scope: Vec<String> = vec!["References/zzlib".into(), "Wiki/zzlib".into(), "Clippings/zzlib".into()];
    let tasks = vec![Task::Facts, Task::Links, Task::Duplicates, Task::Staleness];

    // 1. dry run: nothing written.
    let dry = librarian::run(&ctx, RunOptions { dry_run: true, tasks: tasks.clone(), requested_by: "test".into(), path_prefixes: scope.clone() }).await;
    let f = dry.facts.as_ref().unwrap();
    assert!(f.added >= 6, "{f:?}");
    assert_eq!(docs::facts(&pool, SLUG, Some("memory"), None, false).await.unwrap().len(), 0, "dry run wrote a fact");
    assert_eq!(std::fs::read_to_string(root.join(FILES[0])).unwrap().matches("launch review").count(), 0);

    // 2. real run.
    let rep = librarian::run(&ctx, RunOptions { dry_run: false, tasks: tasks.clone(), requested_by: "test".into(), path_prefixes: scope.clone() }).await;
    eprintln!("{}", serde_json::to_string_pretty(&rep).unwrap());
    let f = rep.facts.as_ref().unwrap();
    let mem = docs::facts(&pool, SLUG, Some("memory"), None, false).await.unwrap();
    assert_eq!(mem.len(), 1);
    assert_eq!((mem[0].value_num, mem[0].unit.as_deref(), mem[0].value_text.as_deref()), (Some(144.0), Some("GB"), Some("HBM3e")));
    assert_eq!(mem[0].verified_by.as_deref(), Some("vendor"));
    let new = docs::facts(&pool, SLUG, Some("price_new"), None, false).await.unwrap();
    assert_eq!((new[0].value_num, new[0].value_hi, new[0].verified_by.as_deref()), (Some(23000.0), Some(30000.0), Some("community")));
    assert!(new[0].confidence.unwrap() <= 0.5);
    let rent = docs::facts(&pool, SLUG, Some("price_rental.vultr-on-demand"), None, false).await.unwrap();
    assert_eq!(rent[0].unit.as_deref(), Some("USD/GPU-hr"));
    assert_eq!(rent[0].valid_from, chrono::NaiveDate::from_ymd_opt(2026, 11, 8));
    assert_eq!(rent[0].source_uri.as_deref(), Some("https://example.invalid/vultr"));
    // contradiction reported, the other source's value kept
    assert!(f.contradictions.iter().any(|c| c.attribute == "power" && c.entity == SLUG), "{:?}", f.contradictions);
    let power = docs::facts(&pool, SLUG, Some("power"), None, false).await.unwrap();
    assert_eq!(power[0].value_num, Some(1000.0));
    // The owner's reference: never touched, but its facts are read (reading is fine)
    assert_eq!(std::fs::read_to_string(root.join(FILES[1])).unwrap(), owner_before);

    // duplicates + staleness
    let d = rep.duplicates.as_ref().unwrap();
    assert!(d.pairs.iter().any(|p| p.same_source && p.a.contains("zzlib Dup Post")), "{d:?}");
    let s = rep.staleness.as_ref().unwrap();
    let ov = s.pages.iter().find(|p| p.path == FILES[2]).expect("stale overview");
    assert!(ov.reasons.iter().any(|r| r.contains("re-check")), "{:?}", ov.reasons);

    // links (LLM-dependent)
    if llm_url.is_some() {
        let l = rep.links.as_ref().unwrap();
        assert!(l.orphans >= 1);
        for p in &l.proposed {
            assert_ne!(p.page, FILES[1], "proposed a link on the owner's page");
            let text = std::fs::read_to_string(root.join(&p.page)).unwrap();
            assert!(text.contains(&p.line), "{} lacks {}", p.page, p.line);
            assert!(text.contains("author: librarian"));
        }
        if l.edited > 0 {
            let log = std::fs::read_to_string(root.join("log.md")).unwrap();
            assert!(log.contains("lint | librarian run —"), "{log}");
            assert!(rep.log_md.starts_with("appended to log.md"));
        }
        eprintln!("links: {:?}", l.proposed);
    }

    // feed rows
    let rows = db::activity(&pool, 200, None, None, None).await.unwrap();
    assert!(rows.iter().any(|r| r.op == "librarian.run" && r.actor.as_deref() == Some("librarian")));
    assert!(rows.iter().any(|r| r.op == "librarian.fact" && r.target.as_deref() == Some("zzlib-test-chip/memory")));
    assert!(rows.iter().any(|r| r.op == "librarian.contradiction"));

    // 3. idempotent: a second real run writes no facts and no links.
    let again = librarian::run(&ctx, RunOptions { dry_run: false, tasks, requested_by: "test".into(), path_prefixes: scope }).await;
    let f2 = again.facts.as_ref().unwrap();
    assert_eq!(f2.added + f2.updated, 0, "{f2:?}");
    assert!(f2.unchanged >= 6);
    assert_eq!(again.links.as_ref().map(|l| l.edited).unwrap_or(0), 0);

    cleanup(&pool).await;
}
