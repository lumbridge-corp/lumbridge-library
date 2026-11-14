//! Tiny OpenAI-compatible chat client for the librarian's local model (`[librarian] base_url`; put a
//! tracing proxy in front of it if you want every call traced). Structured output via vLLM's
//! `response_format: json_schema`; low temperature, timeouts and retries. Output is untrusted:
//! callers validate everything against the database before acting on it.

use crate::config::LibrarianConfig;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Sent as `x-tracekit-client`, so a tracing proxy can tell callers apart.
pub const CLIENT_TAG: &str = crate::legacy::wire::TRACE_LIBRARIAN;
const RETRIES: usize = 3;

#[derive(Clone)]
pub struct Llm {
    base_url: String,
    model: String,
    http: reqwest::Client,
    /// Calls left in this run (shared by clones).
    budget: Arc<AtomicUsize>,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: Msg,
}
#[derive(Deserialize)]
struct Msg {
    #[serde(default)]
    content: Option<String>,
}

impl Llm {
    pub fn new(base_url: &str, model: &str, timeout: Duration, max_calls: usize) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            http: reqwest::Client::builder().timeout(timeout).build().expect("reqwest client"),
            budget: Arc::new(AtomicUsize::new(max_calls)),
        }
    }

    pub fn from_config(c: &LibrarianConfig) -> Self {
        Self::new(&c.base_url, &c.model, Duration::from_secs(c.timeout_secs.max(5)), c.max_llm_calls)
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Calls left in the budget.
    pub fn remaining(&self) -> usize {
        self.budget.load(Ordering::Relaxed)
    }

    fn take_call(&self) -> bool {
        self.budget.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1)).is_ok()
    }

    /// Is the model being served? (`GET /models` lists it.)
    pub async fn healthy(&self) -> bool {
        let Ok(r) = self.http.get(format!("{}/models", self.base_url)).header("x-tracekit-client", CLIENT_TAG).send().await else {
            return false;
        };
        let Ok(v) = r.json::<Value>().await else { return false };
        v["data"].as_array().is_some_and(|a| a.iter().any(|m| m["id"].as_str() == Some(self.model.as_str())))
    }

    /// One chat turn constrained to `schema` (a JSON Schema object), parsed into `T`.
    pub async fn json<T: DeserializeOwned>(&self, system: &str, user: &str, schema_name: &str, schema: Value) -> Result<T> {
        if !self.take_call() {
            bail!("LLM call budget for this run is used up");
        }
        let body = json!({
            "model": self.model,
            "temperature": 0.1,
            "max_tokens": 1200,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "response_format": {"type": "json_schema", "json_schema": {"name": schema_name, "strict": true, "schema": schema}},
            // Hybrid reasoning models: ask for the answer only (ignored by servers without it).
            "chat_template_kwargs": {"enable_thinking": false},
        });
        let mut last = None;
        for attempt in 0..RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
            }
            match self.once(&body).await {
                Ok(text) => match parse_json::<T>(&text) {
                    Ok(v) => return Ok(v),
                    Err(e) => last = Some(e),
                },
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("LLM call failed")))
    }

    async fn once(&self, body: &Value) -> Result<String> {
        let r = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .header("x-tracekit-client", CLIENT_TAG)
            .json(body)
            .send()
            .await
            .context("LLM request")?;
        let status = r.status();
        if !status.is_success() {
            let t = r.text().await.unwrap_or_default();
            bail!("LLM HTTP {status}: {}", crate::docs::truncate_chars(&t, 300));
        }
        let resp: ChatResp = r.json().await.context("LLM response")?;
        resp.choices.into_iter().next().and_then(|c| c.message.content).context("LLM returned no content")
    }
}

/// Parse model output as JSON: strips a `<think>…</think>` preamble and ``` fences if present.
pub fn parse_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    let mut t = text.trim();
    if let Some(i) = t.rfind("</think>") {
        t = t[i + "</think>".len()..].trim();
    }
    if let Some(r) = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")) {
        t = r.trim().trim_end_matches("```").trim();
    }
    serde_json::from_str(t).with_context(|| format!("LLM output is not the expected JSON: {}", crate::docs::truncate_chars(t, 200)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Debug)]
    struct V {
        yes: bool,
    }

    #[test]
    fn parses_wrapped_json() {
        assert!(parse_json::<V>("{\"yes\": true}").unwrap().yes);
        assert!(!parse_json::<V>("<think>hmm</think>\n```json\n{\"yes\": false}\n```").unwrap().yes);
        assert!(parse_json::<V>("nope").is_err());
    }

    #[test]
    fn budget() {
        let l = Llm::new("http://127.0.0.1:1/v1", "brain", Duration::from_secs(1), 2);
        assert!(l.take_call());
        assert!(l.take_call());
        assert!(!l.take_call());
        assert_eq!(l.remaining(), 0);
    }
}
