use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::info;

use omni_core::config::{LlmAuth, LlmConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmJob {
    pub url: String,
    #[serde(default = "default_index_str")]
    pub index_str: String,
    #[serde(default = "default_keyword")]
    pub keyword: String,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
}

fn default_index_str() -> String {
    "1".to_string()
}
fn default_keyword() -> String {
    "ASSET".to_string()
}
fn default_confidence() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmParseResult {
    #[serde(default = "default_journalist")]
    pub journalist_surname: String,
    #[serde(default)]
    pub jobs: Vec<LlmJob>,
}

fn default_journalist() -> String {
    "MCR".to_string()
}

/// One OpenAI-compatible chat endpoint, local or online (plan P4.22).
///
/// Nearly every runtime and provider speaks this protocol: LM Studio,
/// Ollama, GenieX, llama.cpp and vLLM locally; OpenAI, Azure OpenAI (v1 API),
/// Google Gemini, Anthropic, OpenRouter, Mistral and Groq online. What
/// differs is how the key is sent (`LlmAuth`) and which optional request
/// fields a server accepts. The client sends the most useful form first and,
/// when a server answers 400/422, drops the field it objects to and
/// remembers that, so one configuration works everywhere.
pub struct LlmClient {
    client: Client,
    /// `…/chat/completions`.
    endpoint: String,
    model: String,
    auth: LlmAuth,
    api_key: String,
    disable_thinking: bool,
    /// The server refused `response_format: json_schema`: use `json_object`.
    no_json_schema: AtomicBool,
    /// …and refused `json_object` too: no `response_format` at all (the
    /// prompt still asks for JSON, and the answer is validated either way).
    no_response_format: AtomicBool,
    /// Refused `reasoning_effort`.
    no_reasoning_param: AtomicBool,
    /// Refused `temperature` (some reasoning models accept only the default).
    no_temperature: AtomicBool,
    /// Wants `max_completion_tokens` instead of `max_tokens` (newer OpenAI).
    use_max_completion_tokens: AtomicBool,
}

impl std::fmt::Debug for LlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key: this can end up in a log line.
        f.debug_struct("LlmClient")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

/// Strip a Markdown code fence some models wrap JSON in.
pub fn strip_json_fence(content: &str) -> &str {
    let s = content.trim();
    if let Some(rest) = s.strip_prefix("```json") {
        rest.trim_end_matches("```").trim()
    } else if let Some(rest) = s.strip_prefix("```") {
        rest.trim_end_matches("```").trim()
    } else {
        s
    }
}

/// The JSON object in a model's answer: the whole answer, the inside of a
/// code fence, or the outermost `{…}` when the model wrote a sentence first.
pub fn extract_json_object(content: &str) -> Result<serde_json::Value> {
    let fenced = strip_json_fence(content);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(fenced) {
        if v.is_object() {
            return Ok(v);
        }
    }
    if let (Some(a), Some(b)) = (fenced.find('{'), fenced.rfind('}')) {
        if a < b {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&fenced[a..=b]) {
                if v.is_object() {
                    return Ok(v);
                }
            }
        }
    }
    Err(anyhow!("LLM answer is not a JSON object: {}", content.chars().take(200).collect::<String>()))
}

/// Whether `base_url` is on this machine or the station network. Anything
/// else is "online": mail content leaves the building, so the assist redacts
/// contact details first (plan P4.22).
pub fn is_local_endpoint(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url.trim()) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(std::net::IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Err(_) => {
            host == "localhost"
                || host.ends_with(".localhost")
                || host.ends_with(".local")
                || host.ends_with(".lan")
                || host.ends_with(".internal")
                || !host.contains('.')
        }
    }
}

/// A base URL the panel may save: http(s), and https for anything online
/// (the key and mail text would otherwise cross the internet in clear).
pub fn check_base_url(base_url: &str) -> std::result::Result<(), String> {
    let url = reqwest::Url::parse(base_url.trim()).map_err(|_| "The base URL is not a URL.".to_string())?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_local_endpoint(base_url) => Ok(()),
        "http" => Err("An online provider must use https://: the key and the mail text would cross the internet in clear.".into()),
        _ => Err("The base URL must start with http:// or https://.".into()),
    }
}

