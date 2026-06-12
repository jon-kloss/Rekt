//! REKT's AI analyst layer: a Claude Messages API client, a manual tool-use
//! loop, and cost metering (PLAN.md §5 layer 2).
//!
//! HARD INVARIANT: this crate never depends on `rekt-broker`. The analyst is
//! advisory only — it can read portfolio data through the [`ToolExecutor`]
//! trait the server implements, but nothing in this dependency graph can
//! place an order. Recommendations only ever prefill a ticket that a human
//! confirms through the normal guardrailed order path.
//!
//! Rust has no official Anthropic SDK, so this is a thin raw-HTTP client for
//! `POST /v1/messages` (anthropic-version 2023-06-01). Content blocks are
//! kept as raw `serde_json::Value`s so unknown block types (server tool
//! results, thinking blocks) round-trip losslessly when echoed back.

pub mod pricing;
pub mod runner;

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

pub const API_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Morning briefings are short and frequent → the fast, cheap model.
pub const BRIEFING_MODEL: &str = "claude-haiku-4-5";
/// Deep reviews and on-demand analysis → the most capable Opus-tier model.
pub const ANALYST_MODEL: &str = "claude-opus-4-8";

#[derive(Debug, thiserror::Error)]
pub enum AnalystError {
    #[error("ANTHROPIC_API_KEY rejected (401) — check the key")]
    Auth,
    #[error("Claude API rate limited (429) — try again shortly")]
    RateLimited,
    #[error("Claude API overloaded — try again shortly")]
    Overloaded,
    #[error("Claude API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("network error talking to the Claude API: {0}")]
    Network(String),
    #[error("the model declined this request (refusal)")]
    Refused,
    #[error("tool loop exceeded {0} iterations without finishing")]
    LoopLimit(usize),
    #[error("bad response shape: {0}")]
    BadResponse(String),
}

/// Token usage for one API call, as reported by the response `usage` block.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

/// Accumulated usage across every call in a tool loop.
#[derive(Debug, Clone, Default)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Number of API calls made.
    pub requests: u32,
}

impl UsageTotals {
    pub fn add(&mut self, usage: &Usage) {
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_input_tokens.unwrap_or(0);
        self.cache_write_tokens += usage.cache_creation_input_tokens.unwrap_or(0);
        self.requests += 1;
    }
}

/// One `POST /v1/messages` response, content kept raw for lossless echo.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageResponse {
    pub content: Vec<serde_json::Value>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Usage,
}

impl MessageResponse {
    /// Concatenated text blocks (the human-readable answer).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The FIRST text block only — structured outputs guarantee the first
    /// text block carries the schema-valid JSON; any later text block is
    /// commentary that would corrupt a concatenated parse.
    pub fn structured_text(&self) -> String {
        self.content
            .iter()
            .find(|block| block["type"] == "text")
            .and_then(|block| block["text"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// Client-tool calls awaiting execution: (id, name, input).
    /// `server_tool_use` blocks are excluded — Anthropic runs those.
    pub fn tool_uses(&self) -> Vec<(String, String, serde_json::Value)> {
        self.content
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .filter_map(|block| {
                Some((
                    block["id"].as_str()?.to_string(),
                    block["name"].as_str()?.to_string(),
                    block["input"].clone(),
                ))
            })
            .collect()
    }
}

/// The wire to the Claude API — a trait so tests (and the server's tests)
/// can script responses without a network.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, request: &serde_json::Value) -> Result<MessageResponse, AnalystError>;
}

/// Real HTTPS transport with bounded retries on 429/5xx/529.
pub struct HttpTransport {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl HttpTransport {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                // Deep reviews with web search can legitimately run minutes.
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client"),
            api_key,
            base_url: API_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    async fn send_once(
        &self,
        request: &serde_json::Value,
    ) -> Result<MessageResponse, AnalystError> {
        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(request)
            .send()
            .await
            .map_err(|e| AnalystError::Network(e.to_string()))?;

        let status = response.status();
        match status.as_u16() {
            401 => Err(AnalystError::Auth),
            429 => Err(AnalystError::RateLimited),
            529 => Err(AnalystError::Overloaded),
            s if s >= 500 => Err(AnalystError::Overloaded),
            s if s >= 400 => {
                let body = response.text().await.unwrap_or_default();
                // The error body is JSON {type:"error", error:{message}} —
                // surface the message, not the whole envelope.
                let message = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                    .unwrap_or(body);
                Err(AnalystError::Api { status: s, message })
            }
            _ => response
                .json()
                .await
                .map_err(|e| AnalystError::BadResponse(e.to_string())),
        }
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, request: &serde_json::Value) -> Result<MessageResponse, AnalystError> {
        let mut delay = Duration::from_secs(2);
        for attempt in 0..3 {
            match self.send_once(request).await {
                Err(AnalystError::RateLimited | AnalystError::Overloaded) if attempt < 2 => {
                    tracing::warn!(attempt, "Claude API busy — backing off");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                other => return other,
            }
        }
        unreachable!("loop always returns by the last attempt")
    }
}

/// Read-only data the analyst may ask for. Implemented by the server over
/// its own caches/DB; nothing in this trait can mutate anything.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Tool definitions (name/description/input_schema), in a DETERMINISTIC
    /// order — tools render first in the prompt, so a varying order would
    /// silently break prompt caching.
    fn definitions(&self) -> Vec<serde_json::Value>;
    /// Execute one tool call; `Err` becomes an `is_error` tool_result the
    /// model can react to.
    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_text_and_tool_uses_extract() {
        let response: MessageResponse = serde_json::from_value(serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "", "signature": "x"},
                {"type": "text", "text": "Hello"},
                {"type": "tool_use", "id": "tu_1", "name": "get_portfolio", "input": {}},
                {"type": "server_tool_use", "id": "st_1", "name": "web_search", "input": {"query": "spy"}},
                {"type": "text", "text": "world"}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100}
        }))
        .unwrap();
        assert_eq!(response.text(), "Hello\nworld");
        // Structured parses must never see trailing commentary blocks.
        assert_eq!(response.structured_text(), "Hello");
        let tools = response.tool_uses();
        assert_eq!(tools.len(), 1, "server_tool_use must not be executed by us");
        assert_eq!(tools[0].1, "get_portfolio");

        let mut totals = UsageTotals::default();
        totals.add(&response.usage);
        assert_eq!(totals.input_tokens, 10);
        assert_eq!(totals.cache_read_tokens, 100);
        assert_eq!(totals.requests, 1);
    }
}
