//! The manual agentic loop: call the Messages API, execute client tool
//! calls through the read-only [`ToolExecutor`], feed results back, repeat
//! until the model finishes (or honestly fail — never fabricate an answer).

use serde_json::{json, Value};

use crate::{AnalystError, ToolExecutor, Transport, UsageTotals};

/// Everything needed to run one analysis to completion.
pub struct RunConfig<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    /// Stable system prompt — cached with an ephemeral breakpoint, so keep
    /// it byte-identical across runs (volatile data goes in `user_content`).
    pub system: &'a str,
    /// The user turn: task + injected context (portfolio JSON etc.).
    pub user_content: String,
    /// Adaptive thinking (Opus). Haiku runs without a thinking block.
    pub adaptive_thinking: bool,
    /// Extra server-side tools (e.g. web_search) appended after client tools.
    pub server_tools: Vec<Value>,
    /// Forces the FINAL response into this JSON schema when set.
    pub output_schema: Option<Value>,
    /// Hard cap on loop iterations (each is one API call).
    pub max_iterations: usize,
}

/// The result of a completed loop.
#[derive(Debug)]
pub struct RunOutcome {
    /// Final response text (or the raw JSON when `output_schema` was set).
    pub text: String,
    pub usage: UsageTotals,
    /// One entry per executed client tool call: {name, input, ok}.
    pub tool_log: Vec<Value>,
}

/// A failed loop STILL carries the usage that was billed before it failed —
/// dropping it would make failed runs invisible to budget accounting.
#[derive(Debug)]
pub struct RunFailure {
    pub error: AnalystError,
    pub usage: UsageTotals,
    pub tool_log: Vec<Value>,
}

