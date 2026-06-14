//! Phase 5: the AI analyst (PLAN.md §5 layer 2) — morning briefings, weekly
//! deep reviews, and on-demand analysis over the Claude API.
//!
//! Safety posture: the analyst is ADVISORY ONLY. Its tool surface is
//! read-only (portfolio, candles, quotes, performance), `rekt-analyst` has
//! no path to `rekt-broker`, and recommendations only prefill an order
//! ticket the human confirms through /api/orders with every guardrail.
//! Every run is cost-metered and a daily budget gates new runs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::America::New_York;
use rekt_analyst::runner::{run, RunConfig, RunOutcome};
use rekt_analyst::{pricing, ToolExecutor, UsageTotals, ANALYST_MODEL, BRIEFING_MODEL};
use rust_decimal::Decimal;
use serde_json::{json, Value};

use crate::api::{err, internal, validate_symbol, ApiError};
use crate::live::SIGNAL_WINDOW_BARS;
use crate::{repo, AppState};

/// Recommendations lapse after a week — stale advice must expire honestly.
const RECOMMENDATION_TTL_DAYS: i64 = 7;
/// One window for BOTH hit-rate computations (the model's prompt and the
/// UI header) — the feedback loop breaks if they quietly diverge.
const TRACK_RECORD_RECS: i64 = 20;
/// Generous cap on loop iterations (each is one API call).
const MAX_ITERATIONS: usize = 12;

/// Midnight of the CURRENT New York calendar day, as UTC. Both the budget
/// day and the scheduler's once-per-day guards use this — mixing UTC days
/// with NY fire conditions would re-open the window every evening.
fn ny_day_start() -> DateTime<Utc> {
    Utc::now()
        .with_timezone(&New_York)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_local_timezone(New_York)
        .single()
        .expect("NY midnight is never ambiguous (US DST shifts at 2am)")
        .to_utc()
}

/// Releases the single-flight latch on success, error, AND panic — a
/// detached task's panic is swallowed by tokio, so only a destructor can
/// guarantee the analyst doesn't wedge until restart.
struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

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
                // The inconsistent-log payload is Ok(type:"error") — passing
                // its missing keys through as nulls would hand the model a
                // phantom empty portfolio. Fail the tool call honestly.
                if snapshot["type"] == "error" {
                    return Err(format!(
                        "portfolio unavailable: {}",
                        snapshot["error"]
                            .as_str()
                            .unwrap_or("inconsistent transaction log")
                    ));
                }
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

/// In-prompt instruction that makes the CLI backend emit the same
/// `{report_md, recommendations[]}` shape the HTTP backend gets from the
/// API's output schema, so `claude -p` weekly reviews still populate the
/// recommendation-outcome tracker. Mirrors [`review_schema`] — keep in sync.
fn cli_review_json_instruction() -> &'static str {
    "You have no web access for this review — reason from the portfolio state \
     above and your own knowledge; do not claim to have looked anything up.\n\n\
     Respond with ONLY a single JSON object — no prose before or after it, no \
     markdown code fences — of exactly this shape:\n\
     {\n  \
       \"report_md\": \"<the full review, markdown>\",\n  \
       \"recommendations\": [\n    \
         {\n      \
           \"symbol\": \"TICKER\",\n      \
           \"action\": \"buy|sell|trim|hold|watch\",\n      \
           \"sizing\": \"plain words, e.g. '2% of equity' or '5 shares'\",\n      \
           \"rationale\": \"why\",\n      \
           \"confidence\": \"low|medium|high\"\n    \
         }\n  \
       ]\n\
     }\n\
     The recommendations array may be empty if you have no concrete calls."
}

/// Extract a JSON object from model output. The HTTP backend's strict output
/// schema returns clean JSON (the trivial case), but the CLI backend returns
/// free text that may wrap the object in ```json fences or surround it with
/// prose. Returns `None` only when no parseable object is present — an honest
/// signal to the caller.
fn extract_json_object(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    // Try to read the first complete JSON value starting at each '{'. A naive
    // first-'{'..last-'}' slice breaks when prose contains a stray brace (e.g.
    // "(note: {AAPL} concentration): {\"report_md\":…}") — it would splice the
    // text between the braces and fail to parse, silently dropping the whole
    // result. StreamDeserializer reads one value and tolerates trailing prose,
    // so we skip non-JSON '{'s and take the first that yields an object.
    for (start, _) in trimmed.match_indices('{') {
        let mut stream = serde_json::Deserializer::from_str(&trimmed[start..]).into_iter::<Value>();
        if let Some(Ok(v @ Value::Object(_))) = stream.next() {
            return Some(v);
        }
    }
    None
}

