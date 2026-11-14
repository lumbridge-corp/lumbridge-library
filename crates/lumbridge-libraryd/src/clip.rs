//! URL / PDF → markdown via the Python extractor (tools/extract.py in a venv, `[clipper]`:
//! trafilatura for HTML, pdftotext/pypdf for PDFs). Used by `docs_clip` when the caller sends a URL
//! without text.

use anyhow::{Context, Result, bail};
use lumbridge_library_core::config::ClipperConfig;
use serde::Deserialize;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // url: echoed by the extractor, kept for debugging
pub struct Extracted {
    pub url: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub date: Option<String>,
    pub sitename: Option<String>,
    pub kind: String,
    pub markdown: String,
}

pub async fn extract(cfg: &ClipperConfig, url: &str) -> Result<Extracted> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("only http(s) URLs can be clipped");
    }
    let out = tokio::time::timeout(
        Duration::from_secs(180),
        Command::new(&cfg.python).arg("-I").arg(&cfg.script).arg(url).kill_on_drop(true).output(),
    )
    .await
    .context("extractor timed out")??;
    if !out.status.success() {
        bail!("extractor failed: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("").trim());
    }
    serde_json::from_slice(&out.stdout).context("extractor returned bad JSON")
}