pub async fn run(
    transport: &dyn Transport,
    tools: Option<&dyn ToolExecutor>,
    config: RunConfig<'_>,
) -> Result<RunOutcome, RunFailure> {
    let mut tool_definitions: Vec<Value> = tools.map(|t| t.definitions()).unwrap_or_default();
    tool_definitions.extend(config.server_tools.iter().cloned());

    let mut messages: Vec<Value> = vec![json!({
        "role": "user",
        "content": config.user_content,
    })];
    let mut usage = UsageTotals::default();
    let mut tool_log: Vec<Value> = Vec::new();
    // Any early exit must carry the usage accumulated so far.
    macro_rules! fail {
        ($error:expr) => {
            return Err(RunFailure {
                error: $error,
                usage,
                tool_log,
            })
        };
    }

    for _ in 0..config.max_iterations {
        let mut request = json!({
            "model": config.model,
            "max_tokens": config.max_tokens,
            // Stable prefix first, cache breakpoint on the system block
            // (which also covers the tools rendered before it — for runs
            // that share the same tool set; tool sets differ per kind).
            "system": [{
                "type": "text",
                "text": config.system,
                "cache_control": {"type": "ephemeral"},
            }],
            "messages": messages,
        });
        if !tool_definitions.is_empty() {
            request["tools"] = Value::Array(tool_definitions.clone());
        }
        if config.adaptive_thinking {
            request["thinking"] = json!({"type": "adaptive"});
        }
        if let Some(schema) = &config.output_schema {
            request["output_config"] = json!({"format": schema});
        }

        let response = match transport.send(&request).await {
            Ok(response) => response,
            Err(error) => fail!(error),
        };
        usage.add(&response.usage);

        match response.stop_reason.as_deref() {
            Some("end_turn") | Some("stop_sequence") => {
                // Structured outputs guarantee the FIRST text block is the
                // schema-valid JSON — trailing commentary blocks must not
                // be concatenated into it.
                let text = if config.output_schema.is_some() {
                    response.structured_text()
                } else {
                    response.text()
                };
                return Ok(RunOutcome {
                    text,
                    usage,
                    tool_log,
                });
            }
            Some("max_tokens") => {
                // Truncated output is dishonest output — fail loudly.
                fail!(AnalystError::BadResponse(
                    "response hit max_tokens before finishing".into(),
                ));
            }
            Some("refusal") => fail!(AnalystError::Refused),
            // Server-side tool (web search) paused its loop: echo the
            // assistant turn back verbatim and the API resumes on its own.
            Some("pause_turn") => {
                messages.push(json!({"role": "assistant", "content": response.content}));
            }
            Some("tool_use") => {
                let calls = response.tool_uses();
                messages.push(json!({"role": "assistant", "content": response.content}));
                let Some(executor) = tools else {
                    fail!(AnalystError::BadResponse(
                        "model called a tool but no executor is configured".into(),
                    ));
                };
                let mut results: Vec<Value> = Vec::with_capacity(calls.len());
                for (id, name, input) in calls {
                    let (content, is_error) = match executor.execute(&name, &input).await {
                        Ok(output) => (output, false),
                        Err(message) => (message, true),
                    };
                    tool_log.push(json!({"name": name, "input": input, "ok": !is_error}));
                    results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": content,
                        "is_error": is_error,
                    }));
                }
                messages.push(json!({"role": "user", "content": results}));
            }
            other => {
                fail!(AnalystError::BadResponse(format!(
                    "unexpected stop_reason {other:?}"
                )));
            }
        }
    }
    Err(RunFailure {
        error: AnalystError::LoopLimit(config.max_iterations),
        usage,
        tool_log,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::MessageResponse;

    /// Scripted transport: pops pre-baked responses, records requests.
    struct Script {
        responses: Mutex<Vec<MessageResponse>>,
        requests: Mutex<Vec<Value>>,
    }

    impl Script {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .rev()
                        .map(|v| serde_json::from_value(v).unwrap())
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Transport for Script {
        async fn send(&self, request: &Value) -> Result<MessageResponse, AnalystError> {
            self.requests.lock().unwrap().push(request.clone());
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| AnalystError::BadResponse("script exhausted".into()))
        }
    }

    struct FakeTools;

    #[async_trait]
    impl ToolExecutor for FakeTools {
        fn definitions(&self) -> Vec<Value> {
            vec![
                json!({"name": "get_portfolio", "description": "d", "input_schema": {"type": "object"}}),
            ]
        }
        async fn execute(&self, name: &str, _input: &Value) -> Result<String, String> {
            match name {
                "get_portfolio" => Ok("{\"equity\": \"1000\"}".into()),
                other => Err(format!("unknown tool {other}")),
            }
        }
    }

    fn config() -> RunConfig<'static> {
        RunConfig {
            model: "claude-opus-4-8",
            max_tokens: 1000,
            system: "You are a test.",
            user_content: "go".into(),
            adaptive_thinking: true,
            server_tools: vec![],
            output_schema: None,
            max_iterations: 5,
        }
    }

    #[tokio::test]
    async fn tool_loop_executes_and_finishes() {
        let script = Script::new(vec![
            json!({
                "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "get_portfolio", "input": {}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            json!({
                "content": [{"type": "text", "text": "All good."}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 20, "output_tokens": 7}
            }),
        ]);
        let outcome = run(&script, Some(&FakeTools), config()).await.unwrap();
        assert_eq!(outcome.text, "All good.");
        assert_eq!(outcome.usage.requests, 2);
        assert_eq!(outcome.usage.input_tokens, 30);
        assert_eq!(outcome.tool_log.len(), 1);
        assert_eq!(outcome.tool_log[0]["ok"], true);

        // The second request must carry the assistant echo + tool_result.
        let requests = script.requests.lock().unwrap();
        let followup = &requests[1]["messages"];
        assert_eq!(followup.as_array().unwrap().len(), 3);
        assert_eq!(followup[1]["role"], "assistant");
        assert_eq!(followup[2]["content"][0]["type"], "tool_result");
        assert_eq!(followup[2]["content"][0]["tool_use_id"], "tu_1");
        // Adaptive thinking + cached system prompt on every call.
        assert_eq!(requests[0]["thinking"]["type"], "adaptive");
        assert_eq!(
            requests[0]["system"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[tokio::test]
    async fn pause_turn_resumes_and_refusal_is_honest() {
        let script = Script::new(vec![
            json!({
                "content": [
                    {"type": "server_tool_use", "id": "st_1", "name": "web_search", "input": {"query": "x"}}
                ],
                "stop_reason": "pause_turn",
                "usage": {"input_tokens": 5, "output_tokens": 1}
            }),
            json!({
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 5, "output_tokens": 2}
            }),
        ]);
        let outcome = run(&script, Some(&FakeTools), config()).await.unwrap();
        assert_eq!(outcome.text, "done");
        // pause_turn echoes the assistant content without adding tool results.
        // (Scoped so the guard drops before the next await.)
        {
            let requests = script.requests.lock().unwrap();
            assert_eq!(requests[1]["messages"].as_array().unwrap().len(), 2);
        }

        let refusing = Script::new(vec![json!({
            "content": [],
            "stop_reason": "refusal",
            "usage": {"input_tokens": 1, "output_tokens": 0}
        })]);
        let failure = run(&refusing, Some(&FakeTools), config())
            .await
            .unwrap_err();
        assert!(matches!(failure.error, AnalystError::Refused));
        // The tokens the refused call billed survive the failure.
        assert_eq!(failure.usage.input_tokens, 1);
    }

    #[tokio::test]
    async fn loop_limit_and_tool_errors_surface() {
        // A model that calls tools forever hits the iteration cap.
        let endless: Vec<Value> = (0..5)
            .map(|i| {
                json!({
                    "content": [
                        {"type": "tool_use", "id": format!("tu_{i}"), "name": "nope", "input": {}}
                    ],
                    "stop_reason": "tool_use",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })
            })
            .collect();
        let script = Script::new(endless);
        let failure = run(&script, Some(&FakeTools), config()).await.unwrap_err();
        assert!(matches!(failure.error, AnalystError::LoopLimit(5)));
        // Five paid calls are still accounted on the failure path.
        assert_eq!(failure.usage.requests, 5);
        assert_eq!(failure.usage.input_tokens, 5);
        // Unknown tool became an is_error result, not a crash.
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests[1]["messages"][2]["content"][0]["is_error"], true);
    }
}
