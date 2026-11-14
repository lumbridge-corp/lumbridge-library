//! Thin async wrappers over the `git` CLI. Every call has a timeout and runs with prompts disabled,
//! so a dead remote or a credential prompt can never hang lumbridge-libraryd.

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub const SHORT: Duration = Duration::from_secs(30);
pub const LONG: Duration = Duration::from_secs(1800);

/// Hosts [`validate_remote_url`] accepts by default for agent-supplied mirror URLs.
pub const PUBLIC_GIT_HOSTS: &[&str] = &["github.com", "gitlab.com", "codeberg.org", "git.sr.ht", "huggingface.co", "bitbucket.org"];

/// Cap for README / manifest / license blobs read for cards and license detection.
pub const META_MAX_BYTES: u64 = 512 * 1024;

/// `GIT_SSH_COMMAND` with `BatchMode=yes`, keeping whatever ssh command/options the environment set.
fn ssh_command() -> String {
    match std::env::var("GIT_SSH_COMMAND") {
        Ok(c) if !c.trim().is_empty() => {
            if c.contains("BatchMode=yes") {
                c
            } else {
                format!("{} -o BatchMode=yes", c.trim())
            }
        }
        _ => "ssh -o BatchMode=yes".to_string(),
    }
}

pub fn cmd(dir: Option<&Path>) -> Command {
    let mut c = Command::new("git");
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    c.args([
        "-c",
        "core.quotepath=off",
        "-c",
        "color.ui=false",
        "-c",
        "protocol.ext.allow=never",
        "-c",
        "protocol.fd.allow=never",
    ])
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_PROTOCOL_FROM_USER", "0")
    .env("GIT_ASKPASS", "true")
    .env("SSH_ASKPASS", "true")
    .env("GIT_SSH_COMMAND", ssh_command())
    .env_remove("GIT_DIR")
    .env_remove("GIT_WORK_TREE")
    .stdin(Stdio::null())
    .kill_on_drop(true);
    c
}

fn valid_url_segment(s: &str) -> bool {
    let mut ch = s.chars();
    matches!(ch.next(), Some(c) if c.is_ascii_alphanumeric())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && !s.contains("..")
}

/// Accept only `https://<host>/<owner>/<name>[.git]` with `host` in `allowed_hosts` (case-insensitive):
/// no userinfo, port, query or fragment; owner and name match `[A-Za-z0-9][A-Za-z0-9._-]*` without `..`.
/// Use for URLs that come from agents (see [`PUBLIC_GIT_HOSTS`]).
pub fn validate_remote_url(url: &str, allowed_hosts: &[&str]) -> Result<()> {
    let bad = |why: &str| anyhow!("rejected remote URL {url:?}: {why}");
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad("contains whitespace or control characters"));
    }
    let rest = url
        .get(..8)
        .filter(|p| p.eq_ignore_ascii_case("https://"))
        .map(|_| &url[8..])
        .ok_or_else(|| bad("only https:// URLs are allowed"))?;
    if rest.contains('?') || rest.contains('#') {
        return Err(bad("query strings and fragments are not allowed"));
    }
    let (host, path) = rest.split_once('/').ok_or_else(|| bad("expected https://<host>/<owner>/<name>"))?;
    if host.contains('@') {
        return Err(bad("userinfo is not allowed"));
    }
    if host.contains(':') {
        return Err(bad("an explicit port is not allowed"));
    }
    if !allowed_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        return Err(bad(&format!("host {host:?} is not in the allowed list {allowed_hosts:?}")));
    }
    let segs: Vec<&str> = path.split('/').collect();
    let [owner, name] = segs.as_slice() else {
        return Err(bad("expected exactly /<owner>/<name>"));
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    if !valid_url_segment(owner) || !valid_url_segment(name) {
        return Err(bad("owner and name must match [A-Za-z0-9][A-Za-z0-9._-]* (no leading dot, no ..)"));
    }
    Ok(())
}

