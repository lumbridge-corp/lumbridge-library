use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// lumbridge-libraryd configuration: `~/.config/lumbridge-library/lumbridge-libraryd.toml` unless
/// `--config` or `LUMBRIDGE_LIBRARYD_CONFIG` names another file (see [`default_config_path`] for the
/// folders earlier releases used, which are still read).
///
/// Secrets are not in this file: `DATABASE_URL`, `READER_DATABASE_URL` and the API token come
/// from the environment (systemd `EnvironmentFile=`) or from `api_token_file`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Address to bind, e.g. 127.0.0.1:8830. Bind a private-network address only behind a firewall
    /// or VPN you trust: the bearer token is the only lock, and the HTTP is plain.
    pub listen: SocketAddr,
    /// Hostnames MCP clients may use in the Host header (DNS-rebinding guard). Unset: localhost,
    /// 127.0.0.1, ::1 and the IP address in `listen`.
    #[serde(default)]
    pub allowed_hosts: Option<Vec<String>>,
    /// File holding the bearer token clients must send (0600).
    pub api_token_file: PathBuf,
    /// Second token for owner-only REST actions: approve/retire/remove, server-side URL fetch for
    /// clips, verdicts from the CLI, exports. MCP never accepts it. Unset = those routes are off.
    #[serde(default)]
    pub admin_token_file: Option<PathBuf>,
    /// Root for vault checkout, warehouse mirrors, attachments, datasets.
    #[serde(default = "default_library_root")]
    pub library_root: PathBuf,
    pub embed: EmbedConfig,
    pub rerank: RerankConfig,
    #[serde(default)]
    pub vault: VaultConfig,
    #[serde(default)]
    pub gitea: Option<GiteaConfig>,
    #[serde(default)]
    pub jobs: JobsConfig,
    /// Python with trafilatura + pypdf, and the extractor script (tools/extract.py) for docs_clip.
    #[serde(default)]
    pub clipper: Option<ClipperConfig>,
    /// The librarian: nightly upkeep agent (facts, orphan links, duplicates, staleness, evals).
    #[serde(default)]
    pub librarian: LibrarianConfig,
    /// The design shelf: screenshot captions by a vision model.
    #[serde(default)]
    pub designs: DesignsConfig,
    /// The owner, the agent authors and the daemon's git identity ([`crate::identity`]).
    #[serde(default)]
    pub identity: crate::identity::Identity,
    #[serde(skip)]
    pub database_url: String,
    #[serde(skip)]
    pub reader_database_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbedConfig {
    /// OpenAI-compatible base, e.g. http://127.0.0.1:8012/v1
    pub base_url: String,
    #[serde(default = "default_embed_model")]
    pub model: String,
    #[serde(default = "default_dims")]
    pub dims: usize,
    /// Instruction prefixes for embedders trained with them (the defaults suit E5-style models);
    /// set both to "" for a model that takes plain text.
    #[serde(default = "default_query_prefix")]
    pub query_prefix: String,
    #[serde(default = "default_passage_prefix")]
    pub passage_prefix: String,
    #[serde(default = "default_batch")]
    pub batch: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RerankConfig {
    /// e.g. http://127.0.0.1:8015/v1
    pub base_url: String,
    #[serde(default = "default_rerank_model")]
    pub model: String,
}

/// The vault checkout at `{library_root}/vault`, kept in sync with a git remote.
#[derive(Debug, Clone, Deserialize)]
pub struct VaultConfig {
    /// git remote to clone from / pull from / push to, e.g. `git@git.example.com:me/vault.git`.
    pub remote: Option<String>,
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Push agent writes to the remote right after committing.
    #[serde(default = "yes")]
    pub push: bool,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self { remote: None, branch: default_branch(), push: true }
    }
}

/// Gitea API for publishing components and designs as repos in one organisation.
#[derive(Debug, Clone, Deserialize)]
pub struct GiteaConfig {
    /// e.g. http://127.0.0.1:3000
    pub api_base: String,
    /// 0600 file with a library-bot token (write:repository, write:organization).
    pub token_file: PathBuf,
    /// ssh host (or ~/.ssh/config alias) used for pushes, e.g. `git.example.com`.
    pub ssh_host: String,
    #[serde(default = "default_component_org")]
    pub component_org: String,
}

/// Background loop periods in seconds (0 disables a loop).
#[derive(Debug, Clone, Deserialize)]
pub struct JobsConfig {
    #[serde(default = "d60")]
    pub vault_sync_secs: u64,
    #[serde(default = "d20")]
    pub embed_secs: u64,
    #[serde(default = "d86400")]
    pub mirror_sync_secs: u64,
    #[serde(default = "d86400")]
    pub lint_secs: u64,
    /// Chunks embedded per pass per shelf.
    #[serde(default = "d256")]
    pub embed_batch: u64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self { vault_sync_secs: 60, embed_secs: 20, mirror_sync_secs: 86400, lint_secs: 86400, embed_batch: 256 }
    }
}

