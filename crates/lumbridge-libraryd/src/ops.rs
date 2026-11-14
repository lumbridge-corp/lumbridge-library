//! Upkeep loops (phase 4). Each runs on its own tokio task with the period from `[jobs]`:
//!
//! - **vault**: `git pull` from the vault remote (edits made on other machines) then `docs::sync_vault`.
//!   Cheap when nothing changed (one sha256 per file).
//! - **embed**: drain the embed backlog of both shelves (content-hashed, so only new text).
//! - **mirrors**: fetch every warehouse repo and re-index what changed.
//! - **lint**: nightly vault lint, summarised into `lib.ops_log`.
//! - **librarian**: nightly upkeep agent at `[librarian] nightly_hour` (see `librarian.rs`).
//!
//! Everything reports to `lib.ops_log` so `lumbridge-docs status` / Prometheus can show freshness.

use crate::gitops;
use crate::state::AppState;
use lumbridge_library_core::{db, docs};
use serde_json::json;
use std::time::Duration;
use tracing::{info, warn};

/// After an agent write: push the vault to Gitea in the background (failures are logged, the
/// commit stays local and goes out with the next push).
pub fn after_vault_write(st: &AppState, out: &docs::WriteOutcome, op: &str, actor: &str, message: Option<&str>) {
    let path = out.path.clone();
    let summary = format!(
        "{} {}{}",
        if out.created { "created" } else { "updated" },
        path,
        message.map(|m| format!(" — {m}")).unwrap_or_default()
    );
    let detail = json!({"commit": out.commit, "created": out.created});
    {
        let (st, op, actor) = (st.clone(), op.to_string(), actor.to_string());
        let path = path.clone();
        tokio::spawn(async move { db::record(&st.pool, &op, "docs", &path, true, Some(&actor), Some(&summary), detail).await });
    }
    if !(st.cfg.vault.push && st.cfg.vault.remote.is_some()) {
        return;
    }
    let st = st.clone();
    tokio::spawn(async move {
        let root = st.cfg.vault_root();
        if let Err(e) = docs::push_vault(&root).await {
            // Someone pushed from elsewhere: rebase onto it and retry once.
            warn!("vault push failed ({e:#}); pulling and retrying");
            let r = match docs::pull_vault(&root).await {
                Ok(_) => docs::push_vault(&root).await.map(|_| ()),
                Err(e) => Err(e),
            };
            let line = if r.is_ok() { format!("pushed {path} to Gitea after a rebase") } else { format!("could not push {path} to Gitea (kept locally)") };
            db::record(&st.pool, "push", "docs", &path, r.is_ok(), Some(lumbridge_library_core::legacy::wire::ACTOR), Some(&line), json!({"error": r.err().map(|e| format!("{e:#}"))})).await;
        }
    });
}

fn every(secs: u64) -> Option<tokio::time::Interval> {
    if secs == 0 {
        return None;
    }
    let mut iv = tokio::time::interval(Duration::from_secs(secs));
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    Some(iv)
}

pub async fn vault_once(st: &AppState) -> anyhow::Result<docs::SyncReport> {
    let root = st.cfg.vault_root();
    let before = gitops::git(&root, &["rev-parse", "HEAD"]).await.unwrap_or_default();
    if st.cfg.vault.remote.is_some()
        && let Err(e) = docs::pull_vault(&root).await
    {
        // Conflicts abort the rebase (see docs::pull_vault); record it so it is visible, not silent.
        warn!("vault pull failed: {e:#}");
        db::record(&st.pool, "pull", "docs", "vault", false, Some(lumbridge_library_core::legacy::wire::ACTOR), Some("vault pull from Gitea failed (rebase aborted; local commits kept)"), json!({"error": format!("{e:#}")})).await;
    }
    let after = gitops::git(&root, &["rev-parse", "HEAD"]).await.unwrap_or_default();
    if !before.is_empty() && before != after {
        record_pulled_commits(st, &root, &before, &after).await;
    }
    let r = docs::sync_vault(&st.pool, &root).await?;
    if r.added + r.updated + r.deleted > 0 {
        info!(added = r.added, updated = r.updated, deleted = r.deleted, chunks_new = r.chunks_new, "vault synced");
        let summary = format!("vault indexed: +{} new, ~{} changed, -{} removed documents", r.added, r.updated, r.deleted);
        db::record(&st.pool, "sync", "docs", "vault", true, Some(lumbridge_library_core::legacy::wire::ACTOR), Some(&summary), serde_json::to_value(&r).unwrap_or_default()).await;
    }
    Ok(r)
}

