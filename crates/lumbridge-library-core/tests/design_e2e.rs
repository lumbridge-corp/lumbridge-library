//! Design shelf end to end against TEST_DATABASE_URL (skips without it); search ranking also needs
//! TEST_EMBED_URL / TEST_RERANK_URL. Slugs start with `test-wh-dz-`; only those rows are touched.
//!   set -a; . ./test.env; cargo test -p lumbridge-library-core --test design_e2e -- --nocapture

mod warehouse_common;

use lumbridge_library_core::warehouse::design::{self as dz, DesignFilter, DesignSearchParams, NewDesign};
use serde_json::json;
use std::path::Path;
use warehouse_common::*;

/// A valid 1x1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00,
    0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

fn capture(root: &Path, name: &str, dark: bool) {
    let dir = dz::designs_dir(root).join(name);
    let (bg, ink, font, feel) = if dark {
        ("rgb(10, 10, 10)", "rgb(255, 255, 255)", "\"Space Mono\", monospace", "A near-black engineering console with monospace caps labels and hairline rails.")
    } else {
        ("rgb(250, 247, 240)", "rgb(34, 34, 34)", "\"Fraunces\", serif", "A warm cream editorial page with a large serif display face and generous margins.")
    };
    write(&dir, "manifest.json", &json!({
        "site": format!("https://{name}.example/"), "slug": name, "captured_at": "2026-11-09T08:10:20Z", "tool": "design-capture 2",
        "pages": [{"n": "00", "url": format!("https://{name}.example/"), "title": format!("{name} home")}]
    }).to_string());
    write(&dir, "tokens.json", &json!({
        "background": [{"v": bg, "c": 100}], "text_color": [{"v": ink, "c": 100}],
        "font_family": [{"v": font, "c": 100}], "radius": [{"v": "6px", "c": 10}], "spacing": [{"v": "16px", "c": 9}]
    }).to_string());
    write(&dir, "system/DESIGN.md", &format!("# {name} design system\n\nCaptured 2026.\n\n## The feel in one line\n{feel}\n\n## Colour\n| a | b |\n"));
    write(&dir, "system/tokens.css", ":root { --x-accent: #95aba7; --x-ink: #fff; }\n");
    write(&dir, "system/COMPONENTS.md", "# Components\n\n## Pill button\n\n## Metric card\n");
    let p = dir.join("pages/00/desktop-fold.png");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, PNG).unwrap();
    std::fs::write(dir.join("pages/00/desktop.png"), PNG).unwrap();
    // Raw assets never reach the design repo.
    write(&dir, "assets/logo.svg", "<svg/>");
}

async fn cleanup_designs(pool: &sqlx::PgPool) {
    sqlx::query("delete from wh.designs where slug like 'test-wh-dz-%'").execute(pool).await.unwrap();
    let slugs: Vec<String> = sqlx::query_scalar("select slug from wh.repos where slug like 'designs/test-wh-dz-%'").fetch_all(pool).await.unwrap();
    for s in slugs {
        lumbridge_library_core::warehouse::remove_repo(pool, &s, false).await.unwrap();
    }
}