/// Run git and return stdout (bytes). Fails on non-zero exit with stderr in the error.
pub async fn run_bytes(dir: Option<&Path>, args: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    let mut c = cmd(dir);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().context("spawning git")?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("git {} timed out after {:?}", args.join(" "), timeout))??;
    if !out.status.success() {
        bail!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

pub async fn run(dir: Option<&Path>, args: &[&str], timeout: Duration) -> Result<String> {
    let b = run_bytes(dir, args, timeout).await?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// Which transports a network command may use. `GIT_PROTOCOL_FROM_USER=0` turns off the `file`
/// transport; only the owner's own repos (`add_own` with a local path) turn it back on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// https / ssh / git only.
    Remote,
    /// Also local paths and `file://`.
    AllowLocal,
}

/// `-c` flags for clone / fetch / ls-remote.
fn net_args(t: Transport) -> Vec<&'static str> {
    let mut v = vec!["-c", "http.followRedirects=false"];
    if t == Transport::AllowLocal {
        v.extend(["-c", "protocol.file.allow=always"]);
    }
    v
}

pub async fn clone_mirror(url: &str, dest: &Path, t: Transport) -> Result<()> {
    let d = dest.to_string_lossy();
    let mut args = net_args(t);
    args.extend(["clone", "--mirror", "--quiet", "--", url, &d]);
    run(None, &args, LONG).await?;
    Ok(())
}

pub async fn fetch(dir: &Path, t: Transport) -> Result<()> {
    let mut args = net_args(t);
    args.extend(["fetch", "--prune", "--quiet", "--force", "origin"]);
    run(Some(dir), &args, LONG).await?;
    Ok(())
}

/// Point the bare repo's HEAD at the remote's default branch, if the remote says.
pub async fn refresh_remote_head(dir: &Path, t: Transport) -> Result<()> {
    let mut args = net_args(t);
    args.extend(["ls-remote", "--symref", "origin", "HEAD"]);
    let out = run(Some(dir), &args, SHORT).await?;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("ref: ")
            && let Some((target, _)) = rest.split_once('\t')
            && target.starts_with("refs/heads/")
        {
            run(Some(dir), &["symbolic-ref", "HEAD", target], SHORT).await?;
        }
    }
    Ok(())
}

pub async fn default_branch(dir: &Path) -> Result<String> {
    let r = run(Some(dir), &["symbolic-ref", "--quiet", "HEAD"], SHORT).await?;
    Ok(r.trim().trim_start_matches("refs/heads/").to_string())
}

/// Commit id of HEAD, or None for an empty repo.
pub async fn head(dir: &Path) -> Result<Option<String>> {
    match run(Some(dir), &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], SHORT).await {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(_) => Ok(None),
    }
}

pub async fn commit_date(dir: &Path, commit: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let s = run(Some(dir), &["log", "-1", "--format=%cI", commit], SHORT).await?;
    Ok(chrono::DateTime::parse_from_rfc3339(s.trim()).ok().map(|d| d.with_timezone(&chrono::Utc)))
}

/// `remote.origin.url` of a repo, if set.
pub async fn origin_url(dir: &Path) -> Result<Option<String>> {
    match run(Some(dir), &["config", "--get", "remote.origin.url"], SHORT).await {
        Ok(s) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Err(_) => Ok(None),
    }
}

pub async fn is_git_dir(dir: &Path) -> bool {
    dir.exists() && run(Some(dir), &["rev-parse", "--git-dir"], SHORT).await.is_ok()
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: String,
    pub sha: String,
    pub size: u64,
}

/// Result of [`ls_tree`]: regular-file blobs plus what was dropped while listing.
#[derive(Debug, Default)]
pub struct LsTree {
    pub entries: Vec<TreeEntry>,
    /// Regular files whose path is not valid UTF-8 (skipped, never lossy-decoded).
    pub non_utf8_paths: usize,
}

/// All blobs at `commit` (regular files only: no submodules, no symlinks).
pub async fn ls_tree(dir: &Path, commit: &str) -> Result<LsTree> {
    let out = run_bytes(Some(dir), &["ls-tree", "-r", "-l", "-z", "--full-tree", commit], LONG).await?;
    Ok(parse_ls_tree(&out))
}

fn parse_ls_tree(out: &[u8]) -> LsTree {
    let mut t = LsTree::default();
    for rec in out.split(|b| *b == 0) {
        // "<mode> SP <type> SP <object> SP+ <size> TAB <path>"
        let Some(tab) = rec.iter().position(|b| *b == b'\t') else { continue };
        let Ok(meta) = std::str::from_utf8(&rec[..tab]) else { continue };
        let mut it = meta.split_whitespace();
        let (Some(mode), Some(kind), Some(sha), Some(size)) = (it.next(), it.next(), it.next(), it.next()) else {
            continue;
        };
        if kind != "blob" || !(mode == "100644" || mode == "100755") {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&rec[tab + 1..]) else {
            t.non_utf8_paths += 1;
            continue;
        };
        t.entries.push(TreeEntry { path: path.to_string(), sha: sha.to_string(), size: size.parse().unwrap_or(0) });
    }
    t
}

