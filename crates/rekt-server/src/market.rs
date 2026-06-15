//! Market gauges + brief: a small fixed set of index ETFs for state-of-market
//! context, scored with the SAME deterministic signals as the screener and
//! narrated by the analyst. The indices join the fetch sets (history /
//! refresh_symbols) so candles + signals exist. Honest degradation: a gauge
//! whose history hasn't backfilled yet reports null signals, never guesses.

use axum::{extract::State, response::Json};
use rekt_core::signals::summarize;
use rust_decimal::Decimal;
use serde_json::{json, Value};

use crate::api::{internal, ApiError};
use crate::live::SIGNAL_WINDOW_BARS;
use crate::{repo, AppState};

/// The gauges, broad → narrow. SPY is also the portfolio benchmark.
pub const INDICES: &[(&str, &str)] = &[
    ("SPY", "S&P 500"),
    ("QQQ", "Nasdaq 100"),
    ("IWM", "Russell 2000"),
    ("DIA", "Dow 30"),
];

/// Min daily closes for the gauge's signals to be meaningful.
const MIN_BARS: usize = 30;

pub fn index_symbols() -> Vec<String> {
    INDICES.iter().map(|(s, _)| s.to_string()).collect()
}

/// GET /api/market — the index gauges with price / day% / signals. Pure.
pub async fn gauges(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let prices = state.live.price_views().await;
    let mut out = Vec::new();
    for (symbol, name) in INDICES {
        let closes = repo::recent_closes(&state.db, symbol, SIGNAL_WINDOW_BARS)
            .await
            .map_err(internal)?;
        let pv = prices.get(*symbol);
        // Live price if known, else the last candle close.
        let price = pv.map(|p| p.price).or_else(|| closes.last().copied());
        let prev = pv
            .and_then(|p| p.prev_close)
            .or_else(|| (closes.len() >= 2).then(|| closes[closes.len() - 2]));
        let day_pct = match (price, prev) {
            (Some(p), Some(pr)) if pr > Decimal::ZERO => {
                Some(((p - pr) / pr * Decimal::from(100)).round_dp(2))
            }
            _ => None,
        };
        let signals = (closes.len() >= MIN_BARS).then(|| summarize(&closes));
        tracing::debug!(
            symbol,
            closes = closes.len(),
            has_signals = signals.is_some(),
            "gauge"
        );
        out.push(json!({
            "symbol": symbol,
            "name": name,
            "price": price,
            "day_pct": day_pct,
            "signals": signals,
        }));
    }
    // The latest finished market brief, so the UI shows the AI read inline.
    let brief = repo::latest_market_brief(&state.db)
        .await
        .map_err(internal)?
        .map(|(report_md, ts)| json!({ "report_md": report_md, "ts": ts }));
    tracing::debug!(
        gauges = out.len(),
        brief = brief.is_some(),
        "GET /api/market"
    );
    Ok(Json(json!({ "gauges": out, "brief": brief })))
}

/// POST /api/market/brief — kick off an AI state-of-market brief over the
/// gauges (advisory context, no recommendations). Bills nothing on disabled.
pub async fn brief(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    crate::demo_guard(&state)?;
    let id = crate::analyst::start_run(&state, "market_brief", None).await?;
    tracing::debug!(id, "POST /api/market/brief started");
    Ok(Json(json!({ "started": true, "id": id })))
}

/// A compact, signal-grounded gauges block injected into the analyst prompt —
/// the FACTS the brief narrates (it must not invent levels).
pub async fn gauges_prompt_block(state: &AppState) -> String {
    let prices = state.live.price_views().await;
    let opt = |d: Option<Decimal>| d.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    let mut out = String::from(
        "Current market gauges (index ETFs; RSI/SMA distance/drawdown are \
         deterministic facts computed locally):\n",
    );
    let mut rows = 0;
    for (symbol, name) in INDICES {
        let closes = repo::recent_closes(&state.db, symbol, SIGNAL_WINDOW_BARS)
            .await
            .unwrap_or_default();
        if closes.len() < MIN_BARS {
            continue;
        }
        rows += 1;
        let s = summarize(&closes);
        let price = prices
            .get(*symbol)
            .map(|p| p.price)
            .or_else(|| closes.last().copied());
        out.push_str(&format!(
            "- {} ({}): last {}, RSI {}, vs 50d {}%, vs 200d {}%, drawdown {}%\n",
            symbol,
            name,
            opt(price),
            opt(s.rsi14),
            opt(s.vs_sma50_pct),
            opt(s.vs_sma200_pct),
            opt(s.drawdown_pct),
        ));
    }
    // Honest degradation on a fresh install: if no index has enough history yet,
    // tell the model to decline rather than improvise levels it cannot see.
    if rows == 0 {
        out.push_str(
            "No index gauges have enough history yet — say the market read isn't \
             available rather than guessing.\n",
        );
    }
    tracing::debug!(rows, "gauges_prompt_block");
    out
}