#[tokio::test]
async fn design_shelf_end_to_end() {
    let Some(pool) = pool().await else {
        eprintln!("TEST_DATABASE_URL not set; skipping");
        return;
    };
    cleanup_designs(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    capture(&root, "test-wh-dz-dark", true);
    capture(&root, "test-wh-dz-light", false);

    // An older component repo for the dark design: its history must be adopted.
    let old = root.join("warehouse/components/design-test-wh-dz-dark");
    write(&old, "component.toml", "slug = \"design-test-wh-dz-dark\"\n");
    git(&old, &["init", "-q", "-b", "main"]);
    git(&old, &["add", "-A"]);
    git(&old, &["commit", "-q", "-m", "old component"]);

    let mk = |slug: &str| NewDesign {
        slug: slug.into(),
        kind: "capture".into(),
        capture: Some(slug.into()),
        tags: vec!["test".into()],
        created_by: "claude".into(),
        ..Default::default()
    };
    let dark = dz::upsert_design(&pool, &root, mk("test-wh-dz-dark")).await.unwrap();
    assert_eq!(dark.theme.as_deref(), Some("dark"));
    assert_eq!(dark.bg.as_deref(), Some("#0a0a0a"));
    assert_eq!(dark.fg.as_deref(), Some("#ffffff"));
    assert_eq!(dark.accent.as_deref(), Some("#95aba7"));
    assert_eq!(dark.fonts, vec!["Space Mono".to_string()]);
    assert_eq!(dark.radius_px, Some(6.0));
    assert_eq!(dark.status, "draft");
    assert_eq!(dark.shots, 2);
    assert_eq!(dark.site_url.as_deref(), Some("https://test-wh-dz-dark.example/"));
    assert!(dark.summary.starts_with("A near-black engineering console"), "{}", dark.summary);
    assert_eq!(dark.repo_slug.as_deref(), Some("designs/test-wh-dz-dark"));
    let repo_dir = Path::new(dark.local_path.as_deref().unwrap());
    let log = std::process::Command::new("git").arg("-C").arg(repo_dir).args(["log", "--format=%s"]).output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("old component"), "history adopted: {log}");
    assert!(!repo_dir.join("component.toml").exists());
    assert!(repo_dir.join("design.toml").exists() && repo_dir.join("DESIGN.md").exists());
    assert!(!repo_dir.join("assets").exists() && !repo_dir.join("logo.svg").exists());

    let light = dz::upsert_design(&pool, &root, mk("test-wh-dz-light")).await.unwrap();
    assert_eq!(light.theme.as_deref(), Some("light"));
    assert_eq!(light.fonts, vec!["Fraunces".to_string()]);

    // Detail: files from git, shots ordered fold first.
    let det = dz::design_detail(&pool, "designs/test-wh-dz-dark").await.unwrap().unwrap();
    assert!(det.files.contains(&"tokens.css".to_string()) && det.files.contains(&"README.md".to_string()));
    assert_eq!(det.shots[0].view, "fold");
    assert_eq!(det.shots[0].width, Some(1));
    assert_eq!(det.facets["spacing"][0], "16px");
    let css = dz::read_design_file(&pool, "test-wh-dz-dark", "tokens.css").await.unwrap();
    assert!(css.contains("--x-accent"));
    assert!(dz::read_design_file(&pool, "test-wh-dz-dark", "../x").await.is_err());
    let f = dz::shot_file(&pool, &root, "test-wh-dz-dark", det.shots[0].id).await.unwrap();
    assert!(f.ends_with("pages/00/desktop-fold.png"));

    // Filters.
    let f = DesignFilter { theme: Some("light".into()), tag: Some("test".into()), ..Default::default() };
    let l: Vec<String> = dz::list_designs(&pool, &f).await.unwrap().into_iter().map(|d| d.slug).collect();
    assert_eq!(l, vec!["test-wh-dz-light".to_string()]);
    let f = DesignFilter { font: Some("mono".into()), tag: Some("test".into()), ..Default::default() };
    assert_eq!(dz::list_designs(&pool, &f).await.unwrap().len(), 1);

    // A caption survives a re-import when the screenshot didn't change.
    sqlx::query("update wh.design_shots set caption = 'dark hero with a pill button and mono metric labels' where design_id = $1::uuid and view = 'fold'")
        .bind(&dark.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("update wh.design_shots set caption = 'cream page with a big serif headline and photo grid' where design_id = $1::uuid and view = 'fold'")
        .bind(&light.id)
        .execute(&pool)
        .await
        .unwrap();
    let again = dz::upsert_design(&pool, &root, mk("test-wh-dz-dark")).await.unwrap();
    assert_eq!(again.captioned, 1);
    assert_eq!(again.id, dark.id);

    // Status round trip writes design.toml.
    let ap = dz::set_status(&pool, "test-wh-dz-dark", "approved").await.unwrap();
    assert!(dz::write_status_file(&ap).await.unwrap());
    assert!(std::fs::read_to_string(repo_dir.join("design.toml")).unwrap().contains("status = \"approved\""));
    // ...and a re-import keeps it approved.
    assert_eq!(dz::upsert_design(&pool, &root, mk("test-wh-dz-dark")).await.unwrap().status, "approved");

    // Validation.
    let mut bad = mk("test-wh-dz-nope");
    bad.capture = Some("missing".into());
    assert!(dz::upsert_design(&pool, &root, bad).await.is_err());
    let mut bad = mk("Bad Slug");
    bad.capture = None;
    assert!(dz::upsert_design(&pool, &root, bad).await.is_err());
    let refd = NewDesign {
        slug: "test-wh-dz-ref".into(),
        kind: "reference".into(),
        files: vec![("DESIGN.md".into(), "# Ref\n\n## 1. Visual Theme & Atmosphere\n\nBright playful pastel marketplace with rounded cards.\n".into())],
        theme: Some("light".into()),
        accent: Some("rgb(255, 56, 92)".into()),
        tags: vec!["test".into()],
        created_by: "claude".into(),
        ..Default::default()
    };
    let r = dz::upsert_design(&pool, &root, refd).await.unwrap();
    assert_eq!(r.accent.as_deref(), Some("#ff385c"));
    assert_eq!(r.shots, 0);

    // Search (lexical always; vector + rerank when models are configured).
    let models = models();
    let mid = match &models {
        Some((e, _)) => {
            let mid = model_id(&pool, e).await;
            dz::embed_pending(&pool, e, mid, 1000).await.unwrap();
            Some(mid)
        }
        None => None,
    };
    let (e, r) = models.unwrap_or_else(|| {
        // Unreachable endpoints: search must degrade to lexical lists.
        let m = lumbridge_library_core::config::EmbedConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "x".into(),
            dims: 2048,
            query_prefix: String::new(),
            passage_prefix: String::new(),
            batch: 1,
        };
        (
            lumbridge_library_core::models::Embedder::new(m),
            lumbridge_library_core::models::Reranker::new(lumbridge_library_core::config::RerankConfig { base_url: "http://127.0.0.1:9/v1".into(), model: "x".into() }),
        )
    });
    let p = |q: &str| DesignSearchParams { query: q.into(), tag: Some("test".into()), include_drafts: true, limit: 5, ..Default::default() };
    let res = dz::search(&pool, &e, &r, mid, &p("dark console with monospace labels")).await.unwrap();
    eprintln!("lists {:?}: {:?}", res.lists, res.hits.iter().map(|h| (&h.slug, h.score)).collect::<Vec<_>>());
    assert_eq!(res.hits[0].slug, "test-wh-dz-dark");
    assert!(res.hits[0].shots.iter().any(|s| s.url.starts_with("/v1/designs/test-wh-dz-dark/shots/")));
    let res = dz::search(&pool, &e, &r, mid, &p("warm serif editorial cream")).await.unwrap();
    assert_eq!(res.hits[0].slug, "test-wh-dz-light");
    // Colour filter: near-black keeps only the dark one.
    let mut q = p("");
    q.color = Some("#000000".into());
    let res = dz::search(&pool, &e, &r, mid, &q).await.unwrap();
    assert_eq!(res.hits.iter().map(|h| h.slug.as_str()).collect::<Vec<_>>(), vec!["test-wh-dz-dark"]);
    let mut q = p("anything");
    q.theme = Some("purple".into());
    assert!(dz::search(&pool, &e, &r, mid, &q).await.is_err());

    cleanup_designs(&pool).await;
}
