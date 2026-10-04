use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::LlmConfig;

/// What the LLM server reports about the model it serves (llama.cpp `/v1/models` and
/// `/props`). Everything except the name is missing on servers that do not expose it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    pub name: String,
    /// Context window the server was started with (`-c`).
    pub n_ctx: Option<i64>,
    /// Context length the model was trained for.
    pub n_ctx_train: Option<i64>,
    pub n_params: Option<i64>,
    pub size_bytes: Option<i64>,
    /// e.g. "IQ4_XS - 4.25 bpw".
    pub quantization: Option<String>,
    /// llama.cpp build, e.g. "b11269-cee37ffea".
    pub build_info: Option<String>,
}

struct Current {
    info: ModelInfo,
    /// Not yet stored in the database (see `take_unrecorded`).
    unrecorded: bool,
}

/// Minimal client for llama.cpp's OpenAI-compatible API. Without a configured model name
/// it uses the one the server reports, and follows the server when it is switched.
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    /// Empty = take whatever the server serves.
    configured_model: String,
    current: Arc<Mutex<Current>>,
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
    /// Model name the server answered with.
    pub model: Option<String>,
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
            configured_model: cfg.model.trim().to_string(),
            current: Arc::new(Mutex::new(Current {
                info: ModelInfo {
                    name: cfg.model.trim().to_string(),
                    ..Default::default()
                },
                unrecorded: false,
            })),
            api_key: cfg.api_key.clone().filter(|k| !k.is_empty()),
        })
    }

    /// Name of the model in use; empty until the server was asked and no name is configured.
    pub fn model(&self) -> String {
        self.info().name
    }

    pub fn info(&self) -> ModelInfo {
        self.current.lock().expect("model lock").info.clone()
    }

    /// The model details if they changed since the last call, so they are stored once.
    pub fn take_unrecorded(&self) -> Option<ModelInfo> {
        let mut cur = self.current.lock().expect("model lock");
        let pending = cur.unrecorded && !cur.info.name.is_empty();
        cur.unrecorded = false;
        pending.then(|| cur.info.clone())
    }

    /// Asks the server which model it serves and with which settings. A configured name
    /// selects that model from the list; without one the server must serve exactly one.
    pub async fn refresh(&self) -> anyhow::Result<ModelInfo> {
        let list = self.models().await?;
        let entries = list["data"].as_array().cloned().unwrap_or_default();
        let entry = if self.configured_model.is_empty() {
            match entries.as_slice() {
                [only] => Some(only),
                [] => bail!("the LLM server reports no model"),
                _ => bail!(
                    "the LLM server serves several models ({}); set llm.model",
                    entries
                        .iter()
                        .filter_map(|m| m["id"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        } else {
            entries
                .iter()
                .find(|m| m["id"] == self.configured_model.as_str())
        };
        let meta = entry.map(|m| &m["meta"]).unwrap_or(&Value::Null);
        let name = match entry {
            Some(m) => m["id"].as_str().unwrap_or_default().to_string(),
            None => self.configured_model.clone(),
        };
        let props = self.props().await;
        let info = ModelInfo {
            name,
            n_ctx: meta["n_ctx"]
                .as_i64()
                .or_else(|| props["default_generation_settings"]["n_ctx"].as_i64()),
            n_ctx_train: meta["n_ctx_train"].as_i64(),
            n_params: meta["n_params"].as_i64(),
            size_bytes: meta["size"].as_i64(),
            quantization: meta["ftype"].as_str().map(str::to_string),
            build_info: props["build_info"].as_str().map(str::to_string),
        };
        self.set_info(info.clone());
        Ok(info)
    }

    fn set_info(&self, info: ModelInfo) {
        let mut cur = self.current.lock().expect("model lock");
        if cur.info != info {
            cur.unrecorded = true;
            cur.info = info;
        }
    }

    async fn props(&self) -> Value {
        match self.request(reqwest::Method::GET, "/props").send().await {
            Ok(resp) => resp.json().await.unwrap_or(Value::Null),
            Err(_) => Value::Null,
        }
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
        if self.model().is_empty() {
            self.refresh().await?;
        }
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
        let mut body = json!({
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
        if !self.configured_model.is_empty() {
            body["model"] = json!(self.configured_model);
        }

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
        let model = v["model"].as_str().map(str::to_string);
        // Without a configured name, a different answering model means the server was
        // switched since we last asked: pick up the new details.
        if self.configured_model.is_empty() && model.as_deref().is_some_and(|m| m != self.model()) {
            let _ = self.refresh().await;
        }
        Ok(Completion {
            model,
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
