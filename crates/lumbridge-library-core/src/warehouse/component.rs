//! Components: reusable pieces with a license trail.
//!
//! - `distilled`: copied / adapted from a warehouse repo, with attribution (NOTICE) and the source's
//!   license. Refused when the source is copyleft or its license is unknown: use `cleanroom`.
//! - `cleanroom`: reimplemented from a behaviour spec (SPEC.md) by an agent that never read the
//!   source. The spec is required and recorded as `spec_path`.
//! - `original`: written from scratch.
//!
//! Each component is a git repo at `{root}/warehouse/components/<slug>/`, indexed like any repo
//! (tier `component`, slug `components/<slug>`).

use super::index::{IndexOpts, sync_repo};
use super::license;
use super::{git, require_repo, warehouse_dir};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::path::{Component as PathComponent, Path};

pub const ORIGINS: &[&str] = &["distilled", "cleanroom", "original"];

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NewComponent {
    /// `[a-z0-9][a-z0-9._-]*`, e.g. `sse-stream-parser`.
    pub slug: String,
    pub summary: String,
    #[serde(default)]
    pub languages: Vec<String>,
    /// distilled | cleanroom | original
    pub origin: String,
    /// Warehouse repo slug it came from (required for distilled).
    #[serde(default)]
    pub source_repo: Option<String>,
    /// `commit:path` in the source repo.
    #[serde(default)]
    pub source_ref: Option<String>,
    /// Required. For distilled: equal to (or one OR-alternative of) the source's license.
    pub license_spdx: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// (relative path, content).
    #[serde(default)]
    pub files: Vec<(String, String)>,
    /// The behaviour spec (required for cleanroom).
    #[serde(default)]
    pub spec_md: Option<String>,
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, sqlx::FromRow)]
pub struct ComponentRow {
    pub id: String,
    pub slug: String,
    pub summary: String,
    pub languages: Vec<String>,
    pub origin: String,
    pub source_repo: Option<String>,
    pub source_ref: Option<String>,
    pub spec_path: Option<String>,
    pub license_spdx: String,
    pub tags: Vec<String>,
    pub status: String,
    /// `components/<slug>` in wh.repos (None if that row was removed).
    pub repo_slug: Option<String>,
    pub local_path: Option<String>,
    pub uses: i64,
    #[schemars(with = "String")]
    pub created_at: DateTime<Utc>,
    #[schemars(with = "Option<String>")]
    pub approved_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComponentFilter {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    /// Include retired components (default false unless status = retired).
    #[serde(default)]
    pub include_retired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageRow {
    pub id: i64,
    pub component: String,
    pub project: String,
    pub commit: Option<String>,
    pub added_by: String,
}

const COMPONENT_SELECT: &str = "select c.id::text as id, c.slug, c.summary, c.languages, c.origin, c.source_repo, c.source_ref,
        c.spec_path, c.license_spdx, c.tags, c.status, r.slug as repo_slug, r.local_path,
        (select count(*) from wh.usage u where u.component_id = c.id) as uses, c.created_at, c.approved_at
   from wh.components c left join wh.repos r on r.id = c.repo_id";

fn valid_slug(s: &str) -> bool {
    let mut ch = s.chars();
    matches!(ch.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.len() <= 80
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
}

fn valid_rel_path(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty()
        && path.components().all(|c| matches!(c, PathComponent::Normal(_)))
        && !path.components().any(|c| c.as_os_str() == ".git")
}

/// Files the warehouse writes itself at a component's root (case-insensitive).
fn is_reserved_root_file(p: &str) -> bool {
    let mut comps = Path::new(p).components();
    let (Some(PathComponent::Normal(first)), None) = (comps.next(), comps.next()) else { return false };
    let Some(name) = first.to_str() else { return false };
    let l = name.to_ascii_lowercase();
    matches!(l.as_str(), "component.toml" | "notice" | "spec.md" | "readme.md") || license::is_license_file(&l)
}

#[derive(Serialize)]
struct ComponentToml<'a> {
    slug: &'a str,
    summary: &'a str,
    origin: &'a str,
    license: &'a str,
    languages: &'a [String],
    tags: &'a [String],
    status: &'a str,
    created_by: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<SourceToml<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spec: Option<&'a str>,
}

#[derive(Serialize)]
struct SourceToml<'a> {
    repo: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    r#ref: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<&'a str>,
}

/// Validate the license rules, write the component repo, commit, register and index it.
pub async fn create_component(pool: &PgPool, library_root: &Path, c: NewComponent) -> Result<ComponentRow> {
    if !valid_slug(&c.slug) {
        bail!("invalid component slug {:?}: use [a-z0-9][a-z0-9._-]*", c.slug);
    }
    if !ORIGINS.contains(&c.origin.as_str()) {
        bail!("origin must be one of {ORIGINS:?}");
    }
    if c.summary.trim().is_empty() {
        bail!("summary is required");
    }
    let lic = license::normalise_expr(&c.license_spdx).context("license_spdx is required")?;
    if c.created_by.trim().is_empty() {
        bail!("created_by is required");
    }
    for (p, _) in &c.files {
        if !valid_rel_path(p) {
            bail!("invalid file path {p:?}");
        }
        if is_reserved_root_file(p) {
            bail!(
                "file {p:?} is reserved: component.toml, README.md, SPEC.md, NOTICE and LICENSE*/COPYING* at the \
                 component root are written by the warehouse (pass the spec as spec_md)"
            );
        }
    }

    // Source repo (required for distilled; recorded for cleanroom when given).
    let source = match &c.source_repo {
        Some(s) if !s.trim().is_empty() => Some(require_repo(pool, s).await?),
        _ => None,
    };
    let mut spec_path = None;
    match c.origin.as_str() {
        "distilled" => {
            let src = source.as_ref().context("a distilled component needs source_repo")?;
            let Some(src_lic) = src.license_spdx.as_deref() else {
                bail!(
                    "refusing distilled component: source {} has no detected license (all rights reserved). \
                     Write a behaviour spec and create a cleanroom component instead.",
                    src.slug
                );
            };
            if src.copyleft == Some(true) || license::is_copyleft(src_lic) {
                bail!(
                    "refusing distilled component: source {} is copyleft ({src_lic}). \
                     Write a behaviour spec and create a cleanroom component instead.",
                    src.slug
                );
            }
            if !license::compatible(src_lic, &lic) {
                bail!("license {lic:?} is not compatible with the source license {src_lic:?}; a distilled component inherits it");
            }
        }
        "cleanroom" => {
            if c.spec_md.as_deref().is_none_or(|s| s.trim().is_empty()) {
                bail!("a cleanroom component requires spec_md: the behaviour spec the implementer worked from");
            }
            spec_path = Some("SPEC.md".to_string());
        }
        _ => {}
    }
    if license::is_copyleft(&lic) && c.origin != "distilled" {
        tracing::info!(slug = %c.slug, "component declared under a copyleft license: {lic}");
    }

    let exists: bool = sqlx::query_scalar("select exists(select 1 from wh.components where slug = $1)")
        .bind(&c.slug)
        .fetch_one(pool)
        .await?;
    if exists {
        bail!("component {} already exists", c.slug);
    }
    let dir = warehouse_dir(library_root).join("components").join(&c.slug);
    if dir.exists() {
        bail!("{} already exists on disk", dir.display());
    }
    tokio::fs::create_dir_all(&dir).await?;
    let res = write_and_register(pool, &dir, &c, &lic, source.as_ref(), spec_path.as_deref()).await;
    if res.is_err() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let _ = sqlx::query("delete from wh.components where slug = $1 and status = 'draft'").bind(&c.slug).execute(pool).await;
        let _ = sqlx::query("delete from wh.repos where slug = $1").bind(format!("components/{}", c.slug)).execute(pool).await;
    }
    res?;
    let opts = IndexOpts { fetch: false, ..Default::default() };
    sync_repo(pool, library_root, &format!("components/{}", c.slug), &opts).await?;
    get_component(pool, &c.slug).await?.context("component vanished")
}

