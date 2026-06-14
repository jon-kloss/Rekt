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
        // The API key is an `x-api-key` header; neither it nor the prompt
        // body is logged — only the non-sensitive request shape.
        tracing::debug!(
            model = request["model"].as_str().unwrap_or("?"),
            max_tokens = request["max_tokens"].as_u64().unwrap_or(0),
            tools = request["tools"].as_array().map(|t| t.len()).unwrap_or(0),
            "POST /v1/messages"
        );
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
        tracing::debug!(status = %status, "messages response");
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
            _ => {
                let parsed: MessageResponse = response
                    .json()
                    .await
                    .map_err(|e| AnalystError::BadResponse(e.to_string()))?;
                tracing::debug!(
                    stop_reason = parsed.stop_reason.as_deref().unwrap_or("?"),
                    input_tokens = parsed.usage.input_tokens,
                    output_tokens = parsed.usage.output_tokens,
                    "messages parsed"
                );
                Ok(parsed)
            }
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

/// Parse the JSON object `claude -p --output-format json` prints into a
/// [`MessageResponse`]. Factored out so it's unit-testable without the CLI.
fn parse_cli_output(stdout: &str) -> Result<MessageResponse, AnalystError> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| AnalystError::BadResponse(format!("claude -p output not JSON: {e}")))?;
    if v["is_error"].as_bool().unwrap_or(false)
        || v["subtype"].as_str() == Some("error_during_execution")
    {
        let msg = v["result"]
            .as_str()
            .unwrap_or("claude -p reported an error")
            .to_string();
        return Err(AnalystError::Api {
            status: 0,
            message: msg,
        });
    }
    let text = v["result"].as_str().unwrap_or_default().to_string();
    // The CLI's `usage` block uses the same field names as the API's.
    let usage: Usage = serde_json::from_value(v["usage"].clone()).unwrap_or_default();
    Ok(MessageResponse {
        content: vec![serde_json::json!({ "type": "text", "text": text })],
        stop_reason: Some("end_turn".to_string()),
        usage,
    })
}

/// Transport that drives the analyst through the local Claude Code CLI
/// (`claude -p`) instead of the HTTP API, so a self-hoster on the same
/// machine reuses their existing Claude Code auth (subscription or key) —
/// no `ANTHROPIC_API_KEY` needed.
///
/// SAFETY: the CLI is launched with `--allowed-tools ""`, so it can run NO
/// tools — it cannot read files, run bash, or reach REKT's order API. The
/// advisory-only invariant holds two ways: by the crate graph (this crate
/// still has no path to `rekt-broker`) AND by the empty tool allowlist. The
/// server runs every analysis kind tool-lessly (context injected into the
/// prompt) when this transport is active.
pub struct CliTransport {
    bin: String,
    timeout: Duration,
}

impl CliTransport {
    pub fn new() -> Self {
        Self {
            bin: "claude".to_string(),
            // Deep reviews can run minutes; bound it like the HTTP client.
            timeout: Duration::from_secs(600),
        }
    }

    pub fn with_bin(bin: String) -> Self {
        Self { bin, ..Self::new() }
    }

    /// Whether the CLI is actually runnable (`<bin> --version` exits 0). Lets
    /// the caller disable the analyst honestly at startup instead of
    /// advertising a backend that fails to spawn on every run.
    pub async fn is_available(&self) -> bool {
        use std::process::Stdio;
        let probe = tokio::process::Command::new(&self.bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        matches!(
            tokio::time::timeout(Duration::from_secs(10), probe).await,
            Ok(Ok(status)) if status.success()
        )
    }
}

impl Default for CliTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for CliTransport {
    async fn send(&self, request: &serde_json::Value) -> Result<MessageResponse, AnalystError> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let model = request["model"].as_str().unwrap_or(ANALYST_MODEL);
        let system = request["system"]
            .get(0)
            .and_then(|s| s["text"].as_str())
            .unwrap_or("");
        // Tool-less runs carry a single user turn whose content is a string.
        let prompt = request["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .filter(|m| m["role"] == "user")
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default();

        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("json")
            // EMPTY allowlist = the CLI may run no tools (no orders, no I/O).
            .arg("--allowed-tools")
            .arg("")
            .arg("--model")
            .arg(model);
        if !system.is_empty() {
            cmd.arg("--append-system-prompt").arg(system);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        tracing::debug!(model, "claude -p (CLI transport)");
        let mut child = cmd
            .spawn()
            .map_err(|e| AnalystError::Network(format!("could not launch `{}`: {e}", self.bin)))?;
        // Feed stdin from a separate task so the parent keeps draining stdout
        // and stderr while it writes: writing the whole prompt before reading
        // output would deadlock if the child fills its (undrained) output pipe
        // mid-write — a real risk for a large weekly-review prompt.
        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = stdin.write_all(prompt.as_bytes()).await; // drop closes → EOF
            });
        }
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| AnalystError::Network("claude -p timed out".into()))?
            .map_err(|e| AnalystError::Network(e.to_string()))?;
        if !out.status.success() && out.stdout.is_empty() {
            let err: String = String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(300)
                .collect();
            return Err(AnalystError::Api {
                status: 0,
                message: format!("claude -p failed: {err}"),
            });
        }
        parse_cli_output(&String::from_utf8_lossy(&out.stdout))
    }
}

