//! Phase 3: candle backfill, the equity-curve endpoint, and EOD snapshots.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use chrono::{Duration, NaiveTime, Utc};
use chrono_tz::America::New_York;
use rekt_core::history::{equity_series, metrics_for};
use rekt_core::portfolio::compute_basis;

use crate::api::{err, internal, ApiError};
use crate::{repo, AppState};

/// The cash-flow-matched benchmark (PLAN.md §5 — "would I be less rekt if
/// I'd just bought SPY?").
pub const BENCHMARK: &str = "SPY";

/// Pull missing daily candles for every tracked symbol + the benchmark.
/// Idempotent; runs at startup and on the scheduler tick.
pub async fn backfill_candles(state: &AppState) -> anyhow::Result<()> {
    let Some(bars) = &state.bars else {
        return Ok(());
    };
    let Some(first) = repo::first_tx_date(&state.db).await? else {
        return Ok(()); // nothing to chart yet
    };
    let today = Utc::now().date_naive();

    let mut symbols = repo::all_symbols(&state.db).await?;
    symbols.extend(repo::watchlist_symbols(&state.db).await?);
    symbols.push(BENCHMARK.to_string());
    symbols.sort();
    symbols.dedup();

    let mut fetched_any = false;
    for symbol in symbols {
        // Resume after the last cached candle; a small overlap re-fetches
        // the most recent bar in case it was written intraday.
        let from = match repo::last_candle_date(&state.db, &symbol).await? {
            Some(last) => last - Duration::days(3),
            None => first - Duration::days(7),
        };
        if from >= today {
            continue;
        }
        match bars.daily_candles(&symbol, from, today).await {
            Ok(candles) if !candles.is_empty() => {
                let written = repo::upsert_candles(&state.db, &symbol, &candles).await?;
                if written > 0 {
                    fetched_any = true;
                    tracing::info!(symbol, bars = written, "candles backfilled");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(symbol, error = %e, "candle backfill failed"),
        }
    }
    if fetched_any {
        state.live.bump_candles_revision();
    }
    Ok(())
}

/// Write today's EOD snapshot once the market has closed (NY). Idempotent —
/// INSERT OR REPLACE keyed on (date, mode).
pub async fn maybe_snapshot_eod(state: &AppState) -> anyhow::Result<()> {
    let now_ny = Utc::now().with_timezone(&New_York);
    if now_ny.time() < NaiveTime::from_hms_opt(16, 5, 0).expect("valid time") {
        return Ok(());
    }
    let txs = repo::fetch_mode_txs(&state.db, "live").await?;
    let Ok(basis) = compute_basis(&txs) else {
        return Ok(()); // inconsistent log is surfaced elsewhere
    };
    let prices = state.live.price_views().await;
    let view = rekt_core::portfolio::value(&basis, &prices);
    repo::upsert_snapshot(
        &state.db,
        now_ny.date_naive(),
        "live",
        view.equity,
        view.cash,
        view.deposited - view.withdrawn,
        view.realized_pnl,
    )
    .await?;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub range: Option<String>,
}

/// GET /api/history?range=1m|3m|1y|all — the equity curve with benchmark
/// overlay and window-accurate TWR/IRR.
pub async fn history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let txs = repo::fetch_mode_txs(&state.db, "live")
        .await
        .map_err(internal)?;
    if txs.is_empty() {
        return Ok(Json(serde_json::json!({
            "points": [], "metrics": null,
            "note": "no transactions yet — the curve starts with your first one",
        })));
    }
    let mut symbols: Vec<String> = txs
        .iter()
        .filter_map(|t| t.symbol.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    symbols.push(BENCHMARK.to_string());
    let mut closes = repo::closes_map(&state.db, &symbols)
        .await
        .map_err(internal)?;
    let benchmark = closes.remove(BENCHMARK);

    let today = Utc::now().date_naive();
    let (points, _) = equity_series(&txs, &closes, benchmark.as_ref(), today);

    let days = match query.range.as_deref() {
        Some("1m") => Some(31),
        Some("3m") => Some(92),
        Some("1y") => Some(366),
        Some("all") | None => None,
        Some(other) => {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unknown range {other:?} (use 1m, 3m, 1y or all)"),
            ))
        }
    };
    let sliced: &[_] = match days {
        Some(days) if points.len() > days => &points[points.len() - days..],
        _ => &points,
    };
    let metrics = metrics_for(sliced);

    // Totals from the full log (range-independent context).
    let basis = compute_basis(&txs).map_err(|e| {
        err(
            StatusCode::CONFLICT,
            format!("transaction log inconsistent: {e}"),
        )
    })?;
    let view = rekt_core::portfolio::value(&basis, &state.live.price_views().await);

    Ok(Json(serde_json::json!({
        "points": sliced,
        "metrics": metrics,
        "totals": {
            "realized_pnl": view.realized_pnl,
            "dividends": view.dividends,
            "deposited": view.deposited,
            "withdrawn": view.withdrawn,
        },
        "benchmark_symbol": BENCHMARK,
        "candles_available": !closes.is_empty() || benchmark.is_some(),
    })))
}