async fn write_and_register(
    pool: &PgPool,
    dir: &Path,
    c: &NewComponent,
    lic: &str,
    source: Option<&super::RepoRow>,
    spec_path: Option<&str>,
) -> Result<()> {
    let src_toml = source.map(|s| SourceToml {
        repo: &s.slug,
        r#ref: c.source_ref.as_deref(),
        url: s.origin_url.as_deref().filter(|_| s.tier == "mirror"),
        license: s.license_spdx.as_deref(),
    });
    let manifest = ComponentToml {
        slug: &c.slug,
        summary: c.summary.trim(),
        origin: &c.origin,
        license: lic,
        languages: &c.languages,
        tags: &c.tags,
        status: "draft",
        created_by: &c.created_by,
        source: src_toml,
        spec: spec_path,
    };
    tokio::fs::write(dir.join("component.toml"), toml::to_string_pretty(&manifest)?).await?;
    {
        let mut readme = format!("# {}\n\n{}\n\n- Origin: {}\n- License: {lic}\n", c.slug, c.summary.trim(), c.origin);
        if let Some(s) = source {
            readme.push_str(&format!("- Source: {}{}\n", s.slug, c.source_ref.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()));
        }
        if spec_path.is_some() {
            readme.push_str("- Behaviour spec: [SPEC.md](SPEC.md). Implemented without reading the source.\n");
        }
        tokio::fs::write(dir.join("README.md"), readme).await?;
    }
    if let (Some(p), Some(spec)) = (spec_path, &c.spec_md) {
        tokio::fs::write(dir.join(p), spec).await?;
    }
    if c.origin == "distilled"
        && let Some(s) = source
    {
        let notice = format!(
            "This component contains code adapted from {}{}{}.\n\
             The original is licensed under {}; this component inherits that license ({lic}).\n\
             Original copyright notices are retained in the files that carry them; see LICENSE* for the upstream text.\n",
            s.slug,
            s.origin_url.as_deref().filter(|_| s.tier == "mirror").map(|u| format!(" <{u}>")).unwrap_or_default(),
            c.source_ref.as_deref().map(|r| format!(" at {r}")).unwrap_or_default(),
            s.license_spdx.as_deref().unwrap_or("?"),
        );
        tokio::fs::write(dir.join("NOTICE"), notice).await?;
        // Carry the upstream license text(s) along.
        if let Some(commit) = s.last_indexed_commit.as_deref().or(s.head_commit.as_deref()) {
            let src_dir = Path::new(&s.local_path);
            let names = git::run(Some(src_dir), &["ls-tree", "--name-only", commit], git::SHORT).await?;
            for n in names.lines().filter(|n| license::is_license_file(n)) {
                match git::show(src_dir, commit, n, git::META_MAX_BYTES).await? {
                    git::Blob::Data(b) => tokio::fs::write(dir.join(n), b).await?,
                    git::Blob::TooLarge(size) => {
                        tracing::warn!(source = %s.slug, file = n, size, "upstream license file over the size cap; not copied")
                    }
                    git::Blob::Missing => {}
                }
            }
        }
    }
    for (p, content) in &c.files {
        let dest = dir.join(p);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&dest, content).await?;
    }
    let head = git::init_and_commit(dir, &c.created_by, &format!("{}: {} component\n\n{}", c.slug, c.origin, c.summary.trim())).await?;

    let mut tx = pool.begin().await?;
    let repo_id: String = sqlx::query_scalar(
        "insert into wh.repos (slug, tier, origin_url, local_path, default_branch, head_commit, license_spdx,
                               license_confidence, copyleft, description, tags, added_by)
         values ($1, 'component', null, $2, 'main', $3, $4, 1.0, $5, $6, $7, $8)
         returning id::text",
    )
    .bind(format!("components/{}", c.slug))
    .bind(dir.to_string_lossy().as_ref())
    .bind(&head)
    .bind(lic)
    .bind(license::is_copyleft(lic))
    .bind(c.summary.trim())
    .bind(&c.tags)
    .bind(&c.created_by)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "insert into wh.components (slug, summary, languages, origin, source_repo, source_ref, spec_path, license_spdx, tags, repo_id)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::uuid)",
    )
    .bind(&c.slug)
    .bind(c.summary.trim())
    .bind(&c.languages)
    .bind(&c.origin)
    .bind(source.map(|s| s.slug.as_str()))
    .bind(&c.source_ref)
    .bind(spec_path)
    .bind(lic)
    .bind(&c.tags)
    .bind(&repo_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

fn bare(slug: &str) -> &str {
    let s = slug.trim().trim_start_matches('/');
    let s = s.strip_prefix("warehouse/").unwrap_or(s);
    s.strip_prefix("components/").unwrap_or(s)
}

pub async fn get_component(pool: &PgPool, slug: &str) -> Result<Option<ComponentRow>> {
    let sql = format!("{COMPONENT_SELECT} where c.slug = $1");
    Ok(sqlx::query_as::<_, ComponentRow>(&sql).bind(bare(slug)).fetch_optional(pool).await?)
}

async fn set_status(pool: &PgPool, slug: &str, status: &str) -> Result<ComponentRow> {
    let n = sqlx::query(
        "update wh.components set status = $2,
           approved_at = case when $2 = 'approved' then now() else approved_at end
         where slug = $1",
    )
    .bind(bare(slug))
    .bind(status)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        bail!("no component {slug:?}");
    }
    get_component(pool, slug).await?.context("component vanished")
}

