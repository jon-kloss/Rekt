//! Phase 5: the AI analyst (PLAN.md §5 layer 2) — morning briefings, weekly
//! deep reviews, and on-demand analysis over the Claude API.
//!
//! Safety posture: the analyst is ADVISORY ONLY. Its tool surface is
//! read-only (portfolio, candles, quotes, performance), `rekt-analyst` has
//! no path to `rekt-broker`, and recommendations only prefill an order
//! ticket the human confirms through /api/orders with every guardrail.
//! Every run is cost-metered and a daily budget gates new runs.

use std::sync::atomic::Ordering;

use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use chrono::{Duration, Utc};
use rekt_analyst::runner::{run, RunConfig};
use rekt_analyst::{pricing, ToolExecutor, ANALYST_MODEL, BRIEFING_MODEL};
use rust_decimal::Decimal;
use serde_json::{json, Value};

use crate::api::{err, internal, validate_symbol, ApiError};
use crate::live::SIGNAL_WINDOW_BARS;
use crate::{repo, AppState};

/// Recommendations lapse after a week — stale advice must expire honestly.
const RECOMMENDATION_TTL_DAYS: i64 = 7;
/// Generous cap on loop iterations (each is one API call).
const MAX_ITERATIONS: usize = 12;

/// Frozen system prompt — byte-identical across runs so the prompt cache
/// holds (volatile data travels in the user turn, never here).
const SYSTEM_PROMPT: &str = "You are the REKT analyst, the AI advisor inside REKT, a self-hosted \
portfolio tracker for one person's US stocks & ETFs.\n\
\n\
Hard rules:\n\
- You are ADVISORY ONLY. You cannot place, modify, or cancel orders, and you must never imply \
that you did. A human reviews and confirms every action through guarded order entry.\n\
- Be honest about uncertainty. If data is missing or a tool fails, say so plainly — never \
fabricate prices, positions, or facts.\n\
- This is analysis and tooling, not financial advice; the user is a self-directed investor. \
Skip boilerplate disclaimers — the product already displays one.\n\
- Quant signals you receive (RSI, SMA distance, drawdown) are deterministic facts computed \
locally. Distinguish facts from your judgment.\n\
\n\
Tools: call get_portfolio first when the question involves current holdings. Call get_candles \
when a judgment depends on price history beyond the provided signals. Use web_search (when \
available) for anything time-sensitive — news, earnings, macro events — rather than answering \
from memory.\n\
\n\
Write tight, concrete analysis. Lead with what matters most. Use plain markdown (headings, \
bold, lists).";

/// Read-only tool surface the model may call. Deterministic definition
/// order (tools render first in the prompt — order changes break caching).
pub struct ServerTools {
    state: AppState,
}

