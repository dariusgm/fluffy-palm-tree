use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::config::LlmConfig;

/// Minimal client for llama.cpp's OpenAI-compatible API.
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
}

pub enum Part {
    Text(String),
    Jpeg(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct Completion {
    /// Assistant message with any `<think>` block removed.
    pub text: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub latency_ms: i64,
    /// True if generation stopped at `max_tokens` (`finish_reason == "length"`).
    pub truncated: bool,
}

impl LlmClient {
    pub fn new(cfg: &LlmConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone().filter(|k| !k.is_empty()),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self
            .http
            .request(method, format!("{}{path}", self.base_url));
        match &self.api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }

    /// Whether llama.cpp accepts image input (`/props` → `modalities.vision`).
    /// `None` if the server does not expose it (e.g. not llama.cpp).
    pub async fn vision_supported(&self) -> Option<bool> {
        let resp = self
            .request(reqwest::Method::GET, "/props")
            .send()
            .await
            .ok()?;
        let body: Value = resp.json().await.ok()?;
        body["modalities"]["vision"].as_bool()
    }

    pub async fn models(&self) -> anyhow::Result<Value> {
        let resp = self
            .request(reqwest::Method::GET, "/v1/models")
            .send()
            .await
            .context("LLM server not reachable")?;
        let status = resp.status();
        let body: Value = resp.json().await.context("invalid /v1/models response")?;
        if !status.is_success() {
            bail!("LLM server returned {status}: {body}");
        }
        Ok(body)
    }

    pub async fn chat(
        &self,
        system: &str,
        parts: Vec<Part>,
        max_tokens: u32,
    ) -> anyhow::Result<Completion> {
        let content: Vec<Value> = parts
            .into_iter()
            .map(|p| match p {
                Part::Text(t) => json!({ "type": "text", "text": t }),
                Part::Jpeg(bytes) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                    json!({ "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{b64}") } })
                }
            })
            .collect();
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": content },
            ],
            "temperature": 0.2,
            "max_tokens": max_tokens,
            "response_format": { "type": "json_object" },
            // Qwen3-style models otherwise spend most of the budget on reasoning.
            "chat_template_kwargs": { "enable_thinking": false },
        });

        let started = Instant::now();
        let resp = self
            .request(reqwest::Method::POST, "/v1/chat/completions")
            .json(&body)
            .send()
            .await
            .context("LLM request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("reading LLM response")?;
        let latency_ms = started.elapsed().as_millis() as i64;
        if !status.is_success() {
            bail!(
                "LLM server returned {status}: {}",
                text.chars().take(500).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&text).context("LLM response is not JSON")?;
        let message = v["choices"][0]["message"]["content"]
            .as_str()
            .context("LLM response has no message content")?;
        Ok(Completion {
            text: strip_think(message),
            prompt_tokens: v["usage"]["prompt_tokens"].as_i64(),
            completion_tokens: v["usage"]["completion_tokens"].as_i64(),
            latency_ms,
            truncated: v["choices"][0]["finish_reason"] == "length",
        })
    }
}

/// Removes `<think>…</think>` reasoning blocks, including an unterminated leading one.
pub fn strip_think(s: &str) -> String {
    let mut out = s;
    if let Some(end) = out.rfind("</think>") {
        out = &out[end + "</think>".len()..];
    } else if let Some(start) = out.find("<think>") {
        out = &out[..start];
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_think_blocks() {
        assert_eq!(strip_think("<think>hmm</think>\n{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_think("reasoning</think>{}"), "{}");
        assert_eq!(strip_think("{} <think>unfinished"), "{}");
        assert_eq!(strip_think("  plain "), "plain");
    }
}