/// One object read through [`CatFile::blob`] / [`show`].
#[derive(Debug, PartialEq, Eq)]
pub enum Blob {
    /// Missing, ambiguous, or not a blob.
    Missing,
    /// Larger than the caller's cap; its bytes were drained, never buffered.
    TooLarge(u64),
    Data(Vec<u8>),
}

impl Blob {
    pub fn data(self) -> Option<Vec<u8>> {
        match self {
            Blob::Data(b) => Some(b),
            _ => None,
        }
    }
}

/// One long-lived `git cat-file --batch`: blobs are streamed one at a time, never a whole repo.
pub struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    poisoned: bool,
}

impl CatFile {
    pub fn spawn(dir: &Path) -> Result<Self> {
        let mut c = cmd(Some(dir));
        c.args(["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = c.spawn().context("spawning git cat-file")?;
        let stdin = child.stdin.take().context("cat-file stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("cat-file stdout")?);
        Ok(Self { child, stdin, stdout, poisoned: false })
    }

    /// Read an object by name (`<sha>` or `<commit>:<path>`). Objects over `max_bytes` (by the
    /// batch header) are drained without allocating and reported as [`Blob::TooLarge`].
    pub async fn blob(&mut self, name: &str, max_bytes: u64) -> Result<Blob> {
        if self.poisoned {
            bail!("cat-file stream is out of sync after an earlier error");
        }
        if name.contains('\n') {
            bail!("object name contains a newline");
        }
        self.poisoned = true; // cleared only once the whole response was consumed
        let (stdin, stdout) = (&mut self.stdin, &mut self.stdout);
        let fut = async {
            stdin.write_all(name.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
            let mut header = String::new();
            if (&mut *stdout).take(4096).read_line(&mut header).await? == 0 {
                bail!("cat-file closed");
            }
            if !header.ends_with('\n') {
                bail!("cat-file header too long");
            }
            let header = header.trim_end();
            if header.ends_with(" missing") || header.ends_with(" ambiguous") {
                return Ok(Blob::Missing);
            }
            let mut parts = header.split(' ');
            let _sha = parts.next();
            let kind = parts.next().unwrap_or("");
            let size: u64 = parts.next().and_then(|s| s.parse().ok()).context("cat-file header")?;
            if kind != "blob" || size > max_bytes {
                // Drain content + trailing LF without buffering it.
                let n = tokio::io::copy(&mut (&mut *stdout).take(size + 1), &mut tokio::io::sink()).await?;
                if n != size + 1 {
                    bail!("cat-file closed mid-object");
                }
                return Ok(if kind == "blob" { Blob::TooLarge(size) } else { Blob::Missing });
            }
            let len = usize::try_from(size).context("blob size")?;
            let mut buf = vec![0u8; len + 1];
            stdout.read_exact(&mut buf).await?;
            buf.pop(); // trailing LF
            Ok(Blob::Data(buf))
        };
        let r = tokio::time::timeout(Duration::from_secs(60), fut).await.map_err(|_| anyhow!("cat-file timed out"))??;
        self.poisoned = false;
        Ok(r)
    }

    pub async fn close(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        let _ = self.child.start_kill();
    }
}

/// Read one file at a commit, refusing (without buffering) anything over `max_bytes`.
pub async fn show(dir: &Path, commit: &str, path: &str, max_bytes: u64) -> Result<Blob> {
    let mut cat = CatFile::spawn(dir)?;
    let r = cat.blob(&format!("{commit}:{path}"), max_bytes).await;
    cat.close().await;
    r
}

/// Commit everything in a working tree (used for components).
pub async fn init_and_commit(dir: &Path, author: &str, message: &str) -> Result<String> {
    if !dir.join(".git").exists() {
        run(Some(dir), &["init", "--quiet", "--initial-branch=main"], SHORT).await?;
    }
    run(Some(dir), &["add", "--all"], SHORT).await?;
    let name = format!("user.name={author}");
    let email = format!("user.email={author}@warehouse.local");
    run(
        Some(dir),
        &["-c", &name, "-c", &email, "-c", "commit.gpgsign=false", "commit", "--quiet", "--allow-empty", "-m", message],
        SHORT,
    )
    .await?;
    Ok(head(dir).await?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_url_allow_list() {
        let ok = |u: &str| validate_remote_url(u, PUBLIC_GIT_HOSTS).is_ok();
        assert!(ok("https://github.com/tokio-rs/tokio"));
        assert!(ok("https://github.com/BurntSushi/memchr.git"));
        assert!(ok("https://GitHub.com/dtolnay/anyhow"));
        assert!(ok("https://huggingface.co/deepseek-ai/DeepSeek-V3"));
        for bad in [
            "file:///etc",
            "file://localhost/tmp/x/y",
            "/local/owner/name",
            "./owner/name",
            "ext::sh -c touch% /tmp/pwned",
            "git@evil:o/n.git",
            "git@github.com:o/n.git",
            "ssh://github.com/o/n",
            "http://github.com/o/n",
            "https://user@github.com/o/n",
            "https://user:pw@github.com/o/n",
            "https://github.com:22/x/y",
            "https://github.com:443/x/y",
            "https://github.com/x/y?ref=main",
            "https://github.com/x/y#frag",
            "https://github.com/.hidden/y",
            "https://github.com/x/.y",
            "https://github.com/x/a..b",
            "https://github.com/x/y/tree/main",
            "https://github.com/x",
            "https://github.com/x/y/",
            "https://evil.example/x/y",
            "https://github.com.evil.example/x/y",
            "https://github.com/x/%2e%2e",
            "https://github.com/x/y z",
            "https://github.com/-x/y",
        ] {
            assert!(!ok(bad), "should reject {bad:?}");
        }
        assert!(validate_remote_url("https://git.example/o/n", &["git.example"]).is_ok());
    }

    #[test]
    fn ls_tree_skips_non_utf8_paths() {
        let mut out = Vec::new();
        out.extend_from_slice(b"100644 blob aaaa       12\tsrc/ok.rs\0");
        out.extend_from_slice(b"100644 blob bbbb       12\tsrc/bad\xff.rs\0");
        out.extend_from_slice(b"120000 blob cccc       12\tlink\xff\0");
        out.extend_from_slice(b"160000 commit dddd       -\tsub\0");
        let t = parse_ls_tree(&out);
        assert_eq!(t.entries.len(), 1);
        assert_eq!(t.entries[0].path, "src/ok.rs");
        assert_eq!(t.entries[0].size, 12);
        assert_eq!(t.non_utf8_paths, 1, "only regular files are counted");
    }

    #[tokio::test]
    async fn blob_size_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("small.txt"), "hello\n").unwrap();
        std::fs::write(dir.join("big.txt"), "x".repeat(100_000)).unwrap();
        let head = init_and_commit(dir, "test", "fixture").await.unwrap();

        // show(): capped, missing, ok.
        assert_eq!(show(dir, &head, "small.txt", 1024).await.unwrap(), Blob::Data(b"hello\n".to_vec()));
        assert_eq!(show(dir, &head, "big.txt", 1024).await.unwrap(), Blob::TooLarge(100_000));
        assert_eq!(show(dir, &head, "nope.txt", 1024).await.unwrap(), Blob::Missing);
        assert_eq!(show(dir, &head, "big.txt", 100_000).await.unwrap().data().map(|b| b.len()), Some(100_000));

        // The batch stream stays in sync after draining an over-cap blob and a non-blob.
        let mut cat = CatFile::spawn(dir).unwrap();
        assert_eq!(cat.blob(&format!("{head}:big.txt"), 10).await.unwrap(), Blob::TooLarge(100_000));
        assert_eq!(cat.blob(&head, 1 << 20).await.unwrap(), Blob::Missing, "a commit is not a blob");
        assert_eq!(cat.blob(&format!("{head}:small.txt"), 10).await.unwrap(), Blob::Data(b"hello\n".to_vec()));
        assert_eq!(cat.blob(&format!("{head}:small.txt"), 5).await.unwrap(), Blob::TooLarge(6));
        assert!(cat.blob("a\nb", 10).await.is_err());
        assert_eq!(cat.blob(&format!("{head}:small.txt"), 10).await.unwrap(), Blob::Data(b"hello\n".to_vec()));
        cat.close().await;
    }

    #[test]
    fn ssh_command_keeps_batch_mode() {
        assert!(ssh_command().contains("BatchMode=yes"));
    }
}