#[async_trait]
impl ToolExecutor for ServerTools {
    fn definitions(&self) -> Vec<Value> {
        vec![
            json!({
                "name": "get_portfolio",
                "description": "Current portfolio snapshot: positions with quantities, cost basis, \
                    live prices, P&L, quant signals, plus cash, equity and watchlist. Call this \
                    when the question involves current holdings or allocation.",
                "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}
            }),
            json!({
                "name": "get_candles",
                "description": "Recent daily closing prices for one symbol, oldest first (up to \
                    ~250 trading days). Call this when a judgment depends on price history or \
                    trend beyond the summary signals.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "symbol": {"type": "string", "description": "Ticker, e.g. AAPL"},
                        "days": {"type": "integer", "description": "How many recent closes (default 60, max 250)"}
                    },
                    "required": ["symbol"],
                    "additionalProperties": false
                }
            }),
            json!({
                "name": "get_performance",
                "description": "Portfolio performance metrics: time-weighted return, IRR, and \
                    cash-flow-matched SPY benchmark over a range. Call this for any question \
                    about how the portfolio has performed.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "range": {"type": "string", "enum": ["1m", "3m", "1y", "all"], "description": "Window (default all)"}
                    },
                    "additionalProperties": false
                }
            }),
        ]
    }

    async fn execute(&self, name: &str, input: &Value) -> Result<String, String> {
        match name {
            "get_portfolio" => {
                let snapshot = crate::live::portfolio_snapshot(&self.state)
                    .await
                    .map_err(|e| format!("portfolio unavailable: {e}"))?;
                // The model gets portfolio + watchlist, not trading internals.
                Ok(json!({
                    "portfolio": snapshot["portfolio"],
                    "watchlist": snapshot["watchlist"],
                    "market": snapshot["market"],
                })
                .to_string())
            }
            "get_candles" => {
                let symbol = input["symbol"]
                    .as_str()
                    .ok_or("symbol is required")?
                    .trim()
                    .to_uppercase();
                let days = input["days"]
                    .as_i64()
                    .unwrap_or(60)
                    .clamp(5, SIGNAL_WINDOW_BARS);
                let closes = repo::recent_closes(&self.state.db, &symbol, days)
                    .await
                    .map_err(|e| format!("candle fetch failed: {e}"))?;
                if closes.is_empty() {
                    return Err(format!("no cached candles for {symbol}"));
                }
                Ok(json!({"symbol": symbol, "closes": closes}).to_string())
            }
            "get_performance" => {
                let range = input["range"].as_str().unwrap_or("all");
                let response = crate::history::history_payload(&self.state, range)
                    .await
                    .map_err(|(_, body)| body.0["error"].to_string())?;
                Ok(json!({
                    "range": range,
                    "metrics": response["metrics"],
                    "totals": response["totals"],
                })
                .to_string())
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
}

/// Structured-output schema for the weekly review: a report plus zero or
/// more advisory recommendations the server can persist.
fn review_schema() -> Value {
    json!({
        "type": "json_schema",
        "schema": {
            "type": "object",
            "properties": {
                "report_md": {"type": "string", "description": "The full review, markdown"},
                "recommendations": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "symbol": {"type": "string"},
                            "action": {"type": "string", "enum": ["buy", "sell", "trim", "hold", "watch"]},
                            "sizing": {"type": "string", "description": "Suggested size in plain words, e.g. '2% of equity' or '5 shares'"},
                            "rationale": {"type": "string"},
                            "confidence": {"type": "string", "enum": ["low", "medium", "high"]}
                        },
                        "required": ["symbol", "action", "sizing", "rationale", "confidence"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["report_md", "recommendations"],
            "additionalProperties": false
        }
    })
}

/// Compact context block injected into the user turn (volatile data lives
/// here, after the cached prefix).
async fn context_block(state: &AppState) -> String {
    let snapshot = crate::live::portfolio_snapshot(state).await.ok();
    let portfolio = snapshot
        .as_ref()
        .map(|s| json!({"portfolio": s["portfolio"], "watchlist": s["watchlist"]}).to_string())
        .unwrap_or_else(|| "portfolio unavailable".into());
    format!(
        "Today is {}.\n\nCurrent state:\n{portfolio}",
        Utc::now().format("%Y-%m-%d")
    )
}

/// Run one analysis end to end and persist the outcome. Spawned in the
/// background; all failures land in the analyses row, never silently.
pub async fn run_analysis(state: AppState, id: i64, kind: String, question: Option<String>) {
    match run_analysis_inner(&state, &kind, question.as_deref()).await {
        Ok((outcome, model)) => {
            let tool_log = serde_json::to_string(&outcome.tool_log).ok();
            let cost = pricing::cost_usd(model, &outcome.usage);
            // The weekly review answers in structured JSON; split it into
            // the report + persisted recommendations. A parse failure is an
            // honest error (with the tokens we PAID for still accounted),
            // never a silently-mangled report.
            let (report, error): (String, Option<String>) = if kind == "weekly_review" {
                match serde_json::from_str::<Value>(&outcome.text) {
                    Ok(parsed) => {
                        store_recommendations(&state, id, &parsed).await;
                        (
                            parsed["report_md"]
                                .as_str()
                                .unwrap_or(&outcome.text)
                                .to_string(),
                            None,
                        )
                    }
                    Err(e) => (
                        outcome.text.clone(), // keep the raw output for debugging
                        Some(format!("structured output did not parse: {e}")),
                    ),
                }
            } else {
                (outcome.text.clone(), None)
            };
            let row = repo::AnalysisOutcome {
                input_tokens: outcome.usage.input_tokens as i64,
                output_tokens: outcome.usage.output_tokens as i64,
                cache_read_tokens: outcome.usage.cache_read_tokens as i64,
                cache_write_tokens: outcome.usage.cache_write_tokens as i64,
                cost_usd: cost,
                report_md: Some(&report),
                tool_log_json: tool_log.as_deref(),
                error: error.as_deref(),
            };
            if let Err(e) = repo::finish_analysis(&state.db, id, row).await {
                tracing::error!(analysis = id, error = %e, "failed to persist analysis");
            }
            match error {
                Some(message) => tracing::error!(analysis = id, kind, %message, "analysis errored"),
                None => tracing::info!(analysis = id, kind, "analysis complete"),
            }
        }
        Err(message) => {
            tracing::error!(analysis = id, kind, %message, "analysis failed");
            let row = repo::AnalysisOutcome {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: Decimal::ZERO,
                report_md: None,
                tool_log_json: None,
                error: Some(&message),
            };
            if let Err(e) = repo::finish_analysis(&state.db, id, row).await {
                tracing::error!(analysis = id, error = %e, "failed to persist analysis error");
            }
        }
    }
    state.analyst_running.store(false, Ordering::Relaxed);
}

async fn run_analysis_inner(
    state: &AppState,
    kind: &str,
    question: Option<&str>,
) -> Result<(rekt_analyst::runner::RunOutcome, &'static str), String> {
    let Some(transport) = &state.analyst else {
        return Err("no ANTHROPIC_API_KEY configured".into());
    };
    let tools = ServerTools {
        state: state.clone(),
    };
    let context = context_block(state).await;

    let (model, user_content, server_tools, output_schema, max_tokens) = match kind {
        "briefing" => (
            BRIEFING_MODEL,
            format!(
                "{context}\n\nWrite this morning's briefing: 1) portfolio state in two sentences, \
                 2) the signals that deserve attention today, 3) anything to watch. \
                 Under 250 words."
            ),
            vec![],
            None,
            2048u32,
        ),
        "weekly_review" => (
            ANALYST_MODEL,
            format!(
                "{context}\n\nDo the weekly deep review: performance vs the SPY benchmark, \
                 position-by-position assessment using signals and recent price action, \
                 concentration and cash posture, and what changed this week in the market that \
                 affects these holdings (use web_search). End with concrete recommendations."
            ),
            vec![json!({"type": "web_search_20260209", "name": "web_search", "max_uses": 6})],
            Some(review_schema()),
            16000,
        ),
        _ => (
            ANALYST_MODEL,
            format!(
                "{context}\n\nQuestion from the user:\n{}",
                question.unwrap_or("Analyze my portfolio.")
            ),
            vec![json!({"type": "web_search_20260209", "name": "web_search", "max_uses": 4})],
            None,
            8000,
        ),
    };

    let outcome = run(
        transport.as_ref(),
        Some(&tools),
        RunConfig {
            model,
            max_tokens,
            system: SYSTEM_PROMPT,
            user_content,
            adaptive_thinking: model == ANALYST_MODEL,
            server_tools,
            output_schema,
            max_iterations: MAX_ITERATIONS,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok((outcome, model))
}

/// Persist parsed recommendations; invalid symbols are skipped loudly.
async fn store_recommendations(state: &AppState, analysis_id: i64, parsed: &Value) {
    let Some(recommendations) = parsed["recommendations"].as_array() else {
        return;
    };
    let expires = Utc::now() + Duration::days(RECOMMENDATION_TTL_DAYS);
    for recommendation in recommendations {
        let raw_symbol = recommendation["symbol"].as_str().unwrap_or("");
        let Ok(symbol) = validate_symbol(raw_symbol) else {
            tracing::warn!(
                symbol = raw_symbol,
                "skipping recommendation with invalid symbol"
            );
            continue;
        };
        let action = recommendation["action"].as_str().unwrap_or("watch");
        let result = repo::insert_recommendation(
            &state.db,
            repo::NewRecommendation {
                analysis_id,
                symbol: &symbol,
                action,
                sizing: recommendation["sizing"].as_str().unwrap_or(""),
                rationale: recommendation["rationale"].as_str().unwrap_or(""),
                confidence: recommendation["confidence"].as_str().unwrap_or(""),
                expires_ts: expires,
            },
        )
        .await;
        if let Err(e) = result {
            tracing::error!(error = %e, symbol, "failed to store recommendation");
        }
    }
}

/// Gate shared by every run path: key present, nothing already running,
/// today's spend under budget. Returns the analysis id on success.
async fn start_run(
    state: &AppState,
    kind: &str,
    question: Option<String>,
) -> Result<i64, ApiError> {
    if state.analyst.is_none() {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "AI analyst disabled — set ANTHROPIC_API_KEY",
        ));
    }
    let today = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc();
    let spent = repo::analyses_cost_since(&state.db, today)
        .await
        .map_err(internal)?;
    if spent >= state.ai_budget {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "daily AI budget reached (${spent} of ${} — raise REKT_AI_DAILY_BUDGET)",
                state.ai_budget
            ),
        ));
    }
    // One analysis at a time: they share the price cache and the budget.
    if state
        .analyst_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(err(
            StatusCode::CONFLICT,
            "an analysis is already running — wait for it to finish",
        ));
    }

    let model = if kind == "briefing" {
        BRIEFING_MODEL
    } else {
        ANALYST_MODEL
    };
    let id = match repo::insert_analysis(&state.db, kind, model, question.as_deref()).await {
        Ok(id) => id,
        Err(e) => {
            state.analyst_running.store(false, Ordering::Relaxed);
            return Err(internal(e));
        }
    };
    let job_state = state.clone();
    let job_kind = kind.to_string();
    tokio::spawn(async move {
        run_analysis(job_state, id, job_kind, question).await;
    });
    Ok(id)
}