/// "Today is …" line for every user turn (volatile data lives after the
/// cached prefix, never in the system prompt).
fn date_line() -> String {
    format!(
        "Today is {}.",
        Utc::now().with_timezone(&New_York).format("%Y-%m-%d")
    )
}

/// Outcomes for a batch of recommendations, keyed by recommendation id.
/// Derived from cached closes — a symbol with no candle on/after the
/// recommendation date simply has no entry yet (backfill will catch up).
async fn recommendation_outcomes(
    state: &AppState,
    recommendations: &[repo::RecommendationRecord],
) -> anyhow::Result<std::collections::HashMap<i64, rekt_core::outcomes::RecOutcome>> {
    let mut symbols: Vec<String> = recommendations.iter().map(|r| r.symbol.clone()).collect();
    symbols.sort();
    symbols.dedup();
    // Bound the fetch to the oldest listed recommendation: this helper is
    // on the UI's 3-second poll path during runs, and outcomes never look
    // at closes before a rec date.
    let oldest = recommendations
        .iter()
        .filter_map(|rec| DateTime::parse_from_rfc3339(&rec.created_ts).ok())
        .map(|created| rekt_core::taxes::ny_date(created.to_utc()))
        .min();
    let closes = repo::closes_map_since(&state.db, &symbols, oldest).await?;
    Ok(recommendations
        .iter()
        .filter_map(|rec| {
            let created = DateTime::parse_from_rfc3339(&rec.created_ts).ok()?;
            let rec_date = rekt_core::taxes::ny_date(created.to_utc());
            let outcome = rekt_core::outcomes::recommendation_outcome(
                &rec.action,
                rec_date,
                closes.get(&rec.symbol)?,
            )?;
            Some((rec.id, outcome))
        })
        .collect())
}

/// Compact track record for analyst prompts: the model sees how its
/// previous calls aged before making new ones. Failures degrade to an
/// honest one-liner — a broken track record must not block an analysis.
async fn track_record(state: &AppState) -> String {
    let recommendations = match repo::list_recommendations(&state.db, TRACK_RECORD_RECS).await {
        Ok(recommendations) if !recommendations.is_empty() => recommendations,
        Ok(_) => return "No prior recommendations on record.".to_string(),
        Err(e) => return format!("Track record unavailable: {e}"),
    };
    let outcomes = match recommendation_outcomes(state, &recommendations).await {
        Ok(outcomes) => outcomes,
        Err(e) => return format!("Track record unavailable: {e}"),
    };
    let mut lines = vec!["Your recent recommendations and how they aged:".to_string()];
    let (mut hits, mut tested) = (0u32, 0u32);
    for rec in &recommendations {
        let verdict = match outcomes.get(&rec.id) {
            Some(o) => {
                let direction = match o.favorable {
                    Some(true) => {
                        hits += 1;
                        tested += 1;
                        " — favorable"
                    }
                    Some(false) => {
                        tested += 1;
                        " — unfavorable"
                    }
                    None => " — no testable direction",
                };
                // Strictly positive only: "+0%" next to "unfavorable"
                // would hand the model a contradictory signal.
                let sign = if o.return_pct > Decimal::ZERO {
                    "+"
                } else {
                    ""
                };
                format!("{sign}{}% since{direction}", o.return_pct)
            }
            None => "no price data yet".to_string(),
        };
        lines.push(format!(
            "- {} {} {} ({}): {verdict}",
            &rec.created_ts[..10.min(rec.created_ts.len())],
            rec.action,
            rec.symbol,
            rec.status,
        ));
    }
    if tested > 0 {
        lines.push(format!(
            "Hit rate: {hits} of {tested} directional calls favorable."
        ));
    }
    lines.join("\n")
}

