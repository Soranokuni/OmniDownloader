use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::info;

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

pub struct LlmClient {
    client: Client,
    endpoint: String,
    model: String,
    /// The server rejected `response_format: json_schema` once; use plain
    /// JSON mode from then on.
    no_json_schema: AtomicBool,
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

impl LlmClient {
    pub fn new(endpoint: &str, model: &str) -> Self {
        Self::with_timeout(endpoint, model, Duration::from_secs(240))
    }

    /// A client whose every request gives up after `timeout` (plan P4.4:
    /// `llm.timeout_secs`, 30 s — the poll loop waits on it).
    pub fn with_timeout(endpoint: &str, model: &str, timeout: Duration) -> Self {
        let clean_endpoint = endpoint.trim_end_matches('/');
        let final_endpoint = if clean_endpoint.ends_with("/chat/completions") {
            clean_endpoint.to_string()
        } else if clean_endpoint.ends_with("/v1") {
            format!("{}/chat/completions", clean_endpoint)
        } else {
            format!("{}/v1/chat/completions", clean_endpoint)
        };

        let client = Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        Self {
            client,
            endpoint: final_endpoint,
            model: model.to_string(),
            no_json_schema: AtomicBool::new(false),
        }
    }

    /// One chat completion that must answer with a JSON object.
    ///
    /// Asks for `response_format: json_schema` (Ollama >= 0.5, llama.cpp,
    /// vLLM); a server that rejects it with a 4xx is asked again, and from
    /// then on, with plain `json_object` mode — the schema is in the prompt
    /// either way. Temperature 0: the same email should get the same answer.
    pub async fn chat_json(
        &self,
        system: &str,
        user: &str,
        schema: &serde_json::Value,
        max_tokens: u32,
    ) -> Result<serde_json::Value> {
        loop {
            let use_schema = !self.no_json_schema.load(Ordering::Relaxed);
            let response_format = if use_schema {
                serde_json::json!({ "type": "json_schema", "json_schema": { "name": "assist", "schema": schema } })
            } else {
                serde_json::json!({ "type": "json_object" })
            };
            let payload = serde_json::json!({
                "model": self.model,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user}
                ],
                "temperature": 0,
                "max_tokens": max_tokens,
                "stream": false,
                "response_format": response_format,
            });
            let resp = self
                .client
                .post(&self.endpoint)
                .json(&payload)
                .send()
                .await
                .with_context(|| format!("LLM unreachable at {}", self.endpoint))?;
            let status = resp.status();
            if status.is_client_error() && use_schema {
                self.no_json_schema.store(true, Ordering::Relaxed);
                continue;
            }
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow!("LLM HTTP error {}: {}", status, text.chars().take(200).collect::<String>()));
            }
            let data: serde_json::Value = resp.json().await.context("LLM answer is not JSON")?;
            let content = data
                .pointer("/choices/0/message/content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("LLM answer has no message content"))?;
            return serde_json::from_str(strip_json_fence(content))
                .with_context(|| format!("LLM content is not a JSON object: {}", content.chars().take(200).collect::<String>()));
        }
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

    pub async fn ping(&self) -> bool {
        let base = self.endpoint.replace("/chat/completions", "/models");
        if let Ok(resp) = self.client.get(&base).timeout(Duration::from_secs(4)).send().await {
            resp.status().is_success()
        } else {
            false
        }
    }
}
