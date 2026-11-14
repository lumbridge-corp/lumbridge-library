//! Lumbridge Warehouse engine (`lumbridge-warehouse`): mirrors → files/symbols/chunks → embed backlog → code search.
//!
//! Tiers: `mirror` (other people's repos, bare mirrors under `{root}/warehouse/mirrors/<owner>/<name>.git`,
//! never edited), `own` (the owner's repos, also bare mirrors under `warehouse/own/<name>.git`) and
//! `component` (distilled / cleanroom / original pieces, working repos under
//! `warehouse/components/<slug>/`). Every tier is read the same way: through `git` at the repo's
//! last indexed commit.
//!
//! - [`add_mirror`], [`add_own`], [`sync_repo`], [`sync_all`]: clone, fetch, incremental index, repo card.
//! - [`embed_pending`]: the chunk embedding backlog.
//! - [`search`], [`grep`], [`symbols`], [`read`], [`read_symbol`], [`tree`], [`resolve`], [`query`], [`card`]: retrieval.
//! - [`reindex_search_text`] / [`backfill_ranking`]: one-off backfills after migration 0007.
//! - [`create_component`], [`approve_component`], [`retire_component`], [`list_components`],
//!   [`record_usage`]: the component ledger with license rules.

mod card;
mod component;
pub mod design;
pub mod git;
mod index;
pub mod lang;
pub mod license;
pub mod parse;
mod search;
pub mod terms;