/// Full portfolio context injected into the BRIEFING user turn (the
/// briefing is the tool-less, single-call kind — review/ask fetch fresh
/// data through get_portfolio instead of carrying a pre-baked copy).
/// Compact, locally-computed performance + SPY benchmark figures so the
/// (tool-less CLI) review can speak to vs-benchmark and return instead of
/// claiming it lacks the data.
async fn performance_context(state: &AppState) -> String {
    let Ok(p) = crate::history::history_payload(state, "all").await else {
        return String::new();
    };
    let m = &p["metrics"];
    let s = |v: &serde_json::Value| v.as_str().map(str::to_string);
    let mut out = String::from(
        "Performance (computed locally from your transaction log — these are facts, not estimates):\n",
    );
    if let Some(twr) = s(&m["twr_pct"]) {
        out.push_str(&format!("- Time-weighted return, all-time: {twr}%\n"));
    }
    if let Some(irr) = s(&m["irr_pct"]) {
        out.push_str(&format!(
            "- Money-weighted return (IRR, annualized): {irr}%\n"
        ));
    }
    match s(&m["benchmark_twr_pct"]) {
        Some(b) => out.push_str(&format!(
            "- Cash-flow-matched SPY benchmark TWR over the same period: {b}%. The SPY series IS \
             available to you here — give a concrete vs-SPY read; do NOT say you lack benchmark data.\n"
        )),
        None => out.push_str(
            "- SPY benchmark: insufficient overlapping history for a clean figure this period.\n",
        ),
    }
    out
}

async fn portfolio_context(state: &AppState) -> String {
    match crate::live::portfolio_snapshot(state).await {
        // Honesty over nulls: the inconsistent-log payload must read as an
        // error, not as an empty portfolio.
        Ok(snapshot) if snapshot["type"] == "error" => format!(
            "Portfolio unavailable: {}",
            snapshot["error"]
                .as_str()
                .unwrap_or("inconsistent transaction log")
        ),
        Ok(snapshot) => json!({
            "portfolio": snapshot["portfolio"],
            "watchlist": snapshot["watchlist"],
        })
        .to_string(),
        Err(e) => format!("Portfolio unavailable: {e}"),
    }
}

