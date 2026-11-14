//! Shared setup for warehouse integration tests. Every test repo slug starts with `test-wh-` and
//! tests only delete their own rows (another agent tests docs against the same database).

#![allow(dead_code)]

use lumbridge_library_core::config::{EmbedConfig, RerankConfig};
use lumbridge_library_core::models::{Embedder, Reranker};
use sqlx::PgPool;
use std::path::Path;
use std::process::Command;

pub async fn pool() -> Option<PgPool> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = lumbridge_library_core::db::connect(&url, 4).await.expect("connect TEST_DATABASE_URL");
    if let Err(e) = lumbridge_library_core::db::migrate(&pool).await {
        // Another branch (docs, 0002) may have applied a migration this tree does not have yet.
        // sqlx returns that error while still holding its advisory lock on a pooled connection,
        // so drop the pool before retrying leniently.
        let msg = format!("{e:#}");
        assert!(msg.contains("previously applied but is missing"), "migrate: {msg}");
        pool.close().await;
        let pool = lumbridge_library_core::db::connect(&url, 4).await.expect("reconnect");
        let mut m = sqlx::migrate!("../../migrations");
        m.set_ignore_missing(true);
        m.run(&pool).await.expect("lenient migrate");
        return Some(pool);
    }
    Some(pool)
}

pub fn models() -> Option<(Embedder, Reranker)> {
    let e = std::env::var("TEST_EMBED_URL").ok()?;
    let r = std::env::var("TEST_RERANK_URL").ok()?;
    let embed = Embedder::new(EmbedConfig {
        base_url: e,
        model: "embed".into(),
        dims: 2048,
        query_prefix: "query: ".into(),
        passage_prefix: "passage: ".into(),
        batch: 32,
    });
    let rerank = Reranker::new(RerankConfig { base_url: r, model: "rerank".into() });
    Some((embed, rerank))
}

pub async fn model_id(pool: &PgPool, e: &Embedder) -> i32 {
    let ident = e.identity().await.expect("embedder identity");
    lumbridge_library_core::db::register_embed_model(pool, &ident).await.expect("register model")
}

pub fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git");
    assert!(st.success(), "git {args:?}");
}

pub fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

pub async fn cleanup(pool: &PgPool) {
    let slugs: Vec<String> = sqlx::query_scalar(
        "select slug from wh.repos where slug like 'own/test-wh-%' or slug like 'mirrors/%/test-wh-%' or slug like 'components/test-wh-%'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    for s in slugs {
        lumbridge_library_core::warehouse::remove_repo(pool, &s, false).await.unwrap();
    }
    sqlx::query("delete from wh.components where slug like 'test-wh-%'").execute(pool).await.unwrap();
}

pub const MIT: &str = "MIT License\n\nCopyright (c) 2026 Test\n\nPermission is hereby granted, free of charge, to any person obtaining a copy\nof this software and associated documentation files (the \"Software\"), to deal\nin the Software without restriction.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND.\n";

pub const GPL: &str = "                    GNU GENERAL PUBLIC LICENSE\n                       Version 3, 29 June 2007\n\n Copyright (C) 2007 Free Software Foundation, Inc. <https://fsf.org/>\n Everyone is permitted to copy and distribute verbatim copies\n of this license document, but changing it is not allowed.\n";