pub async fn approve_component(pool: &PgPool, slug: &str) -> Result<ComponentRow> {
    set_status(pool, slug, "approved").await
}

pub async fn retire_component(pool: &PgPool, slug: &str) -> Result<ComponentRow> {
    set_status(pool, slug, "retired").await
}

pub async fn list_components(pool: &PgPool, f: &ComponentFilter) -> Result<Vec<ComponentRow>> {
    let sql = format!(
        "{COMPONENT_SELECT}
          where ($1::text is null or c.status = $1)
            and ($2::text is null or c.origin = $2)
            and ($3::text is null or $3 = any(c.tags))
            and ($4::text is null or $4 = any(c.languages))
            and ($5 or $1::text is not null or c.status <> 'retired')
          order by c.status = 'approved' desc, c.slug"
    );
    Ok(sqlx::query_as::<_, ComponentRow>(&sql)
        .bind(&f.status)
        .bind(&f.origin)
        .bind(&f.tag)
        .bind(&f.language)
        .bind(f.include_retired)
        .fetch_all(pool)
        .await?)
}

/// Record that a component was copied into a project (signal for which ones earn their keep).
pub async fn record_usage(pool: &PgPool, slug: &str, project: &str, commit: Option<&str>, added_by: &str) -> Result<UsageRow> {
    let comp = get_component(pool, slug).await?.with_context(|| format!("no component {slug:?}"))?;
    if comp.status == "retired" {
        bail!("component {} is retired", comp.slug);
    }
    let id: i64 = sqlx::query_scalar(
        "insert into wh.usage (component_id, project, commit, added_by) values ($1::uuid, $2, $3, $4) returning id",
    )
    .bind(&comp.id)
    .bind(project)
    .bind(commit)
    .bind(added_by)
    .fetch_one(pool)
    .await?;
    Ok(UsageRow { id, component: comp.slug, project: project.to_string(), commit: commit.map(String::from), added_by: added_by.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        assert!(valid_slug("sse-stream-parser"));
        assert!(!valid_slug("Bad"));
        assert!(!valid_slug("../x"));
        assert!(valid_rel_path("src/lib.rs"));
        assert!(!valid_rel_path("../x"));
        assert!(!valid_rel_path("/etc/passwd"));
        assert!(!valid_rel_path(".git/config"));
        assert_eq!(bare("/warehouse/components/x"), "x");
    }

    #[test]
    fn reserved_component_files() {
        for p in [
            "component.toml",
            "Component.TOML",
            "NOTICE",
            "notice",
            "SPEC.md",
            "spec.MD",
            "README.md",
            "readme.md",
            "LICENSE",
            "LICENSE-MIT",
            "license.txt",
            "Licence",
            "COPYING",
            "README.md/",
        ] {
            assert!(is_reserved_root_file(p), "{p:?} should be reserved");
        }
        for p in ["src/README.md", "docs/LICENSE", "a/NOTICE", "src/lib.rs", "READ.md", "SPEC.txt", "component.json"] {
            assert!(!is_reserved_root_file(p), "{p:?} should be allowed");
        }
    }
}
