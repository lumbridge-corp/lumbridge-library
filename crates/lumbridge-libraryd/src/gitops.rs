//! Git + Gitea plumbing owned by lumbridge-libraryd: the vault checkout (clone / pull / push) and publishing
//! warehouse components as private repos in the Gitea `warehouse` org.

use anyhow::{Context, Result, bail};
use lumbridge_library_core::config::{Config, GiteaConfig};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

pub async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(300),
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output(),
    )
    .await
    .context("git timed out")??;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Make sure `{library_root}/vault` is a checkout of the configured remote. Idempotent.
pub async fn ensure_vault(cfg: &Config) -> Result<()> {
    let root = cfg.vault_root();
    if root.join(".git").exists() {
        return Ok(());
    }
    let Some(remote) = &cfg.vault.remote else {
        std::fs::create_dir_all(&root)?;
        git(&root, &["init", "-q", "-b", &cfg.vault.branch]).await?;
        return Ok(());
    };
    let empty = std::fs::read_dir(&root).map(|mut d| d.next().is_none()).unwrap_or(true);
    if !empty {
        bail!("{} exists, is not a git checkout and is not empty", root.display());
    }
    let parent = root.parent().context("vault root has no parent")?;
    std::fs::create_dir_all(parent)?;
    git(parent, &["clone", "-q", remote, root.to_str().context("non-utf8 vault path")?]).await?;
    configure_identity(&root).await
}

async fn configure_identity(dir: &Path) -> Result<()> {
    let id = lumbridge_library_core::identity::get();
    git(dir, &["config", "user.name", &id.git_name]).await?;
    git(dir, &["config", "user.email", &id.git_email]).await?;
    Ok(())
}

/// Create (if needed) `warehouse/<slug>` on Gitea as a private repo and push the component's
/// local git repo to it. Returns the Gitea full name.
pub async fn publish_component(g: &GiteaConfig, slug: &str, summary: &str, dir: &Path) -> Result<String> {
    let token = std::fs::read_to_string(&g.token_file)
        .with_context(|| format!("reading {}", g.token_file.display()))?;
    let name = slug.rsplit('/').next().unwrap_or(slug);
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let url = format!("{}/api/v1/orgs/{}/repos", g.api_base.trim_end_matches('/'), g.component_org);
    let desc: String = summary.chars().take(250).collect();
    let r = http
        .post(&url)
        .header("Authorization", format!("token {}", token.trim()))
        .json(&serde_json::json!({ "name": name, "private": true, "description": desc, "default_branch": "main" }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let code = r.status();
    if !(code.is_success() || code.as_u16() == 409) {
        bail!("Gitea create repo {name}: {code}: {}", r.text().await.unwrap_or_default());
    }
    let full = format!("{}/{}", g.component_org, name);
    let remote = format!("{}:{}.git", g.ssh_host, full);
    let _ = git(dir, &["remote", "remove", "gitea"]).await;
    git(dir, &["remote", "add", "gitea", &remote]).await?;
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    git(dir, &["push", "-q", "-u", "gitea", &format!("{branch}:main")]).await?;
    Ok(full)
}
