//! Thin clients for lumbridge-libraryd: `lumbridge-docs` ([`docs`]) and `lumbridge-warehouse`
//! ([`warehouse`]).
//!
//! Config: `LUMBRIDGE_LIBRARY_URL` (default http://127.0.0.1:8830) and the token in
//! `LUMBRIDGE_LIBRARY_TOKEN` or `~/.config/lumbridge-library/token` (0600). The names earlier
//! releases used (`LIBRARY_URL`, `LIBRARY_TOKEN`, `LIBRARY_ADMIN_TOKEN`, `~/.config/library/`) are
//! still read when the current ones are absent: see `lumbridge_library_legacy`.

pub mod docs;
pub mod warehouse;

use anyhow::{Context, Result, bail};
use lumbridge_library_legacy as legacy;
use serde_json::Value;
use std::path::PathBuf;

pub struct Client {
    base: String,
    token: String,
    /// The owner's admin token (`admin-token` in the config folder), sent as x-library-admin when present.
    admin: Option<String>,
    http: reqwest::Client,
}

impl Client {
    pub fn from_env() -> Result<Self> {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let base = legacy::env(legacy::ENV_URL).unwrap_or_else(|| legacy::DEFAULT_URL.into());
        let token = match legacy::env(legacy::ENV_TOKEN) {
            Some(t) => t,
            None => {
                let p = legacy::config_file(&home, "token");
                std::fs::read_to_string(&p).with_context(|| format!("no {} and can't read {}", legacy::ENV_TOKEN.new, p.display()))?
            }
        };
        let admin =
            legacy::env(legacy::ENV_ADMIN_TOKEN).or_else(|| std::fs::read_to_string(legacy::config_file(&home, "admin-token")).ok());
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            token: token.trim().into(),
            admin: admin.map(|a| a.trim().to_string()).filter(|a| !a.is_empty()),
            http: reqwest::Client::new(),
        })
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let r = self.http.get(&url).bearer_auth(&self.token).headers(self.headers()).send().await.with_context(|| format!("GET {url}"))?;
        Self::finish(&url, r).await
    }

    /// GET with query parameters (None values dropped).
    pub async fn get_q(&self, path: &str, q: &[(&str, Option<String>)]) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let q: Vec<(&str, String)> = q.iter().filter_map(|(k, v)| v.clone().map(|v| (*k, v))).collect();
        let r = self.http.get(&url).query(&q).bearer_auth(&self.token).headers(self.headers()).send().await.with_context(|| format!("GET {url}"))?;
        Self::finish(&url, r).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let r = self.http.post(&url).json(body).bearer_auth(&self.token).headers(self.headers()).send().await.with_context(|| format!("POST {url}"))?;
        Self::finish(&url, r).await
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(legacy::wire::HEADER_CLIENT, reqwest::header::HeaderValue::from_static("cli"));
        if let Some(a) = &self.admin
            && let Ok(v) = reqwest::header::HeaderValue::from_str(a)
        {
            h.insert(legacy::wire::HEADER_ADMIN, v);
        }
        h
    }

    async fn finish(url: &str, r: reqwest::Response) -> Result<Value> {
        let code = r.status();
        let body = r.text().await?;
        if !code.is_success() {
            bail!("{url}: {code}: {}", body.trim());
        }
        Ok(serde_json::from_str(&body)?)
    }
}

/// Write text to stdout; a closed pipe ends quietly.
pub fn out(s: &str) {
    let _ = std::io::Write::write_all(&mut std::io::stdout().lock(), s.as_bytes());
}

/// Ranked hits as a compact list: `score  path — heading` then a one-line excerpt.
pub fn hits(v: &Value, loc: impl Fn(&Value) -> String) {
    let mut s = String::new();
    if let Some(d) = v.get("degraded").and_then(|d| d.as_array()).filter(|d| !d.is_empty()) {
        s.push_str(&format!("(degraded: {})\n", d.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")));
    }
    for h in v.get("hits").and_then(|h| h.as_array()).into_iter().flatten() {
        let score = h.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let text: String = h.get("content").and_then(|c| c.as_str()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
        let text: String = text.chars().take(160).collect();
        s.push_str(&format!("{score:.3}  {}\n       {text}\n", loc(h)));
    }
    out(&s);
}

/// Prints to stdout; a closed pipe (`| head`) ends quietly instead of panicking.
pub fn print(v: &Value, json: bool) {
    let mut out = String::new();
    if json {
        out = serde_json::to_string_pretty(v).unwrap_or_default();
        out.push('\n');
    } else {
        tree(v, 0, &mut out);
    }
    let _ = std::io::Write::write_all(&mut std::io::stdout().lock(), out.as_bytes());
}

fn tree(v: &Value, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if x.is_object() {
                    out.push_str(&format!("{pad}{k}:\n"));
                    tree(x, depth + 1, out);
                } else {
                    out.push_str(&format!("{pad}{k}: {}\n", scalar(x)));
                }
            }
        }
        other => out.push_str(&format!("{pad}{}\n", scalar(other))),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}