/// `[librarian]`: the upkeep agent. Off by default; when on, it starts in dry-run (reports only).
#[derive(Debug, Clone, Deserialize)]
pub struct LibrarianConfig {
    #[serde(default)]
    pub enabled: bool,
    /// OpenAI-compatible base of a local chat model. Keep it local if your clippings may not leave
    /// the machine.
    #[serde(default = "default_llm_base")]
    pub base_url: String,
    #[serde(default = "default_llm_model")]
    pub model: String,
    /// Local hour (0-23) of the nightly run.
    #[serde(default = "d3")]
    pub nightly_hour: u32,
    /// Vault page edits (orphan links) per run.
    #[serde(default = "d20")]
    pub max_edits_per_run: usize,
    /// Facts written per run (NOOPs don't count).
    #[serde(default = "d500")]
    pub max_facts_per_run: usize,
    /// LLM calls per run (all tasks together).
    #[serde(default = "d60")]
    pub max_llm_calls: usize,
    /// Report what would happen; write nothing to the vault or kb.facts.
    #[serde(default = "yes")]
    pub dry_run: bool,
    /// Prices / "as of" dates older than this many days make a page stale.
    #[serde(default = "d45")]
    pub stale_days: i64,
    /// Seconds per LLM request.
    #[serde(default = "d120")]
    pub timeout_secs: u64,
}

impl Default for LibrarianConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_llm_base(),
            model: default_llm_model(),
            nightly_hour: 3,
            max_edits_per_run: 20,
            max_facts_per_run: 500,
            max_llm_calls: 60,
            dry_run: true,
            stale_days: 45,
            timeout_secs: 120,
        }
    }
}

/// `[designs]`: captions for design screenshots (the embedder reads text only).
#[derive(Debug, Clone, Deserialize)]
pub struct DesignsConfig {
    /// OpenAI-compatible base of a vision chat model. Unset: no captions (design search then
    /// reads only the write-up and tokens).
    #[serde(default)]
    pub vision_base_url: Option<String>,
    #[serde(default = "default_vision_model")]
    pub vision_model: String,
    /// Seconds between caption rounds (0 = off; `POST /v1/designs/caption` still works).
    #[serde(default = "d300")]
    pub caption_secs: u64,
    /// Screenshots captioned per round.
    #[serde(default = "d60")]
    pub caption_batch: usize,
}

impl Default for DesignsConfig {
    fn default() -> Self {
        Self { vision_base_url: None, vision_model: default_vision_model(), caption_secs: 300, caption_batch: 60 }
    }
}

fn default_vision_model() -> String {
    "vision".into()
}
fn d300() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClipperConfig {
    pub python: PathBuf,
    pub script: PathBuf,
}

fn default_branch() -> String {
    "main".into()
}
fn yes() -> bool {
    true
}
fn default_component_org() -> String {
    "warehouse".into()
}
fn d20<T: From<u8>>() -> T {
    T::from(20)
}
fn d3() -> u32 {
    3
}
fn d45() -> i64 {
    45
}
fn d120() -> u64 {
    120
}
fn d500() -> usize {
    500
}
fn default_llm_base() -> String {
    "http://127.0.0.1:8000/v1".into()
}
fn default_llm_model() -> String {
    "local".into()
}
fn d60<T: From<u8>>() -> T {
    T::from(60)
}
fn d256() -> u64 {
    256
}
fn d86400() -> u64 {
    86400
}

fn default_library_root() -> PathBuf {
    crate::legacy::data_root(&home())
}
fn default_embed_model() -> String {
    "embed".into()
}
fn default_dims() -> usize {
    2048
}
fn default_query_prefix() -> String {
    "query: ".into()
}
fn default_passage_prefix() -> String {
    "passage: ".into()
}
fn default_batch() -> usize {
    32
}
fn default_rerank_model() -> String {
    "rerank".into()
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The current file name first, then the ones earlier releases used; nothing is moved
/// (`legacy::daemon_config`).
pub fn default_config_path() -> PathBuf {
    crate::legacy::daemon_config(&home())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.database_url = std::env::var("DATABASE_URL")
            .context("DATABASE_URL is not set (systemd EnvironmentFile=, e.g. ~/.config/lumbridge-library/db.env)")?;
        cfg.identity = cfg.identity.normalized();
        cfg.reader_database_url = std::env::var("READER_DATABASE_URL").ok();
        if cfg.embed.dims != 2048 {
            bail!("embed.dims = {} but the schema stores halfvec(2048); add a migration first", cfg.embed.dims);
        }
        Ok(cfg)
    }

    /// Hosts MCP clients may name in the Host header: `allowed_hosts`, else loopback plus the
    /// address in `listen`.
    pub fn allowed_hosts(&self) -> Vec<String> {
        if let Some(h) = &self.allowed_hosts {
            return h.clone();
        }
        let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"].into_iter().map(String::from).collect();
        let ip = self.listen.ip();
        if !ip.is_unspecified() && !hosts.contains(&ip.to_string()) {
            hosts.push(ip.to_string());
        }
        hosts
    }

    pub fn vault_root(&self) -> PathBuf {
        self.library_root.join("vault")
    }

    pub fn admin_token(&self) -> Result<Option<String>> {
        let Some(p) = &self.admin_token_file else { return Ok(None) };
        let t = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        let t = t.trim().to_string();
        if t.len() < 32 {
            bail!("admin token in {} is too short", p.display());
        }
        Ok(Some(t))
    }

    pub fn api_token(&self) -> Result<String> {
        let t = std::fs::read_to_string(&self.api_token_file)
            .with_context(|| format!("reading {}", self.api_token_file.display()))?;
        let t = t.trim().to_string();
        if t.len() < 32 {
            bail!("api token in {} is too short", self.api_token_file.display());
        }
        Ok(t)
    }
}