/// `…/v1` + `/chat/completions`, whatever form the operator typed.
fn chat_url(endpoint: &str) -> String {
    let clean = endpoint.trim().trim_end_matches('/');
    if clean.ends_with("/chat/completions") {
        clean.to_string()
    } else if clean.ends_with("/v1") || clean.contains("/openai") || clean.ends_with("/v1beta") {
        format!("{clean}/chat/completions")
    } else {
        format!("{clean}/v1/chat/completions")
    }
}

impl LlmClient {
    pub fn new(endpoint: &str, model: &str) -> Self {
        Self::with_timeout(endpoint, model, Duration::from_secs(240))
    }

    /// A keyless client whose every request gives up after `timeout`.
    pub fn with_timeout(endpoint: &str, model: &str, timeout: Duration) -> Self {
        let cfg = LlmConfig {
            timeout_secs: timeout.as_secs().max(1),
            disable_thinking: false,
            ..LlmConfig::default()
        };
        Self::from_settings(endpoint, model, &cfg)
    }

    /// A client for the configured provider: key, auth style, thinking.
    pub fn from_settings(endpoint: &str, model: &str, cfg: &LlmConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.clamp(1, 600)))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            client,
            endpoint: chat_url(endpoint),
            model: model.trim().to_string(),
            auth: cfg.auth,
            api_key: cfg.api_key.trim().to_string(),
            disable_thinking: cfg.disable_thinking,
            no_json_schema: AtomicBool::new(false),
            no_response_format: AtomicBool::new(false),
            no_reasoning_param: AtomicBool::new(false),
            no_temperature: AtomicBool::new(false),
            use_max_completion_tokens: AtomicBool::new(false),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Mail content sent here leaves the station.
    pub fn is_online(&self) -> bool {
        !is_local_endpoint(&self.endpoint)
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            return rb;
        }
        match self.auth {
            LlmAuth::None => rb,
            LlmAuth::Bearer => rb.bearer_auth(&self.api_key),
            LlmAuth::ApiKeyHeader => rb.header("api-key", &self.api_key),
        }
    }

    /// An HTTP failure in words an operator can act on. Never the key.
    fn explain(&self, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
        let detail: String = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.pointer("/error/message")
                    .or_else(|| v.pointer("/error"))
                    .or_else(|| v.pointer("/message"))
                    .and_then(|m| m.as_str().map(String::from))
            })
            .unwrap_or_else(|| body.to_string())
            .chars()
            .take(200)
            .collect();
        let hint = match status.as_u16() {
            401 | 403 => "the provider refused the API key: check the key and how it is sent (Bearer, or api-key for Azure)",
            404 => "not found: check the base URL and that the model id is exactly as the provider lists it",
            408 | 504 => "the provider timed out",
            429 => "rate limited or out of quota at the provider; parsing continues without the assist",
            500..=599 => "the provider had an internal error",
            _ => "the provider rejected the request",
        };
        anyhow!("LLM HTTP {status}: {hint}. {detail}")
    }

    /// One chat completion that must answer with a JSON object.
    ///
    /// Sends `response_format: json_schema`, `temperature: 0`, `max_tokens`
    /// and, with `disable_thinking`, `reasoning_effort: "none"`. A 400/422
    /// that names one of those fields drops it; one that names none steps
    /// the response format down (schema → json_object → none). Each step is
    /// remembered for the life of the client. Temperature 0: the same email
    /// should get the same answer.
    pub async fn chat_json(
        &self,
        system: &str,
        user: &str,
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<serde_json::Value> {
        for _attempt in 0..6 {
            let mut payload = serde_json::json!({
                "model": self.model,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user}
                ],
                "stream": false,
            });
            let sent_reasoning = self.disable_thinking && !self.no_reasoning_param.load(Ordering::Relaxed);
            if sent_reasoning {
                payload["reasoning_effort"] = serde_json::json!("none");
            }
            let sent_temperature = !self.no_temperature.load(Ordering::Relaxed);
            if sent_temperature {
                payload["temperature"] = serde_json::json!(0);
            }
            let tokens_field = if self.use_max_completion_tokens.load(Ordering::Relaxed) {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            payload[tokens_field] = serde_json::json!(max_tokens);
            let use_schema = !self.no_json_schema.load(Ordering::Relaxed);
            let use_format = !self.no_response_format.load(Ordering::Relaxed);
            if use_format {
                payload["response_format"] = if use_schema {
                    serde_json::json!({ "type": "json_schema", "json_schema": { "name": "assist", "schema": schema } })
                } else {
                    serde_json::json!({ "type": "json_object" })
                };
            }

            let resp = self
                .authed(self.client.post(&self.endpoint))
                .json(&payload)
                .send()
                .await
                .with_context(|| format!("LLM unreachable at {}", self.endpoint))?;
            let status = resp.status();
            if status.as_u16() == 400 || status.as_u16() == 422 {
                let body = resp.text().await.unwrap_or_default();
                let lower = body.to_ascii_lowercase();
                if sent_reasoning && lower.contains("reasoning") {
                    self.no_reasoning_param.store(true, Ordering::Relaxed);
                } else if sent_temperature && lower.contains("temperature") {
                    self.no_temperature.store(true, Ordering::Relaxed);
                } else if tokens_field == "max_tokens" && lower.contains("max_tokens") {
                    self.use_max_completion_tokens.store(true, Ordering::Relaxed);
                } else if use_format && use_schema {
                    self.no_json_schema.store(true, Ordering::Relaxed);
                } else if use_format {
                    self.no_response_format.store(true, Ordering::Relaxed);
                } else if sent_reasoning {
                    self.no_reasoning_param.store(true, Ordering::Relaxed);
                } else {
                    return Err(self.explain(status, &body));
                }
                info!("LLM at {} refused a request field; retrying without it", self.endpoint);
                continue;
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(self.explain(status, &body));
            }
            let data: serde_json::Value = resp.json().await.context("LLM answer is not JSON")?;
            let choice = data.pointer("/choices/0").ok_or_else(|| anyhow!("LLM answer has no choices"))?;
            let content = message_text(choice.pointer("/message/content"));
            if content.trim().is_empty() {
                if choice.pointer("/finish_reason").and_then(|v| v.as_str()) == Some("length") {
                    bail!(
                        "the model used its whole budget of {max_tokens} tokens without answering (thinking?): \
                         turn on \"Disable thinking\" or raise the token limit"
                    );
                }
                bail!("LLM answer is empty");
            }
            return extract_json_object(&content);
        }
        bail!("LLM at {} kept refusing the request", self.endpoint)
    }

    /// Model ids the server offers (`GET …/models`), for the admin panel.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = self.endpoint.replace("/chat/completions", "/models");
        let resp = self
            .authed(self.client.get(&url))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .with_context(|| format!("LLM unreachable at {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(self.explain(status, &body));
        }
        let data: serde_json::Value = resp.json().await.context("model list is not JSON")?;
        let mut ids: Vec<String> = data
            .get("data")
            .or_else(|| data.get("models"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").or_else(|| m.get("name")).and_then(|v| v.as_str()))
                    .map(|s| s.trim_start_matches("models/").to_string())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    pub async fn parse_email(&self, system_prompt: &str, email_body: &str) -> Result<LlmParseResult> {
        let payload = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": email_body}
            ],
            "temperature": 0.1,
            "stream": false
        });

        info!("Sending email to LLM at {} (model: {})...", self.endpoint, self.model);
        let resp = self
            .client
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("LLM HTTP request failed to {}", self.endpoint))?;
        let resp = resp;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("LLM HTTP error {}: {}", status, text));
        }

        let data: serde_json::Value = resp.json().await?;
        let content_str = data
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Unexpected OpenAI format from LLM: {:?}", data))?
            .trim();

        // Strip markdown fences
        let cleaned = if content_str.starts_with("```json") {
            content_str.trim_start_matches("```json").trim_end_matches("```").trim()
        } else if content_str.starts_with("```") {
            content_str.trim_start_matches("```").trim_end_matches("```").trim()
        } else {
            content_str
        };

        let mut parsed: LlmParseResult = serde_json::from_str(cleaned)
            .with_context(|| format!("Failed decoding JSON from LLM: '{}'", cleaned))?;

        if parsed.journalist_surname.trim().is_empty() {
            parsed.journalist_surname = "MCR".to_string();
        } else {
            parsed.journalist_surname = parsed.journalist_surname.trim().to_uppercase();
        }

        Ok(parsed)
    }

    /// Reachable, and the key (if any) accepted.
    pub async fn ping(&self) -> bool {
        self.list_models().await.is_ok()
    }
}

/// A message's text: a plain string, or the text parts of a content array
/// (some providers answer in parts).
fn message_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}