pub use card::{CardSummarizer, SummaryFuture};
pub use component::{
    ComponentFilter, ComponentRow, NewComponent, UsageRow, approve_component, create_component, get_component,
    list_components, record_usage, retire_component,
};
pub use index::{
    IndexOpts, SyncOutcome, SyncReport, TermsReport, backfill_ranking, embed_pending, reindex_search_text, sync_all, sync_repo,
};
pub use search::{
    Citation, CodeAnswer, CodeHit, CodeSearchParams, CodeSearchResult, FileView, GrepMatch, GrepParams, GrepResult,
    HitSymbol, RepoRef, SymbolHit, SymbolQuery, SymbolView, TreeEntry, TreeView, card, grep, query, read, read_symbol, resolve,
    search, search_traced, symbols, tree, SearchTrace, TraceCand,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::path::{Path, PathBuf};

pub const TIERS: &[&str] = &["mirror", "own", "component", "design"];

/// One `wh.repos` row.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, sqlx::FromRow)]
pub struct RepoRow {
    pub id: String,
    pub slug: String,
    pub tier: String,
    pub origin_url: Option<String>,
    pub local_path: String,
    pub default_branch: String,
    pub head_commit: Option<String>,
    #[schemars(with = "Option<String>")]
    pub head_committed_at: Option<DateTime<Utc>>,
    pub license_spdx: Option<String>,
    pub license_confidence: Option<f32>,
    pub copyleft: Option<bool>,
    pub description: Option<String>,
    pub stars: Option<i32>,
    #[schemars(with = "Option<String>")]
    pub last_synced_at: Option<DateTime<Utc>>,
    pub last_indexed_commit: Option<String>,
    pub card_commit: Option<String>,
    pub tags: Vec<String>,
    /// Package names found while indexing (root package first).
    pub crates: Vec<String>,
    pub languages: Vec<String>,
    pub file_count: Option<i32>,
    pub health: serde_json::Value,
    pub added_by: String,
    #[schemars(with = "String")]
    pub created_at: DateTime<Utc>,
}

pub(crate) const REPO_COLS: &str = "id::text as id, slug, tier, origin_url, local_path, default_branch, head_commit, \
     head_committed_at, license_spdx, license_confidence, copyleft, description, stars, last_synced_at, \
     last_indexed_commit, card_commit, tags, crates, languages, file_count, health, added_by, created_at";

pub fn warehouse_dir(library_root: &Path) -> PathBuf {
    library_root.join("warehouse")
}

/// Accepts `mirrors/o/n`, `/warehouse/mirrors/o/n`, or a component's bare slug.
pub(crate) fn normalise_slug(s: &str) -> String {
    let s = s.trim().trim_start_matches('/');
    let s = s.strip_prefix("warehouse/").unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

/// Escape `\\`, `%` and `_` for a LIKE pattern (Postgres' default escape character is `\\`).
pub(crate) fn like_escape(p: &str) -> String {
    let mut s = String::with_capacity(p.len() + 2);
    for c in p.chars() {
        if matches!(c, '\\' | '%' | '_') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Exact slug, a bare component slug, or an unambiguous short name (`anyhow` → mirrors/dtolnay/anyhow).
/// Errors when a short name matches several repos.
pub async fn get_repo(pool: &PgPool, slug: &str) -> Result<Option<RepoRow>> {
    let slug = normalise_slug(slug);
    let sql = format!(
        "select {REPO_COLS} from wh.repos where slug = $1 or slug = 'components/' || $1
         order by (slug = $1) desc limit 1"
    );
    if let Some(r) = sqlx::query_as::<_, RepoRow>(&sql).bind(&slug).fetch_optional(pool).await? {
        return Ok(Some(r));
    }
    if slug.is_empty() || slug.contains('/') {
        return Ok(None);
    }
    let sql = format!("select {REPO_COLS} from wh.repos where slug like '%/' || $1 escape '\\' order by slug limit 21");
    let mut rows = sqlx::query_as::<_, RepoRow>(&sql).bind(like_escape(&slug)).fetch_all(pool).await?;
    match rows.len() {
        0 | 1 => Ok(rows.pop()),
        n => {
            let names: Vec<&str> = rows.iter().take(20).map(|r| r.slug.as_str()).collect();
            let more = if n > 20 { ", ..." } else { "" };
            bail!("repo name {slug:?} is ambiguous; use the full slug: {}{more}", names.join(", "))
        }
    }
}

pub(crate) async fn require_repo(pool: &PgPool, slug: &str) -> Result<RepoRow> {
    get_repo(pool, slug).await?.with_context(|| format!("no warehouse repo {slug:?}"))
}

pub async fn list_repos(pool: &PgPool, tier: Option<&str>) -> Result<Vec<RepoRow>> {
    let sql = format!("select {REPO_COLS} from wh.repos where ($1::text is null or tier = $1) order by tier, slug");
    Ok(sqlx::query_as::<_, RepoRow>(&sql).bind(tier).fetch_all(pool).await?)
}

/// Delete a repo's index rows (and optionally its files on disk). Components keep their
/// `wh.components` ledger row (repo_id is set null). Without a library root, files are deleted only
/// when the path has the warehouse layout (`.../warehouse/{mirrors/<o>/<n>.git|own/<n>.git|components/<slug>}`);
/// prefer [`remove_repo_in`], which also requires the path to be inside `{library_root}/warehouse/`.
pub async fn remove_repo(pool: &PgPool, slug: &str, delete_files: bool) -> Result<bool> {
    remove_inner(pool, None, slug, delete_files).await
}

/// [`remove_repo`] that refuses to delete anything not inside `{library_root}/warehouse/`.
pub async fn remove_repo_in(pool: &PgPool, library_root: &Path, slug: &str, delete_files: bool) -> Result<bool> {
    remove_inner(pool, Some(library_root), slug, delete_files).await
}

async fn remove_inner(pool: &PgPool, library_root: Option<&Path>, slug: &str, delete_files: bool) -> Result<bool> {
    let Some(repo) = get_repo(pool, slug).await? else { return Ok(false) };
    let p = PathBuf::from(&repo.local_path);
    // Decide before touching the DB, so a refusal leaves everything as it was.
    let delete_path = if delete_files && p.exists() { Some(deletable_path(library_root, &repo.tier, &p)?) } else { None };
    sqlx::query("delete from wh.repos where id = $1::uuid").bind(&repo.id).execute(pool).await?;
    if let Some(p) = delete_path {
        tokio::fs::remove_dir_all(&p).await.with_context(|| format!("removing {}", p.display()))?;
    }
    Ok(true)
}

/// The canonical path of a repo's files if it is safe to delete: strictly inside the warehouse
/// directory, in the tier's own subtree and layout.
fn deletable_path(library_root: Option<&Path>, tier: &str, path: &Path) -> Result<PathBuf> {
    let canon = std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let refuse = || anyhow::anyhow!("refusing to delete {}: not a repo inside the warehouse directory", canon.display());
    let wh = match library_root {
        Some(root) => {
            let wh = std::fs::canonicalize(warehouse_dir(root)).with_context(|| format!("resolving {}", warehouse_dir(root).display()))?;
            if !canon.starts_with(&wh) || canon == wh {
                return Err(refuse());
            }
            wh
        }
        None => canon.ancestors().find(|a| a.file_name().is_some_and(|n| n == "warehouse")).ok_or_else(refuse)?.to_path_buf(),
    };
    let rel: Vec<String> = canon
        .strip_prefix(&wh)
        .map_err(|_| refuse())?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let rel: Vec<&str> = rel.iter().map(String::as_str).collect();
    let ok = match (tier, rel.as_slice()) {
        ("mirror", ["mirrors", o, n]) => safe_name(o).is_ok() && n.ends_with(".git") && safe_name(n).is_ok(),
        ("own", ["own", n]) => n.ends_with(".git") && safe_name(n).is_ok(),
        ("component", ["components", n]) => safe_name(n).is_ok(),
        _ => false,
    };
    if !ok {
        return Err(refuse());
    }
    Ok(canon)
}

/// (owner, name) from a git URL or path: `https://github.com/o/n(.git)`, `git@host:o/n.git`, `/x/o/n`.
pub fn owner_and_name(url: &str) -> Result<(String, String)> {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let path = if let Some((_, rest)) = u.split_once("://") {
        rest.split_once('/').map(|(_, p)| p).unwrap_or("")
    } else if let Some((host, rest)) = u.split_once(':')
        && host.contains('@')
    {
        rest
    } else {
        u
    };
    let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != "." && *p != "..").collect();
    let name = parts.pop().context("no repo name in URL")?;
    let owner = parts.pop().unwrap_or("local");
    let ok = |s: &str| {
        !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    if !ok(owner) || !ok(name) {
        bail!("cannot derive a safe owner/name from {url:?}");
    }
    Ok((owner.to_string(), name.to_string()))
}

fn safe_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        bail!("invalid name {name:?}: use [A-Za-z0-9._-]");
    }
    Ok(())
}

/// Clone `--mirror` into `dest` unless a repo is already there. Clones into a temp dir first.
async fn ensure_mirror(url: &str, dest: &Path, t: git::Transport) -> Result<bool> {
    if git::is_git_dir(dest).await {
        let existing = git::origin_url(dest).await?;
        if existing.as_deref() != Some(url) {
            bail!(
                "{} already mirrors {}; refusing to re-register it with origin {url:?} (remove the repo with delete_files first)",
                dest.display(),
                existing.as_deref().unwrap_or("(no origin)")
            );
        }
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = dest.with_extension(format!("tmp-{}", std::process::id()));
    if tmp.exists() {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
    if let Err(e) = git::clone_mirror(url, &tmp, t).await {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(e);
    }
    if dest.exists() {
        tokio::fs::remove_dir_all(dest).await?;
    }
    tokio::fs::rename(&tmp, dest).await?;
    Ok(true)
}

/// Register (and clone) another person's repo as a bare mirror. Idempotent; does not index
/// (call [`sync_repo`]).
pub async fn add_mirror(pool: &PgPool, library_root: &Path, url: &str, tags: &[String], added_by: &str) -> Result<RepoRow> {
    let url = url.trim();
    check_mirror_url(url)?;
    let (owner, name) = owner_and_name(url)?;
    let slug = format!("mirrors/{owner}/{name}");
    let dest = warehouse_dir(library_root).join("mirrors").join(&owner).join(format!("{name}.git"));
    add_bare(pool, &slug, "mirror", url, &dest, tags, added_by, git::Transport::Remote).await
}

/// A mirror's origin must be remote: `https://`, `ssh://` or `git@host:path`. Local paths, `file://`,
/// `ext::` and other transports are refused (local paths are only for [`add_own`]).
fn check_mirror_url(url: &str) -> Result<()> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("mirror URL {url:?} contains whitespace or control characters");
    }
    let lower = url.to_ascii_lowercase();
    let host = if let Some(rest) = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("ssh://")) {
        let authority = rest.split('/').next().unwrap_or("");
        authority.rsplit('@').next().unwrap_or("").to_string()
    } else if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
        && !host.contains('/')
        && !path.starts_with('/')
    {
        host.to_string()
    } else {
        bail!("mirror URL {url:?} must be https://, ssh:// or git@host:owner/name (local paths only via add_own)");
    };
    if host.is_empty() || host.starts_with('-') {
        bail!("mirror URL {url:?} has no valid host");
    }
    Ok(())
}

/// Register one of the owner's repos (URL or local path). Also a bare mirror clone, read like a mirror.
pub async fn add_own(
    pool: &PgPool,
    library_root: &Path,
    git_url_or_local_path: &str,
    name: Option<&str>,
    tags: &[String],
) -> Result<RepoRow> {
    let src = git_url_or_local_path.trim();
    let local = Path::new(src);
    let url = if local.exists() {
        std::fs::canonicalize(local)?.to_string_lossy().into_owned()
    } else {
        src.to_string()
    };
    let name = match name {
        Some(n) => n.to_string(),
        None => owner_and_name(&url)?.1,
    };
    safe_name(&name)?;
    let slug = format!("own/{name}");
    let dest = warehouse_dir(library_root).join("own").join(format!("{name}.git"));
    add_bare(pool, &slug, "own", &url, &dest, tags, &crate::identity::get().owner, git::Transport::AllowLocal).await
}

#[allow(clippy::too_many_arguments)]
async fn add_bare(
    pool: &PgPool,
    slug: &str,
    tier: &str,
    url: &str,
    dest: &Path,
    tags: &[String],
    added_by: &str,
    t: git::Transport,
) -> Result<RepoRow> {
    let _cloned = ensure_mirror(url, dest, t).await.with_context(|| format!("cloning {url}"))?;
    let branch = git::default_branch(dest).await.unwrap_or_else(|_| "main".into());
    let head = git::head(dest).await?;
    let (lic, committed_at) = match &head {
        Some(h) => (Some(index::detect_license(dest, h).await?), git::commit_date(dest, h).await?),
        None => (None, None),
    };
    let lic = lic.unwrap_or(license::LicenseInfo { spdx: None, confidence: None, copyleft: None, sources: vec![] });
    sqlx::query(
        "insert into wh.repos (slug, tier, origin_url, local_path, default_branch, head_commit, head_committed_at,
                               license_spdx, license_confidence, copyleft, tags, added_by)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         on conflict (slug) do update set
           origin_url = excluded.origin_url, local_path = excluded.local_path,
           default_branch = excluded.default_branch, head_commit = excluded.head_commit,
           head_committed_at = excluded.head_committed_at,
           license_spdx = excluded.license_spdx, license_confidence = excluded.license_confidence,
           copyleft = excluded.copyleft,
           tags = array(select distinct t from unnest(wh.repos.tags || excluded.tags) t order by t)",
    )
    .bind(slug)
    .bind(tier)
    .bind(url)
    .bind(dest.to_string_lossy().as_ref())
    .bind(&branch)
    .bind(&head)
    .bind(committed_at)
    .bind(&lic.spdx)
    .bind(lic.confidence)
    .bind(lic.copyleft)
    .bind(tags)
    .bind(added_by)
    .execute(pool)
    .await?;
    require_repo(pool, slug).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_from_urls() {
        assert_eq!(owner_and_name("https://github.com/tokio-rs/tokio").unwrap(), ("tokio-rs".into(), "tokio".into()));
        assert_eq!(owner_and_name("https://github.com/BurntSushi/memchr.git/").unwrap(), ("BurntSushi".into(), "memchr".into()));
        assert_eq!(owner_and_name("git@github.com:dtolnay/anyhow.git").unwrap(), ("dtolnay".into(), "anyhow".into()));
        assert_eq!(owner_and_name("/tmp/x/fixture").unwrap(), ("x".into(), "fixture".into()));
        assert_eq!(normalise_slug("/warehouse/mirrors/a/b"), "mirrors/a/b");
        assert!(owner_and_name("https://github.com/.hidden/x").is_err());
        assert!(owner_and_name("https://github.com/o/.x").is_err());    }

    #[test]
    fn like_escaping() {
        assert_eq!(like_escape("a_b%c\\d"), "a\\_b\\%c\\\\d");
        assert_eq!(like_escape("plain-name.rs"), "plain-name.rs");
    }

    #[test]
    fn mirror_url_schemes() {
        for ok in ["https://github.com/o/n", "ssh://git@github.com/o/n.git", "git@github.com:o/n.git", "HTTPS://github.com/o/n"] {
            assert!(check_mirror_url(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "/tmp/x/y",
            "./x/y",
            "x/y",
            "file:///tmp/x/y",
            "FILE:///tmp/x/y",
            "ext::sh -c id",
            "git://github.com/o/n",
            "http://github.com/o/n",
            "git@host:/abs/path",
            "ssh://-oProxyCommand=x/o/n",
            "evil@host:o/n",
            "https:///o/n",
        ] {
            assert!(check_mirror_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn delete_only_inside_warehouse() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        let m = root.join("warehouse/mirrors/o/n.git");
        let own = root.join("warehouse/own/x.git");
        let comp = root.join("warehouse/components/c");
        let outside = tmp.path().join("elsewhere/o/n.git");
        for d in [&m, &own, &comp, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        let r = Some(root.as_path());
        assert!(deletable_path(r, "mirror", &m).is_ok());
        assert!(deletable_path(r, "own", &own).is_ok());
        assert!(deletable_path(r, "component", &comp).is_ok());
        assert!(deletable_path(None, "mirror", &m).is_ok());
        assert!(deletable_path(r, "mirror", &outside).is_err());
        assert!(deletable_path(r, "mirror", &root.join("warehouse")).is_err());
        assert!(deletable_path(r, "mirror", &root.join("warehouse/mirrors")).is_err());
        assert!(deletable_path(r, "own", &m).is_err(), "tier subtree must match");
        assert!(deletable_path(r, "mirror", &root.join("warehouse/mirrors/o/../../../..")).is_err());
        assert!(deletable_path(None, "mirror", &outside).is_err());
        // A symlink inside the warehouse that points outside it.
        let link = root.join("warehouse/mirrors/o/evil.git");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(deletable_path(r, "mirror", &link).is_err());
    }
}
