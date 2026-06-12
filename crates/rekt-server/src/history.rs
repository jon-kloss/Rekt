//! Phase 3: candle backfill, the equity-curve endpoint, and EOD snapshots.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use std::sync::Arc;

use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Utc, Weekday};
use chrono_tz::America::New_York;
use rekt_core::history::{equity_series, metrics_for, DayPoint};
use rekt_core::portfolio::compute_basis;
use rust_decimal::Decimal;

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
    let first = repo::first_tx_date(&state.db).await?;
    // NY calendar date: plain UTC is a day ahead between 8pm and midnight ET.
    let today = Utc::now().with_timezone(&New_York).date_naive();
    // Signals (SMA200, drawdown, drawdown alerts) want ~250 trading bars
    // even for symbols never transacted (watchlist/alert-only), so the
    // default window reaches back at least that far.
    let signal_start = today - Duration::days(380);
    let default_from = match first {
        Some(first) => (first - Duration::days(7)).min(signal_start),
        None => signal_start,
    };

    let mut symbols = repo::all_symbols(&state.db).await?;
    symbols.extend(repo::watchlist_symbols(&state.db).await?);
    symbols.extend(repo::alert_symbols(&state.db).await?); // drawdown alerts need closes
    symbols.push(BENCHMARK.to_string());
    symbols.sort();
    symbols.dedup();

    let mut fetched_any = false;
    for symbol in symbols {
        // Resume after the last cached candle; a small overlap re-fetches
        // the most recent bar in case it was written intraday.
        let from = match repo::last_candle_date(&state.db, &symbol).await? {
            Some(last) => last - Duration::days(3),
            None => default_from,
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

/// Write today's EOD snapshots (one per mode) once the market has closed
/// (NY). Skips weekends and dates already snapshotted, so each trading day
/// is captured exactly once, shortly after the close. (Market holidays do
/// get a redundant flat row — harmless, and a holiday calendar isn't worth
/// the dependency yet.)
pub async fn maybe_snapshot_eod(state: &AppState) -> anyhow::Result<()> {
    let now_ny = Utc::now().with_timezone(&New_York);
    if matches!(now_ny.weekday(), Weekday::Sat | Weekday::Sun) {
        return Ok(());
    }
    if now_ny.time() < NaiveTime::from_hms_opt(16, 5, 0).expect("valid time") {
        return Ok(());
    }
    let date = now_ny.date_naive();
    let prices = state.live.price_views().await;
    for mode in ["live", "paper"] {
        if repo::has_snapshot(&state.db, date, mode).await? {
            continue;
        }
        let txs = repo::fetch_mode_txs(&state.db, mode).await?;
        if txs.is_empty() {
            continue;
        }
        let Ok(basis) = compute_basis(&txs) else {
            continue; // inconsistent log is surfaced elsewhere
        };
        let view = rekt_core::portfolio::value(&basis, &prices);
        repo::upsert_snapshot(
            &state.db,
            date,
            mode,
            view.equity,
            view.cash,
            view.deposited - view.withdrawn,
            view.realized_pnl,
        )
        .await?;
    }
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub range: Option<String>,
}

/// Turn stored EOD snapshots into chartable day points (the fallback when
/// no candles exist yet — e.g. Alpaca keys were configured late). Flows are
/// recovered from the day-over-day change in net deposits.
fn points_from_snapshots(snaps: &[repo::SnapshotRow]) -> Vec<DayPoint> {
    let mut prev_invested: Option<Decimal> = None;
    snaps
        .iter()
        .map(|snap| {
            let flow = match prev_invested {
                Some(prev) => snap.invested - prev,
                None => Decimal::ZERO, // opening value carries the first day
            };
            prev_invested = Some(snap.invested);
            DayPoint {
                date: snap.date,
                equity: snap.total_value,
                cash: snap.cash,
                net_deposited: snap.invested,
                benchmark: None,
                flow,
            }
        })
        .collect()
}

/// GET /api/history?range=1m|3m|1y|all — the equity curve with benchmark
/// overlay and window-accurate TWR/IRR.
///
/// The full series is cached on (tx_revision, candles_revision, day): range
/// clicks and reloads slice the cached points instead of replaying the log.
pub async fn history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
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

    // NY calendar date: plain UTC would chart a phantom "tomorrow" between
    // 8pm and midnight ET.
    let today = Utc::now().with_timezone(&New_York).date_naive();
    let (tx_rev, candles_rev) = state.live.revisions();
    let key = (tx_rev, candles_rev, today);

    let cached = {
        let cache = state.live.history_cache.read().await;
        cache
            .as_ref()
            .filter(|(k, _, _)| *k == key)
            .map(|(_, points, meta)| (points.clone(), meta.clone()))
    };
    let (points, meta) = match cached {
        Some(hit) => hit,
        None => build_series(&state, key).await?,
    };

    let sliced: &[DayPoint] = match days {
        Some(days) if points.len() > days => &points[points.len() - days..],
        _ => &points,
    };
    let metrics = (sliced.len() > 1).then(|| metrics_for(sliced));

    let mut body = meta.as_object().cloned().unwrap_or_default();
    body.insert(
        "points".into(),
        serde_json::to_value(sliced).map_err(internal)?,
    );
    body.insert(
        "metrics".into(),
        serde_json::to_value(metrics).map_err(internal)?,
    );
    Ok(Json(serde_json::Value::Object(body)))
}

/// Build (and cache) the full equity series + range-independent metadata.
async fn build_series(
    state: &AppState,
    key: (u64, u64, NaiveDate),
) -> Result<(Arc<Vec<DayPoint>>, serde_json::Value), ApiError> {
    let txs = repo::fetch_mode_txs(&state.db, "live")
        .await
        .map_err(internal)?;
    if txs.is_empty() {
        // Not cached: trivially cheap, and the first transaction must show
        // up instantly regardless of revision bookkeeping.
        return Ok((
            Arc::new(Vec::new()),
            serde_json::json!({
                "note": "no transactions yet — the curve starts with your first one",
            }),
        ));
    }

    // Totals come from the same replayed log as the curve; an inconsistent
    // log (e.g. hand-edited CSV oversell) is a visible 409, not a blank chart.
    let basis = compute_basis(&txs).map_err(|e| {
        err(
            StatusCode::CONFLICT,
            format!("transaction log inconsistent: {e}"),
        )
    })?;

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
    let candles_available = !closes.is_empty() || benchmark.is_some();

    let (points, source) = if candles_available {
        let (points, _) = equity_series(&txs, &closes, benchmark.as_ref(), key.2);
        (points, "candles")
    } else {
        // No candles at all — fall back to stored EOD snapshots, which carry
        // real marked values, rather than charting avg-cost flatlines.
        let snaps = repo::fetch_snapshots(&state.db, "live")
            .await
            .map_err(internal)?;
        if snaps.is_empty() {
            let (points, _) = equity_series(&txs, &closes, None, key.2);
            (points, "candles")
        } else {
            (points_from_snapshots(&snaps), "snapshots")
        }
    };

    let meta = serde_json::json!({
        "totals": {
            "realized_pnl": basis.positions.values().map(|p| p.realized_pnl).sum::<Decimal>(),
            "dividends": basis.positions.values().map(|p| p.dividends).sum::<Decimal>(),
            "deposited": basis.deposited,
            "withdrawn": basis.withdrawn,
        },
        "benchmark_symbol": BENCHMARK,
        "candles_available": candles_available,
        "source": source,
    });

    let points = Arc::new(points);
    *state.live.history_cache.write().await = Some((key, points.clone(), meta.clone()));
    Ok((points, meta))
}