pub async fn run_briefing(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let id = start_run(&state, "briefing", None).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"id": id}))))
}

pub async fn run_review(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let id = start_run(&state, "weekly_review", None).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"id": id}))))
}

#[derive(Debug, serde::Deserialize)]
pub struct AskInput {
    pub question: String,
}

pub async fn ask(
    State(state): State<AppState>,
    Json(input): Json<AskInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let question = input.question.trim().to_string();
    if question.is_empty() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "question is required",
        ));
    }
    let id = start_run(&state, "on_demand", Some(question)).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"id": id}))))
}

/// GET /api/analyst — one payload for the dashboard section: enablement,
/// budget state, recent analyses and recommendations.
pub async fn summary(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let today = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc();
    let spent = repo::analyses_cost_since(&state.db, today)
        .await
        .map_err(internal)?;
    let analyses = repo::recent_analyses(&state.db, 8)
        .await
        .map_err(internal)?;
    let recommendations = repo::list_recommendations(&state.db, 20)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "enabled": state.analyst.is_some(),
        "running": state.analyst_running.load(Ordering::Relaxed),
        "today_cost_usd": spent,
        "budget_usd": state.ai_budget,
        "analyses": analyses,
        "recommendations": recommendations,
    })))
}

pub async fn get_analysis(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<repo::AnalysisRecord>, ApiError> {
    match repo::get_analysis(&state.db, id).await.map_err(internal)? {
        Some(analysis) => Ok(Json(analysis)),
        None => Err(err(StatusCode::NOT_FOUND, format!("no analysis {id}"))),
    }
}

/// POST /api/recommendations/{id}/accept — marks it accepted. Execution is
/// the HUMAN's: the UI prefills the normal order ticket, nothing more.
pub async fn accept_recommendation(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if !repo::set_recommendation_status(&state.db, id, "accepted")
        .await
        .map_err(internal)?
    {
        return Err(err(
            StatusCode::CONFLICT,
            format!("recommendation {id} is not open"),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn dismiss_recommendation(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if !repo::set_recommendation_status(&state.db, id, "dismissed")
        .await
        .map_err(internal)?
    {
        return Err(err(
            StatusCode::CONFLICT,
            format!("recommendation {id} is not open"),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Scheduler hook: morning briefing on NY weekdays after 08:00, weekly deep
/// review on Saturdays after 09:00 — each at most once per period, budget
/// and single-flight rules identical to manual runs.
pub async fn maybe_scheduled_runs(state: &AppState) -> anyhow::Result<()> {
    use chrono::{Datelike, Timelike, Weekday};
    use chrono_tz::America::New_York;

    if state.analyst.is_none() {
        return Ok(());
    }
    let now_ny = Utc::now().with_timezone(&New_York);
    let today_start = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc();

    let is_weekday = !matches!(now_ny.weekday(), Weekday::Sat | Weekday::Sun);
    if is_weekday
        && now_ny.hour() >= 8
        && !repo::analysis_ran_since(&state.db, "briefing", today_start).await?
        && start_run(state, "briefing", None).await.is_ok()
    {
        tracing::info!("scheduled morning briefing started");
        return Ok(()); // one scheduled run per tick — they serialize anyway
    }
    if now_ny.weekday() == Weekday::Sat
        && now_ny.hour() >= 9
        && !repo::analysis_ran_since(&state.db, "weekly_review", Utc::now() - Duration::days(6))
            .await?
        && start_run(state, "weekly_review", None).await.is_ok()
    {
        tracing::info!("scheduled weekly review started");
    }
    Ok(())
}