/// Commits that arrived from the vault remote (the owner's own edits, other agents): one activity row
/// each, with the files added / modified / deleted. lumbridge-libraryd's own commits are logged at write time.
async fn record_pulled_commits(st: &AppState, root: &std::path::Path, before: &str, after: &str) {
    let range = format!("{before}..{after}");
    let Ok(log) = gitops::git(root, &["log", "--reverse", "--no-renames", "--format=%x1e%H%x1f%an%x1f%s", "--name-status", &range]).await else {
        return;
    };
    for entry in log.split('\x1e').filter(|e| !e.trim().is_empty()) {
        let mut lines = entry.lines();
        let Some(head) = lines.next() else { continue };
        let mut parts = head.split('\x1f');
        let (sha, author, subject) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if author == lumbridge_library_core::identity::get().git_name {
            continue;
        }
        let mut files = Vec::new();
        let (mut added, mut modified, mut deleted) = (0, 0, 0);
        for l in lines.filter(|l| !l.trim().is_empty()) {
            let (status, path) = l.split_once('\t').unwrap_or(("?", l));
            match status.chars().next() {
                Some('A') => added += 1,
                Some('D') => deleted += 1,
                _ => modified += 1,
            }
            if files.len() < 200 {
                files.push(json!({"status": status, "path": path}));
            }
        }
        let summary = format!("{subject} (+{added} ~{modified} -{deleted})");
        db::record(&st.pool, "commit", "docs", &sha[..sha.len().min(12)], true, Some(author), Some(&summary), json!({"sha": sha, "files": files})).await;
    }
}

pub async fn embed_once(st: &AppState) -> anyhow::Result<(usize, usize)> {
    let mid = st.model_id().await;
    if mid == 0 {
        anyhow::bail!("no embed model registered yet");
    }
    let batch = st.cfg.jobs.embed_batch as usize;
    let d = docs::embed_pending(&st.pool, &st.embedder, mid, batch).await?;
    let w = crate::wh::embed_pending(st, mid, batch).await?;
    let z = lumbridge_library_core::warehouse::design::embed_pending(&st.pool, &st.embedder, mid, batch).await?;
    Ok((d, w + z))
}

pub fn spawn_loops(st: AppState) {
    let j = st.cfg.jobs.clone();
    crate::librarian::spawn(st.clone());
    crate::designs::spawn_caption_loop(st.clone());

    if let Some(mut iv) = every(j.vault_sync_secs) {
        let st = st.clone();
        tokio::spawn(async move {
            if let Err(e) = gitops::ensure_vault(&st.cfg).await {
                warn!("vault checkout: {e:#}");
            }
            loop {
                iv.tick().await;
                if let Err(e) = vault_once(&st).await {
                    warn!("vault sync: {e:#}");
                }
            }
        });
    }

    if let Some(mut iv) = every(j.embed_secs) {
        let st = st.clone();
        tokio::spawn(async move {
            loop {
                iv.tick().await;
                // Drain fully while there is a backlog, then wait for the next tick.
                loop {
                    match embed_once(&st).await {
                        Ok((0, 0)) => break,
                        Ok((d, w)) => info!(docs = d, warehouse = w, "embedded"),
                        Err(e) => {
                            warn!("embed: {e:#}");
                            break;
                        }
                    }
                }
            }
        });
    }

    if let Some(mut iv) = every(j.mirror_sync_secs) {
        let st = st.clone();
        tokio::spawn(async move {
            iv.tick().await; // skip the immediate tick: don't fetch every repo on each restart
            loop {
                iv.tick().await;
                crate::wh::sync_all_job(&st, "schedule").await;
            }
        });
    }

    if let Some(mut iv) = every(j.lint_secs) {
        let st = st.clone();
        tokio::spawn(async move {
            iv.tick().await;
            loop {
                iv.tick().await;
                match docs::lint(&st.pool, &st.cfg.vault_root()).await {
                    Ok(r) => {
                        let summary = json!({
                            "broken_links": r.broken_links.len(), "orphans": r.orphans.len(),
                            "missing_categories": r.missing_categories.len(), "duplicate_titles": r.duplicate_titles.len(),
                            "parse_errors": r.parse_errors.len(), "stale_wiki": r.stale_wiki.len(),
                            "expired_facts": r.expired_facts.len(), "unembedded_chunks": r.unembedded_chunks,
                        });
                        let line = format!(
                            "vault lint: {} broken links, {} orphans, {} missing categories, {} stale wiki pages",
                            r.broken_links.len(), r.orphans.len(), r.missing_categories.len(), r.stale_wiki.len()
                        );
                        db::record(&st.pool, "lint", "docs", "vault", true, Some(lumbridge_library_core::legacy::wire::ACTOR), Some(&line), summary).await;
                    }
                    Err(e) => warn!("lint: {e:#}"),
                }
            }
        });
    }
}
