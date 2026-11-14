//! Agent writes into the vault: Wiki/, References/ and Clippings/ only, and only over files an agent
//! wrote (never the owner's: see [`crate::identity`]). Every write appends to `log.md`, is committed
//! with git under the daemon's identity (`[identity] git_name` / `git_email`), and re-synced.

use super::frontmatter::{parse_document, render};
use super::sync::{SyncReport, clean_rel, sync_file};
use super::WRITABLE_FOLDERS;
use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::PgPool;
use std::path::Path;

/// One writer at a time per process (git index, log.md).
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The daemon's git identity. The vault pull job keys on the name to tell its own commits apart.
fn bot() -> (&'static str, &'static str) {
    let id = crate::identity::get();
    (&id.git_name, &id.git_email)
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WriteDoc {
    /// Vault-relative path under Wiki/, References/ or Clippings/ (`.md` added if missing).
    pub path: String,
    /// Defaults to the filename stem.
    #[serde(default)]
    pub title: Option<String>,
    /// One of the configured agent authors (`[identity] agents`); never the owner.
    pub author: String,
    /// Extra frontmatter fields (categories, tags, source, status, entity, ...).
    #[serde(default)]
    pub frontmatter: Option<Map<String, Value>>,
    pub body: String,
    /// Commit message / log.md note.
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WriteOutcome {
    pub path: String,
    pub created: bool,
    /// Commit hash, or None when the vault is not a git checkout.
    pub commit: Option<String>,
    pub sync: SyncReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Clip {
    pub url: String,
    pub title: String,
    pub body: String,
    /// One of the configured agent authors (`[identity] agents`).
    pub author: String,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
}

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// Characters that break filenames or Obsidian links.
const BAD_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\\', '[', ']', '#', '^'];

/// Validate a write target: vault-relative, `.md`, inside a writable folder, filename-safe.
pub(crate) fn sanitize_write_path(p: &str) -> Result<String> {
    let mut rel = clean_rel(p)?;
    match Path::new(&rel).extension().and_then(|e| e.to_str()) {
        Some("md") => {}
        None => rel.push_str(".md"),
        Some(other) => bail!("only .md files can be written (got .{other})"),
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() < 2 || !WRITABLE_FOLDERS.contains(&parts[0]) {
        bail!("agents may only write under {}; got {rel:?}", WRITABLE_FOLDERS.join("/, ") + "/");
    }
    for part in &parts {
        if part.starts_with('.') || part.trim() != *part || part.is_empty() {
            bail!("bad path component {part:?}");
        }
        if part.chars().any(|c| c.is_control() || BAD_CHARS.contains(&c)) {
            bail!("path component {part:?} has characters not allowed in vault filenames");
        }
        if part.len() > 200 {
            bail!("path component too long");
        }
    }
    if parts.last().is_some_and(|f| *f == ".md") {
        bail!("empty filename");
    }
    Ok(rel)
}

/// A filename from a title: unsafe characters → `-`, whitespace collapsed, ≤120 chars.
pub fn safe_filename(title: &str) -> String {
    let s: String = title.chars().map(|c| if c.is_control() || BAD_CHARS.contains(&c) || c == '/' { '-' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_matches(|c: char| c == '.' || c == '-' || c == ' ');
    let s = super::truncate_chars(s, 120).trim().to_string();
    if s.is_empty() { "untitled".into() } else { s }
}

/// Default-deny ownership check: an existing file may be modified only when its frontmatter names
/// a configured agent as the single author. Anything else — the owner, `"[[Owner]]"`, a list, a
/// full name, no author, unparseable frontmatter without a clear agent author line — is treated as
/// the owner's (or someone's) and left alone.
pub fn agent_owned(text: &str) -> bool {
    let is_agent = |v: &str| {
        let v = super::frontmatter::unlink(v.trim().trim_matches(['"', '\''])).to_lowercase();
        crate::identity::get().is_agent(&v)
    };
    let (fm, _, err) = parse_document(text);
    if err.is_none() {
        return match fm.raw.get("author") {
            Some(Value::String(a)) => is_agent(a),
            _ => false,
        };
    }
    // Unparseable frontmatter: accept only an explicit single agent author line.
    let mut authors = text.lines().take(80).filter_map(|l| l.trim().strip_prefix("author:").map(str::to_string));
    match (authors.next(), authors.next()) {
        (Some(a), None) => is_agent(&a),
        _ => false,
    }
}

/// Refuse a path that differs only by case from an existing file (APFS on the Mac would merge them).
async fn check_case_collision(vault_root: &Path, rel: &str) -> Result<()> {
    let full = vault_root.join(rel);
    let (Some(dir), Some(name)) = (full.parent(), full.file_name().and_then(|n| n.to_str())) else { return Ok(()) };
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return Ok(()) };
    let lower = name.to_lowercase();
    while let Ok(Some(e)) = rd.next_entry().await {
        if let Some(n) = e.file_name().to_str()
            && n != name
            && n.to_lowercase() == lower
        {
            bail!("refusing to write {rel}: {n:?} already exists in that folder (differs only by case)");
        }
    }
    Ok(())
}

const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let fut = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=2")
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(GIT_TIMEOUT, fut)
        .await
        .with_context(|| format!("git {} timed out", args.join(" ")))?
        .context("running git")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        bail!("git {} failed: {}{}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim(), if stdout.is_empty() { String::new() } else { format!(" ({stdout})") });
    }
    Ok(stdout)
}

async fn append_log(root: &Path, line: &str) -> Result<()> {
    let p = root.join("log.md");
    let mut text = tokio::fs::read_to_string(&p).await.unwrap_or_else(|_| "# Log\n".to_string());
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push('\n');
    text.push_str(line);
    text.push('\n');
    write_atomic(&p, &text).await
}

async fn write_atomic(p: &Path, text: &str) -> Result<()> {
    let dir = p.parent().context("no parent dir")?;
    tokio::fs::create_dir_all(dir).await?;
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    tokio::fs::write(&tmp, text).await.with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, p).await.with_context(|| format!("renaming into {}", p.display()))?;
    Ok(())
}

/// Write (create or replace) an agent-owned page, log it, commit it, and re-sync it.
pub async fn write_doc(pool: &PgPool, vault_root: &Path, w: WriteDoc) -> Result<WriteOutcome> {
    let author = w.author.trim().to_lowercase();
    let id = crate::identity::get();
    if id.is_owner(&author) {
        bail!("agents cannot write as the owner ({})", id.owner);
    }
    if !id.is_agent(&author) {
        bail!("author must be {}", id.agents_list());
    }
    let rel = sanitize_write_path(&w.path)?;
    let full = vault_root.join(&rel);
    let _guard = WRITE_LOCK.lock().await;
    check_case_collision(vault_root, &rel).await?;

    let existing = match tokio::fs::read_to_string(&full).await {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {rel}")),
    };
    let mut created_date = today();
    let mut status = None;
    // On a rewrite, existing frontmatter fields the caller does not set are kept.
    let mut extras = Map::new();
    if let Some(text) = &existing {
        if !agent_owned(text) {
            bail!("refusing to modify {rel}: it is not agent-authored (author must be {}); the owner's files are never edited", id.agents_list());
        }
        let (fm, _, _) = parse_document(text);
        if let Some(c) = fm.created {
            created_date = c;
        }
        status = fm.status;
        extras = fm.raw;
        extras.remove("created");
        extras.remove("status");
    }
    extras.extend(w.frontmatter.clone().unwrap_or_default());
    for k in ["title", "author", "updated"] {
        extras.remove(k);
    }
    if let Some(c) = extras.remove("created").and_then(|v| v.as_str().and_then(|s| NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").ok())) {
        created_date = c;
    }
    let status = extras
        .remove("status")
        .and_then(|v| v.as_str().map(str::to_lowercase))
        .or(status)
        .filter(|s| matches!(s.as_str(), "raw" | "draft" | "reviewed"))
        .unwrap_or_else(|| "draft".into());
    let title = w
        .title
        .clone()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| super::stem(&rel).to_string());

    let mut fields: Vec<(String, Value)> = vec![
        ("title".into(), Value::from(title)),
        ("author".into(), Value::from(author)),
        ("status".into(), Value::from(status)),
        ("created".into(), Value::from(created_date.to_string())),
        ("updated".into(), Value::from(today().to_string())),
    ];
    fields.extend(extras);
    let text = format!("{}\n{}\n", render(&fields)?, w.body.trim());
    write_atomic(&full, &text).await?;

    let message = w.message.clone().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| format!("write {rel}"));
    let message = message.lines().next().unwrap_or("").trim().to_string();
    append_log(vault_root, &format!("## [{}] write | {rel} — {message}", today())).await?;

    let commit = if vault_root.join(".git").exists() {
        git(vault_root, &["add", "--", &rel, "log.md"]).await?;
        let (bot_name, bot_email) = bot();
        let author_arg = format!("{bot_name} <{bot_email}>");
        git(
            vault_root,
            &[
                "-c",
                &format!("user.name={bot_name}"),
                "-c",
                &format!("user.email={bot_email}"),
                "commit",
                "-q",
                "--no-verify",
                "-m",
                &message,
                "--author",
                &author_arg,
                "--",
                &rel,
                "log.md",
            ],
        )
        .await?;
        Some(git(vault_root, &["rev-parse", "HEAD"]).await?)
    } else {
        None
    };
    drop(_guard);

    let mut sync = sync_file(pool, vault_root, &rel).await?;
    let log = sync_file(pool, vault_root, "log.md").await?;
    sync.added += log.added;
    sync.updated += log.updated;
    sync.chunks_new += log.chunks_new;
    sync.chunks_reused += log.chunks_reused;
    sync.parse_errors.extend(log.parse_errors);
    Ok(WriteOutcome { path: rel, created: existing.is_none(), commit, sync })
}

/// Append one line to `log.md` (e.g. `## [2026-11-08] lint | librarian run — …`), commit it under
/// the daemon's git identity and re-sync it. Same lock and commit path as [`write_doc`].
pub async fn append_log_entry(pool: &PgPool, vault_root: &Path, line: &str) -> Result<Option<String>> {
    let line = line.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        bail!("empty log line");
    }
    let _guard = WRITE_LOCK.lock().await;
    append_log(vault_root, line).await?;
    let commit = if vault_root.join(".git").exists() {
        git(vault_root, &["add", "--", "log.md"]).await?;
        let (bot_name, bot_email) = bot();
        let author_arg = format!("{bot_name} <{bot_email}>");
        let msg = super::truncate_chars(line.trim_start_matches('#').trim(), 200).to_string();
        git(
            vault_root,
            &[
                "-c",
                &format!("user.name={bot_name}"),
                "-c",
                &format!("user.email={bot_email}"),
                "commit",
                "-q",
                "--no-verify",
                "-m",
                &msg,
                "--author",
                &author_arg,
                "--",
                "log.md",
            ],
        )
        .await?;
        Some(git(vault_root, &["rev-parse", "HEAD"]).await?)
    } else {
        None
    };
    drop(_guard);
    sync_file(pool, vault_root, "log.md").await?;
    Ok(commit)
}

/// Save a web clipping to `Clippings/<title>.md` with `source`, `status: raw`, `created: today`.
/// A different clipping with the same title gets a numeric suffix.
pub async fn clip(pool: &PgPool, vault_root: &Path, c: Clip) -> Result<WriteOutcome> {
    if !c.url.contains("://") {
        bail!("clip needs an absolute url");
    }
    let base = safe_filename(&c.title);
    let mut path = format!("Clippings/{base}.md");
    for n in 2..100 {
        let Ok(text) = tokio::fs::read_to_string(vault_root.join(&path)).await else { break };
        let (fm, _, _) = parse_document(&text);
        if fm.source.as_deref() == Some(c.url.as_str()) && agent_owned(&text) {
            break; // our own clipping of the same source: update it in place
        }
        path = format!("Clippings/{base} {n}.md");
    }
    let mut fm = Map::new();
    fm.insert("source".into(), Value::from(c.url.clone()));
    fm.insert("status".into(), Value::from("raw"));
    fm.insert("created".into(), Value::from(today().to_string()));
    if !c.categories.is_empty() {
        fm.insert("categories".into(), Value::from(c.categories.iter().map(|x| format!("[[{}]]", super::frontmatter::unlink(x))).collect::<Vec<_>>()));
    }
    if !c.tags.is_empty() {
        fm.insert("tags".into(), Value::from(c.tags.clone()));
    }
    let message = c.message.clone().unwrap_or_else(|| format!("clip {}", c.url));
    write_doc(
        pool,
        vault_root,
        WriteDoc { path, title: Some(c.title), author: c.author, frontmatter: Some(fm), body: c.body, message: Some(message) },
    )
    .await
}

/// `git push` the vault. May fail offline; the error is returned (commits stay local).
pub async fn push_vault(vault_root: &Path) -> Result<String> {
    let _guard = WRITE_LOCK.lock().await;
    git(vault_root, &["push", "--quiet"]).await
}

/// `git pull --rebase --autostash`. Run [`super::sync_vault`] afterwards to index what arrived.
/// On a conflict the rebase is aborted (local commits stay, unpushed) and the error returned, so the
/// checkout never wedges mid-rebase; callers log it and it shows in `lib.ops_log`.
pub async fn pull_vault(vault_root: &Path) -> Result<String> {
    let _guard = WRITE_LOCK.lock().await;
    let (bot_name, bot_email) = bot();
    let r = git(
        vault_root,
        &["-c", &format!("user.name={bot_name}"), "-c", &format!("user.email={bot_email}"), "pull", "--rebase", "--autostash", "--quiet"],
    )
    .await;
    if r.is_err() && (vault_root.join(".git/rebase-merge").exists() || vault_root.join(".git/rebase-apply").exists()) {
        let _ = git(vault_root, &["rebase", "--abort"]).await;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_paths() {
        assert_eq!(sanitize_write_path("Wiki/GB10 memory").unwrap(), "Wiki/GB10 memory.md");
        assert_eq!(sanitize_write_path("/docs/References/a.md").unwrap(), "References/a.md");
        assert!(sanitize_write_path("Notes/a.md").is_err());
        assert!(sanitize_write_path("Wiki/../Notes/a.md").is_err());
        assert!(sanitize_write_path("/etc/passwd").is_err());
        assert!(sanitize_write_path("Wiki/a.txt").is_err());
        assert!(sanitize_write_path("Wiki/a|b.md").is_err());
        assert!(sanitize_write_path("Wiki/.hidden.md").is_err());
        assert!(sanitize_write_path("Wiki").is_err());
        assert!(sanitize_write_path("log.md").is_err());
    }

    #[test]
    fn owner_ownership() {
        // Default identity: owner `owner`, agents `agent` and `claude`.
        assert!(!agent_owned("---\nauthor: owner\n---\nx"));
        assert!(!agent_owned("---\nauthor: \"Owner\"\ntitle: [bad\n---\nx"));
        assert!(!agent_owned("---\nauthor: \"[[Owner]]\"\n---\nx"));
        assert!(!agent_owned("---\nauthor: [owner, agent]\n---\nx"));
        assert!(!agent_owned("---\nauthor: Sam Example\n---\nx"));
        assert!(!agent_owned("no frontmatter"));
        assert!(!agent_owned("---\ntitle: x\n---\nbody"));
        assert!(agent_owned("---\nauthor: agent\n---\nx"));
        assert!(agent_owned("---\nauthor: \"[[Claude]]\"\n---\nx"));
        assert!(agent_owned("---\nauthor: agent\ntitle: [bad\n---\nx"));
    }

    #[test]
    fn filenames() {
        assert_eq!(safe_filename("What is [[GB10]]? A: test/run"), "What is --GB10--- A- test-run");
        assert_eq!(safe_filename("..."), "untitled");
    }
}