/// Transport that drives the analyst through a LOCAL Ollama server
/// (`http://localhost:11434`) — zero-cost, private, offline after one
/// `ollama pull`. Tool-less like the CLI: the server injects context into the
/// prompt and parses the JSON back out. The deterministic screener does the
/// hard part, so even a small local model is enough to narrate candidates.
pub struct OllamaTransport {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaTransport {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Probe Ollama and resolve the configured model to an EXACT pulled tag,
    /// returning the transport ready to use — or `None` if Ollama is
    /// unreachable or the model isn't pulled (so the analyst degrades honestly
    /// instead of failing every run). Resolving the exact tag matters: a bare
    /// `llama3.1` passed to `/api/chat` can 404 when only `llama3.1:8b` is
    /// pulled, even though `/api/tags` lists it.
    pub async fn resolve(mut self) -> Option<Self> {
        let resp = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v = resp.json::<serde_json::Value>().await.ok()?;
        // Prefer an exact name match; else the first variant of the family
        // (so a configured `llama3.1` resolves to the pulled `llama3.1:8b`).
        let want = self
            .model
            .split(':')
            .next()
            .unwrap_or(&self.model)
            .to_string();
        let exact = v["models"].as_array()?.iter().find_map(|m| {
            let n = m["name"].as_str()?;
            (n == self.model || n.split(':').next() == Some(want.as_str())).then(|| n.to_string())
        })?;
        self.model = exact;
        Some(self)
    }
}

#[async_trait]
impl Transport for OllamaTransport {
    async fn send(&self, request: &serde_json::Value) -> Result<MessageResponse, AnalystError> {
        let system = request["system"]
            .get(0)
            .and_then(|s| s["text"].as_str())
            .unwrap_or("");
        // Tool-less runs carry a single user turn whose content is a string.
        let prompt = request["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .filter(|m| m["role"] == "user")
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default();
        let mut messages = Vec::new();
        if !system.is_empty() {
            messages.push(serde_json::json!({"role": "system", "content": system}));
        }
        messages.push(serde_json::json!({"role": "user", "content": prompt}));
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        });

        tracing::debug!(model = %self.model, "POST ollama /api/chat");
        let resp = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| AnalystError::Network(format!("ollama unreachable: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let msg: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            return Err(AnalystError::Api {
                status: status.as_u16(),
                message: format!("ollama: {msg}"),
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AnalystError::BadResponse(format!("ollama response not JSON: {e}")))?;
        Ok(parse_ollama_chat(&v))
    }
}

/// Map an Ollama `/api/chat` response into a [`MessageResponse`]. Factored out
/// so it's unit-testable without a running Ollama. Cost is $0 (local).
fn parse_ollama_chat(v: &serde_json::Value) -> MessageResponse {
    let text = v["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let usage = Usage {
        input_tokens: v["prompt_eval_count"].as_u64().unwrap_or(0),
        output_tokens: v["eval_count"].as_u64().unwrap_or(0),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    MessageResponse {
        content: vec![serde_json::json!({ "type": "text", "text": text })],
        stop_reason: Some("end_turn".to_string()),
        usage,
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

    #[test]
    fn cli_output_parses_into_an_end_turn_response() {
        // Captured shape of `claude -p --output-format json`.
        let out = r#"{"type":"result","subtype":"success","is_error":false,
            "result":"Portfolio is concentrated in semis.","stop_reason":"end_turn",
            "total_cost_usd":0.0123,
            "usage":{"input_tokens":12,"output_tokens":40,"cache_creation_input_tokens":2000,"cache_read_input_tokens":0}}"#;
        let resp = parse_cli_output(out).unwrap();
        assert_eq!(resp.text(), "Portfolio is concentrated in semis.");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 40);
        assert_eq!(resp.usage.cache_creation_input_tokens, Some(2000));
    }

    #[test]
    fn cli_error_output_surfaces_as_an_error() {
        let out = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}"#;
        assert!(matches!(
            parse_cli_output(out),
            Err(AnalystError::Api { .. })
        ));
        assert!(parse_cli_output("not json").is_err());
    }

    #[test]
    fn ollama_chat_parses_message_and_token_counts() {
        // Captured shape of Ollama's POST /api/chat (stream:false).
        let v: serde_json::Value = serde_json::from_str(
            r#"{"model":"llama3.2","message":{"role":"assistant","content":"MSFT is oversold."},
                "done":true,"prompt_eval_count":120,"eval_count":35}"#,
        )
        .unwrap();
        let resp = parse_ollama_chat(&v);
        assert_eq!(resp.text(), "MSFT is oversold.");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.usage.input_tokens, 120);
        assert_eq!(resp.usage.output_tokens, 35);
        // A malformed/empty response degrades to empty text, never panics.
        assert_eq!(parse_ollama_chat(&serde_json::json!({})).text(), "");
    }
}
