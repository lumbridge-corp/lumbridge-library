//! Clients for the retrieval models (embed + rerank; vLLM or any OpenAI-compatible server).

use crate::config::{EmbedConfig, RerankConfig};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Query,
    Passage,
}

#[derive(Clone)]
pub struct Embedder {
    cfg: EmbedConfig,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelIdentity {
    pub name: String,
    pub root: String,
    pub dims: usize,
    pub fingerprint: String,
}

#[derive(Deserialize)]
struct EmbedResp {
    data: Vec<EmbedRow>,
}
#[derive(Deserialize)]
struct EmbedRow {
    index: usize,
    embedding: Vec<f32>,
}
#[derive(Deserialize)]
struct ModelsResp {
    data: Vec<ModelRow>,
}
#[derive(Deserialize)]
struct ModelRow {
    id: String,
    #[serde(default)]
    root: Option<String>,
}

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client")
}

impl Embedder {
    pub fn new(cfg: EmbedConfig) -> Self {
        Self { cfg, http: client(Duration::from_secs(120)) }
    }

    pub fn dims(&self) -> usize {
        self.cfg.dims
    }

    /// Who is actually serving `model` right now. The fingerprint changes when the weights path,
    /// dimension or prefix scheme changes, which is exactly when old vectors stop being comparable.
    pub async fn identity(&self) -> Result<ModelIdentity> {
        let url = format!("{}/models", self.cfg.base_url.trim_end_matches('/'));
        let resp: ModelsResp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?
            .json()
            .await?;
        let row = resp
            .data
            .into_iter()
            .find(|m| m.id == self.cfg.model)
            .with_context(|| format!("model {:?} not served at {url}", self.cfg.model))?;
        let root = row.root.unwrap_or_else(|| row.id.clone());
        let fp = blake3::hash(
            format!(
                "{}|{}|{}|{}|{}",
                self.cfg.model, root, self.cfg.dims, self.cfg.query_prefix, self.cfg.passage_prefix
            )
            .as_bytes(),
        )
        .to_hex()[..16]
            .to_string();
        Ok(ModelIdentity { name: self.cfg.model.clone(), root, dims: self.cfg.dims, fingerprint: fp })
    }

    pub async fn embed(&self, texts: &[String], kind: Kind) -> Result<Vec<Vec<f32>>> {
        let prefix = match kind {
            Kind::Query => &self.cfg.query_prefix,
            Kind::Passage => &self.cfg.passage_prefix,
        };
        let url = format!("{}/embeddings", self.cfg.base_url.trim_end_matches('/'));
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.cfg.batch.max(1)) {
            let input: Vec<String> = batch.iter().map(|t| format!("{prefix}{t}")).collect();
            let resp: EmbedResp = self
                .http
                .post(&url)
                .json(&serde_json::json!({ "model": self.cfg.model, "input": input }))
                .send()
                .await
                .with_context(|| format!("POST {url}"))?
                .error_for_status()?
                .json()
                .await?;
            let mut rows = resp.data;
            rows.sort_by_key(|r| r.index);
            if rows.len() != batch.len() {
                bail!("embed returned {} vectors for {} inputs", rows.len(), batch.len());
            }
            for r in rows {
                if r.embedding.len() != self.cfg.dims {
                    bail!("embed returned {}-d, expected {}", r.embedding.len(), self.cfg.dims);
                }
                out.push(r.embedding);
            }
        }
        Ok(out)
    }
}

#[derive(Clone)]
pub struct Reranker {
    cfg: RerankConfig,
    http: reqwest::Client,
    /// Whether the server honours `use_activation: false` (raw logits): probed once on first
    /// use, shared by clones.
    raw: std::sync::Arc<tokio::sync::OnceCell<bool>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankHit {
    pub index: usize,
    /// Probability that the document answers the query: `sigmoid(logit)` (absolute, 0..1).
    pub relevance_score: f32,
    /// The model's raw score. The server's f32 sigmoid saturates: on code questions most of the
    /// top 30 come back as 0.99999.. or exactly 1.0 while their logits still span 10+ units.
    #[serde(default)]
    pub logit: f64,
}

#[derive(Deserialize)]
struct RerankResp {
    results: Vec<RawHit>,
}
#[derive(Deserialize)]
struct RawHit {
    index: usize,
    relevance_score: f64,
}

pub fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Inverse sigmoid in f64; a probability of exactly 0 or 1 maps to ±27.6 (1e-12 clamp).
pub fn logit(p: f64) -> f64 {
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    (p / (1.0 - p)).ln()
}

impl Reranker {
    pub fn new(cfg: RerankConfig) -> Self {
        Self { cfg, http: client(Duration::from_secs(60)), raw: Default::default() }
    }

    async fn call(&self, query: &str, documents: &[String], raw: bool) -> Result<Vec<RawHit>> {
        let url = format!("{}/rerank", self.cfg.base_url.trim_end_matches('/'));
        let mut body = serde_json::json!({ "model": self.cfg.model, "query": query, "documents": documents });
        if raw {
            // vLLM pooling param: skip the sigmoid and return the classifier logit.
            body["use_activation"] = serde_json::Value::Bool(false);
        }
        let resp: RerankResp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.results)
    }

    /// True when the server returns logits for `use_activation: false`. Probed with a pair the
    /// model scores as hopeless (logit ≈ -15): a logit is then negative, a probability never is.
    /// A server that rejects or ignores the parameter is used in probability mode. A failed probe
    /// (server down) is not cached.
    async fn raw_logits(&self) -> Result<bool> {
        if let Some(v) = self.raw.get() {
            return Ok(*v);
        }
        let probe = ["The weather in Paris is mild in spring.".to_string()];
        let raw = match self.call("tokio work stealing scheduler", &probe, true).await {
            Ok(h) => h.first().is_some_and(|h| h.relevance_score < 0.0),
            Err(e) => {
                // Distinguish "server down" (propagate, retry next time) from "param rejected".
                self.call("ping", &["pong".to_string()], false).await?;
                tracing::info!("reranker: no raw logits ({e:#}); inverting probabilities");
                false
            }
        };
        let _ = self.raw.set(raw);
        Ok(raw)
    }

    /// Scores every document against the query; best first.
    pub async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<RerankHit>> {
        if documents.is_empty() {
            return Ok(vec![]);
        }
        let raw = self.raw_logits().await?;
        let mut hits: Vec<RerankHit> = self
            .call(query, documents, raw)
            .await?
            .into_iter()
            .map(|h| {
                let (p, l) = if raw {
                    (sigmoid(h.relevance_score), h.relevance_score)
                } else {
                    (h.relevance_score, logit(h.relevance_score))
                };
                RerankHit { index: h.index, relevance_score: p as f32, logit: l }
            })
            .collect();
        hits.sort_by(|a, b| b.logit.total_cmp(&a.logit));
        Ok(hits)
    }

    pub async fn healthy(&self) -> bool {
        self.rerank("ping", &["pong".to_string()]).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logit_inverts_sigmoid() {
        for x in [-20.0, -3.5, 0.0, 2.0, 15.0] {
            assert!((logit(sigmoid(x)) - x).abs() < 1e-4, "{x}");
        }
        assert!(logit(1.0).is_finite() && logit(0.0).is_finite());
    }
}