/// Run one analysis end to end and persist the outcome. Spawned in the
/// background; all failures land in the analyses row, never silently.
pub async fn run_analysis(state: AppState, id: i64, kind: String, question: Option<String>) {
    // Drop guard, not a trailing store: tokio swallows panics in detached
    // tasks, and a wedged latch would 409 every future run until restart.
    let _running = RunningGuard(state.analyst_running.clone());

    match run_analysis_inner(&state, &kind, question.as_deref()).await {
        Ok((outcome, model, allowlist)) => {
            let tool_log = serde_json::to_string(&outcome.tool_log).ok();
            // Ollama runs locally and bills nothing.
            let cost = if state.analyst_backend == "ollama" {
                Decimal::ZERO
            } else {
                pricing::cost_usd(model, &outcome.usage)
            };
            // The weekly review answers in structured JSON; split it into
            // the report + persisted recommendations. Both backends produce
            // the same shape — the HTTP one via the API's output schema, the
            // CLI one via the in-prompt instruction (`extract_json_object`
            // tolerates fences/prose the CLI may add). A parse failure is an
            // honest error (with the tokens we PAID for still accounted),
            // never a silently-mangled report.
            let (report, error): (String, Option<String>) = if kind == "weekly_review"
                || kind == "market_ideas"
            {
                match extract_json_object(&outcome.text) {
                    Some(parsed) => {
                        // market_ideas is GROUNDED: persist only recs the
                        // screener actually surfaced, with the screen's
                        // action — the AI can't add or flip names.
                        if kind == "market_ideas" {
                            store_grounded_recommendations(&state, id, &parsed, &allowlist).await;
                        } else {
                            store_recommendations(&state, id, &parsed).await;
                        }
                        (
                            parsed["report_md"]
                                .as_str()
                                .unwrap_or(&outcome.text)
                                .to_string(),
                            None,
                        )
                    }
                    None => (
                        outcome.text.clone(), // keep the raw output for debugging
                        Some("structured output did not contain a JSON object".to_string()),
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
        Err(failure) => {
            tracing::error!(analysis = id, kind, message = %failure.message, "analysis failed");
            // The tokens a FAILED run billed must still count against the
            // budget — zeros here would let failures spend invisibly.
            let tool_log = serde_json::to_string(&failure.tool_log).ok();
            let row = repo::AnalysisOutcome {
                input_tokens: failure.usage.input_tokens as i64,
                output_tokens: failure.usage.output_tokens as i64,
                cache_read_tokens: failure.usage.cache_read_tokens as i64,
                cache_write_tokens: failure.usage.cache_write_tokens as i64,
                cost_usd: if state.analyst_backend == "ollama" {
                    Decimal::ZERO
                } else {
                    pricing::cost_usd(failure.model, &failure.usage)
                },
                report_md: None,
                tool_log_json: tool_log.as_deref(),
                error: Some(&failure.message),
            };
            if let Err(e) = repo::finish_analysis(&state.db, id, row).await {
                tracing::error!(analysis = id, error = %e, "failed to persist analysis error");
            }
        }
    }
}

/// Everything a failed run still owes the books.
struct RunFailureDetails {
    message: String,
    usage: UsageTotals,
    tool_log: Vec<Value>,
    model: &'static str,
}

async fn run_analysis_inner(
    state: &AppState,
    kind: &str,
    question: Option<&str>,
) -> Result<(RunOutcome, &'static str, Vec<(String, &'static str)>), RunFailureDetails> {
    // Kind split: the BRIEFING is the inject-and-answer kind — portfolio
    // context in the user turn, NO tools, one cheap Haiku call. Review/ask
    // get the tool surface instead and fetch fresh data exactly once.
    let (model, max_tokens) = kind_params(kind);
    let fail = |message: String| RunFailureDetails {
        message,
        usage: UsageTotals::default(),
        tool_log: Vec::new(),
        model,
    };
    let Some(transport) = &state.analyst else {
        return Err(fail("no ANTHROPIC_API_KEY configured".into()));
    };
    let date = date_line();
    // Past calls and how they aged — volatile data, so it lives in the
    // user turn (after the cached prefix), never in the system prompt.
    let track = track_record(state).await;

    // market_ideas screens deterministically UP FRONT: screen errors stay
    // honest (not swallowed into an empty prompt that still bills), and the
    // SAME candidate set drives both the prompt and the post-hoc grounding
    // filter (so the AI can't add or flip names — the screen is authoritative).
    let market_screen = if kind == "market_ideas" {
        let list_id: i64 = question
            .and_then(|q| q.parse().ok())
            .ok_or_else(|| fail("market_ideas run missing a valid list id".into()))?;
        let lists = repo::list_watchlists(&state.db)
            .await
            .map_err(|e| fail(format!("watchlist lookup failed: {e}")))?;
        let list_name = lists
            .iter()
            .find(|l| l.id == list_id)
            .map(|l| l.name.clone())
            .ok_or_else(|| fail(format!("no watchlist {list_id}")))?;
        let (scored, _, _) = crate::screener::scored_candidates(state, list_id)
            .await
            .map_err(|e| fail(format!("screen failed: {e}")))?;
        if scored.is_empty() {
            return Err(fail(
                "no screened candidates at run time (the screen may have changed)".into(),
            ));
        }
        Some((list_name, scored))
    } else {
        None
    };

    let (mut user_content, mut use_tools, mut server_tools, mut output_schema) = match kind {
        "briefing" => (
            format!(
                "{date}\n\nCurrent state:\n{}\n\n{track}\n\nWrite this morning's briefing as short \
                 markdown, under 250 words, with these sections in order:\n\n\
                 ## TL;DR\nONE sentence: the single most important thing right now AND the one \
                 action to take today (or 'Hold — nothing to do'). It must stand on its own and \
                 must NOT merely restate the portfolio-state numbers below — lead with the \
                 takeaway and the decision, not the balance.\n\n\
                 ## Portfolio\nState in two sentences.\n\n\
                 ## Signals\nThe signals that deserve attention today, as tight bullets.\n\n\
                 ## Watch\nAnything to watch.",
                portfolio_context(state).await
            ),
            false,
            vec![],
            None,
        ),
        "weekly_review" => (
            format!(
                "{date}\n\n{track}\n\nWrite this week's portfolio review as a SCANNABLE markdown \
                 brief — short paragraphs and tight bullets, never essays. Lead with what to DO. \
                 Your per-name buy/sell/trim/hold calls go in the structured `recommendations` \
                 you return separately, so do NOT re-narrate every holding in prose here. Use \
                 these exact section headings, in this order:\n\n\
                 ## Bottom line\nThe single most important fact and the one decision that matters \
                 most (2–3 sentences).\n\n\
                 ## Do now\nA short bullet list of concrete actions, highest-leverage first — or \
                 'Nothing to trade — hold the book.'\n\n\
                 ## Performance\nAccount value, total return, and your return VS the SPY benchmark \
                 as a number (benchmark figures are provided below — use them). Call out cash drag \
                 if it is material.\n\n\
                 ## Posture & risk\nCash deployment and concentration as tight bullets; name the \
                 single biggest structural risk.\n\n\
                 ## Watch next\nForward-looking only: catalysts, price levels, or signals to watch \
                 this week (a short list). Use web_search for anything time-sensitive.\n\n\
                 Weigh your track record above — revisit calls that aged badly before repeating \
                 them. Keep the whole brief tight and concrete."
            ),
            true,
            vec![json!({"type": "web_search_20260209", "name": "web_search", "max_uses": 6})],
            Some(review_schema()),
        ),
        "market_brief" => {
            // State-of-the-market context over deterministic index gauges —
            // narration, not portfolio advice; no recommendations.
            let block = crate::market::gauges_prompt_block(state).await;
            (
                format!(
                    "{date}\n\n{block}\n\nWrite a SHORT state-of-the-market read (3-5 sentences): \
                     the overall posture (risk-on / risk-off / mixed), what the indices' trend and \
                     RSI say across broad (SPY), tech (QQQ) and small caps (IWM), and the single \
                     thing to watch next. Ground it in the gauge facts above; plain markdown. This \
                     is market CONTEXT — do not give portfolio or per-name trade advice.",
                ),
                false,
                vec![],
                None,
            )
        }
        "market_ideas" => {
            // Candidates were screened up front (see market_screen); the AI
            // only narrates them.
            let (list_name, scored) = market_screen
                .as_ref()
                .expect("market_screen is Some for market_ideas");
            let block = crate::screener::candidates_prompt_block(list_name, scored);
            (
                format!(
                    "{date}\n\n{track}\n\n{block}\n\nFor EACH candidate above, return a \
                     recommendation whose `action` matches the screen and whose `rationale` is a \
                     1-2 sentence thesis grounding the call in the listed signals plus your own \
                     knowledge of the name. Keep `report_md` to a 2-3 sentence overview of the \
                     batch. Do not add or drop names; do not restate the raw signal lines.",
                ),
                false,
                vec![],
                Some(review_schema()),
            )
        }
        _ => (
            format!(
                "{date}\n\n{track}\n\nQuestion from the user:\n{}",
                question.unwrap_or("Analyze my portfolio.")
            ),
            true,
            vec![json!({"type": "web_search_20260209", "name": "web_search", "max_uses": 4})],
            None,
        ),
    };

    // CLI backend (`claude -p`) drives no in-process tool loop and is
    // launched with no tools, so every kind runs tool-lessly: strip tools
    // and the structured schema, and inject the portfolio snapshot that the
    // tool-using kinds would otherwise have fetched.
    if state.analyst_cli {
        use_tools = false;
        server_tools = vec![];
        output_schema = None;
        // briefing + market_brief carry their own context in the prompt; the
        // market brief is deliberately NOT about the user's holdings.
        if kind != "briefing" && kind != "market_brief" {
            user_content = format!(
                "{user_content}\n\nCurrent portfolio state:\n{}\n\n{}",
                portfolio_context(state).await,
                performance_context(state).await
            );
        }
        // The HTTP backend gets structured recommendations from the API's
        // output schema; the CLI has no such knob, so we ask for the same
        // shape in-prompt and parse it back out (see `extract_json_object`).
        // It also has no web access, so say so rather than have it pretend.
        if kind == "weekly_review" || kind == "market_ideas" {
            user_content = format!("{user_content}\n\n{}", cli_review_json_instruction());
        }
    }

    let tools = ServerTools {
        state: state.clone(),
    };
    let outcome = run(
        transport.as_ref(),
        use_tools.then_some(&tools as &dyn ToolExecutor),
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
    .map_err(|failure| RunFailureDetails {
        message: failure.error.to_string(),
        usage: failure.usage,
        tool_log: failure.tool_log,
        model,
    })?;
    // The screened (symbol, action) set — the ground truth the persisted
    // market_ideas recs are filtered against (empty for other kinds).
    let allowlist: Vec<(String, &'static str)> = market_screen
        .map(|(_, scored)| {
            scored
                .into_iter()
                .map(|s| (s.symbol, s.candidate.action))
                .collect()
        })
        .unwrap_or_default();
    Ok((outcome, model, allowlist))
}

/// Model + max_tokens per kind — shared by the run path and the budget
/// gate's worst-case estimate.
fn kind_params(kind: &str) -> (&'static str, u32) {
    match kind {
        "briefing" => (BRIEFING_MODEL, 2048),
        "weekly_review" => (ANALYST_MODEL, 16000),
        // Narrating pre-screened candidates is light work — the deterministic
        // screener already did the hard part.
        "market_ideas" => (ANALYST_MODEL, 6000),
        // A short state-of-market read over the injected gauges — cheap model.
        "market_brief" => (BRIEFING_MODEL, 1024),
        _ => (ANALYST_MODEL, 8000),
    }
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

/// Persist market-ideas recommendations GROUNDED against the screen: a rec for
/// a symbol the screener didn't surface is dropped, and the action is forced to
/// the screen's (not the AI's) — so a hallucinated ticker or a flipped
/// buy/sell can never become a tracked, stage-able recommendation.
async fn store_grounded_recommendations(
    state: &AppState,
    analysis_id: i64,
    parsed: &Value,
    allowlist: &[(String, &'static str)],
) {
    let Some(recommendations) = parsed["recommendations"].as_array() else {
        return;
    };
    let expires = Utc::now() + Duration::days(RECOMMENDATION_TTL_DAYS);
    for recommendation in recommendations {
        let raw_symbol = recommendation["symbol"].as_str().unwrap_or("");
        let Ok(symbol) = validate_symbol(raw_symbol) else {
            continue;
        };
        // The screen is authoritative: skip anything it didn't surface, and use
        // ITS action regardless of what the model returned.
        let Some((_, screen_action)) = allowlist.iter().find(|(s, _)| *s == symbol) else {
            tracing::warn!(symbol, "dropping market-idea rec not in the screen");
            continue;
        };
        let result = repo::insert_recommendation(
            &state.db,
            repo::NewRecommendation {
                analysis_id,
                symbol: &symbol,
                action: screen_action,
                sizing: recommendation["sizing"].as_str().unwrap_or(""),
                rationale: recommendation["rationale"].as_str().unwrap_or(""),
                confidence: recommendation["confidence"].as_str().unwrap_or(""),
                expires_ts: expires,
            },
        )
        .await;
        if let Err(e) = result {
            tracing::error!(error = %e, symbol, "failed to store grounded recommendation");
        }
    }
}

/// Gate shared by every run path: key present, nothing already running,
/// today's spend under budget. Returns the analysis id on success.
pub(crate) async fn start_run(
    state: &AppState,
    kind: &str,
    question: Option<String>,
) -> Result<i64, ApiError> {
    tracing::debug!(kind, "analyst run requested");
    if state.analyst.is_none() {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "AI analyst disabled — set ANTHROPIC_API_KEY",
        ));
    }
    let (model, max_tokens) = kind_params(kind);
    // Budget gate (NY day, like the scheduler): requires headroom for this
    // run's WORST-CASE output cost, so a run can't sail arbitrarily far past the
    // ceiling. SKIPPED for Ollama — it bills $0, and a $0-headroom gate would
    // still wrongly block on prior paid-backend spend.
    let spent = if state.analyst_backend == "ollama" {
        Decimal::ZERO
    } else {
        let spent = repo::analyses_cost_since(&state.db, ny_day_start())
            .await
            .map_err(internal)?;
        let headroom = pricing::max_output_cost(model, max_tokens);
        if spent + headroom > state.ai_budget {
            return Err(err(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "daily AI budget would be exceeded (${spent} spent of ${}, this run reserves \
                     ${headroom} — raise REKT_AI_DAILY_BUDGET)",
                    state.ai_budget
                ),
            ));
        }
        spent
    };
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

    let id = match repo::insert_analysis(&state.db, kind, model, question.as_deref()).await {
        Ok(id) => id,
        Err(e) => {
            state.analyst_running.store(false, Ordering::Relaxed);
            return Err(internal(e));
        }
    };
    tracing::debug!(
        analysis = id,
        kind,
        model,
        spent = %spent,
        budget = %state.ai_budget,
        "analyst run spawned"
    );
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
/// budget state, recent analyses (LIGHT — no report bodies; this endpoint
/// is polled every 3s during a run) plus the latest analysis in full.
pub async fn summary(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let spent = repo::analyses_cost_since(&state.db, ny_day_start())
        .await
        .map_err(internal)?;
    let analyses = repo::recent_analyses_light(&state.db, 8)
        .await
        .map_err(internal)?;
    // The highlighted "latest" speaks to the user's portfolio, so it skips
    // market_brief / market_ideas (those live in the MARKET / WATCH views).
    // The `analyses` history below still lists every kind.
    let latest = repo::latest_portfolio_analysis(&state.db)
        .await
        .map_err(internal)?;
    let recommendations = repo::list_recommendations(&state.db, TRACK_RECORD_RECS)
        .await
        .map_err(internal)?;
    let outcomes = recommendation_outcomes(&state, &recommendations)
        .await
        .map_err(internal)?;
    let (mut hits, mut tested) = (0u32, 0u32);
    for outcome in outcomes.values() {
        match outcome.favorable {
            Some(true) => {
                hits += 1;
                tested += 1;
            }
            Some(false) => tested += 1,
            None => {}
        }
    }
    let recommendations: Vec<Value> = recommendations
        .iter()
        .map(|rec| {
            let mut value = serde_json::to_value(rec).unwrap_or_default();
            value["outcome"] = outcomes
                .get(&rec.id)
                .map(|o| json!(o))
                .unwrap_or(Value::Null);
            value
        })
        .collect();
    // Which backend is live, so the UI can say so (None when disabled).
    let backend = state.analyst.as_ref().map(|_| state.analyst_backend);
    Ok(Json(json!({
        "enabled": state.analyst.is_some(),
        "backend": backend,
        "running": state.analyst_running.load(Ordering::Relaxed),
        "today_cost_usd": spent,
        "budget_usd": state.ai_budget,
        "latest": latest,
        "analyses": analyses,
        "recommendations": recommendations,
        "track_record": { "favorable": hits, "tested": tested },
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

    if state.analyst.is_none() {
        return Ok(());
    }
    let now_ny = Utc::now().with_timezone(&New_York);
    // NY midnight, NOT UTC midnight: the fire condition below is NY time,
    // so a UTC day boundary (~8pm ET) would re-open the once-per-day guard
    // every evening and fire duplicate paid briefings.
    let today_start = ny_day_start();

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

#[cfg(test)]
mod tests {
    use super::{cli_review_json_instruction, extract_json_object, review_schema};

    #[test]
    fn extracts_clean_json_object() {
        // The HTTP backend's strict schema returns exactly this — the trivial case.
        let v = extract_json_object(r#"{"report_md":"hi","recommendations":[]}"#).unwrap();
        assert_eq!(v["report_md"], "hi");
        assert!(v["recommendations"].is_array());
    }

    #[test]
    fn extracts_json_from_fences_and_prose() {
        // The CLI backend may wrap the object in a fenced block with chatter.
        let out = "Sure, here's the review:\n\n```json\n{\"report_md\":\"r\",\
                   \"recommendations\":[{\"symbol\":\"NVDA\",\"action\":\"hold\"}]}\n```\nHope that helps!";
        let v = extract_json_object(out).unwrap();
        assert_eq!(v["report_md"], "r");
        assert_eq!(v["recommendations"][0]["symbol"], "NVDA");
    }

    #[test]
    fn extracts_json_past_a_stray_brace_in_the_preamble() {
        // A stray '{' in the prose before the object must not break extraction
        // (the old first-'{'..last-'}' slice dropped the whole result here).
        let out = "Here's the review (note: {AAPL} concentration is high):\n\n\
                   {\"report_md\":\"r\",\"recommendations\":[]}\nThanks!";
        let v = extract_json_object(out).unwrap();
        assert_eq!(v["report_md"], "r");
    }

    #[test]
    fn no_object_present_is_none() {
        assert!(extract_json_object("just prose, no json here").is_none());
        assert!(extract_json_object("").is_none());
        // A bare JSON array is not the object shape we persist.
        assert!(extract_json_object("[1, 2, 3]").is_none());
    }

    #[test]
    fn cli_instruction_mentions_every_schema_field() {
        // Drift guard: the CLI backend asks for the recommendation shape in
        // prose, the HTTP backend via review_schema(). If the schema grows a
        // field, the prose must too — or CLI reviews silently lose data.
        let schema = review_schema();
        let item = &schema["schema"]["properties"]["recommendations"]["items"];
        let required = item["required"]
            .as_array()
            .expect("schema lists required fields");
        let instruction = cli_review_json_instruction();
        for field in required {
            let name = field.as_str().unwrap();
            assert!(
                instruction.contains(name),
                "cli_review_json_instruction() is missing schema field `{name}`"
            );
        }
        assert!(instruction.contains("report_md"));
    }
}
