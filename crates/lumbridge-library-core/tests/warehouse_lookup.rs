//! Repo lookup, re-registration and removal guards against the scratch DB (skips without
//! TEST_DATABASE_URL). Rows use `test-wh-` slugs so `cleanup` only touches this test's data.

mod warehouse_common;

use lumbridge_library_core::warehouse as wh;
use sqlx::PgPool;
use std::path::Path;
use warehouse_common::*;

async fn insert_row(pool: &PgPool, slug: &str, tier: &str, local_path: &Path) {
    sqlx::query("insert into wh.repos (slug, tier, local_path, added_by) values ($1, $2, $3, 'test')")
        .bind(slug)
        .bind(tier)
        .bind(local_path.to_string_lossy().as_ref())
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn warehouse_lookup_and_guards() {
    let Some(pool) = pool().await else {
        eprintln!("TEST_DATABASE_URL not set; skipping");
        return;
    };
    cleanup(&pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("library");
    let nowhere = tmp.path().join("nowhere");

    // ── short names: LIKE wildcards are literal, ambiguity is an error ──
    insert_row(&pool, "mirrors/test-wh-a/test-wh-dup", "mirror", &nowhere).await;
    insert_row(&pool, "mirrors/test-wh-b/test-wh-dup", "mirror", &nowhere).await;
    insert_row(&pool, "mirrors/test-wh-c/test-wh-xzy", "mirror", &nowhere).await;
    insert_row(&pool, "mirrors/test-wh-c/test-wh-x_y", "mirror", &nowhere).await;

    let err = wh::get_repo(&pool, "test-wh-dup").await.unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "{err}");
    assert!(err.contains("mirrors/test-wh-a/test-wh-dup") && err.contains("mirrors/test-wh-b/test-wh-dup"), "{err}");
    // Full slugs still resolve.
    let a = wh::get_repo(&pool, "mirrors/test-wh-a/test-wh-dup").await.unwrap().unwrap();
    assert_eq!(a.slug, "mirrors/test-wh-a/test-wh-dup");
    // `_` is literal: matches only test-wh-x_y, not test-wh-xzy.
    let x = wh::get_repo(&pool, "test-wh-x_y").await.unwrap().unwrap();
    assert_eq!(x.slug, "mirrors/test-wh-c/test-wh-x_y");
    let z = wh::get_repo(&pool, "test-wh-xzy").await.unwrap().unwrap();
    assert_eq!(z.slug, "mirrors/test-wh-c/test-wh-xzy");
    // `%` is literal too.
    assert!(wh::get_repo(&pool, "test-wh-%").await.unwrap().is_none());
    assert!(wh::get_repo(&pool, "test-wh-x%").await.unwrap().is_none());
    assert!(wh::get_repo(&pool, "test-wh-x\\_y").await.unwrap().is_none());

    // ── exact slug / components/<slug> win over a short-name match ──
    insert_row(&pool, "components/test-wh-dup", "component", &nowhere).await;
    let c = wh::get_repo(&pool, "test-wh-dup").await.unwrap().unwrap();
    assert_eq!(c.slug, "components/test-wh-dup");

    // ── remove_repo_in refuses paths outside {root}/warehouse ──
    let outside = tmp.path().join("precious");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), "keep").unwrap();
    insert_row(&pool, "own/test-wh-outside", "own", &outside).await;
    std::fs::create_dir_all(root.join("warehouse")).unwrap();
    let err = wh::remove_repo_in(&pool, &root, "own/test-wh-outside", true).await.unwrap_err().to_string();
    assert!(err.contains("refusing to delete"), "{err}");
    assert!(outside.join("keep.txt").exists());
    assert!(wh::get_repo(&pool, "own/test-wh-outside").await.unwrap().is_some(), "row kept on refusal");
    let err = wh::remove_repo(&pool, "own/test-wh-outside", true).await.unwrap_err().to_string();
    assert!(err.contains("refusing to delete"), "{err}");
    assert!(outside.join("keep.txt").exists());
    // Without delete_files the row goes and the files stay.
    assert!(wh::remove_repo_in(&pool, &root, "own/test-wh-outside", false).await.unwrap());
    assert!(outside.join("keep.txt").exists());

    // ── re-adding a name with a different origin is refused ──
    let src1 = tmp.path().join("src1/test-wh-reorigin");
    let src2 = tmp.path().join("src2/test-wh-reorigin");
    for (d, body) in [(&src1, "one"), (&src2, "two")] {
        std::fs::create_dir_all(d).unwrap();
        git(d, &["init", "-q", "--initial-branch=main"]);
        write(d, "a.txt", body);
        git(d, &["add", "-A"]);
        git(d, &["commit", "-q", "-m", "init"]);
    }
    let r1 = wh::add_own(&pool, &root, src1.to_str().unwrap(), Some("test-wh-reorigin"), &[]).await.unwrap();
    let err = wh::add_own(&pool, &root, src2.to_str().unwrap(), Some("test-wh-reorigin"), &[]).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("refusing to re-register"), "{msg}");
    let row = wh::get_repo(&pool, "own/test-wh-reorigin").await.unwrap().unwrap();
    assert_eq!(row.origin_url, r1.origin_url, "DB keeps the original origin");
    // Same origin again is still idempotent.
    wh::add_own(&pool, &root, src1.to_str().unwrap(), Some("test-wh-reorigin"), &[]).await.unwrap();
    // Inside the warehouse, deletion works.
    assert!(wh::remove_repo_in(&pool, &root, "own/test-wh-reorigin", true).await.unwrap());
    assert!(!root.join("warehouse/own/test-wh-reorigin.git").exists());

    // ── add_mirror refuses local paths and file:// ──
    for bad in [src1.to_str().unwrap().to_string(), format!("file://{}", src1.display())] {
        let err = wh::add_mirror(&pool, &root, &bad, &[], "test").await.unwrap_err().to_string();
        assert!(err.contains("must be https://"), "{err}");
    }

    cleanup(&pool).await;
}
