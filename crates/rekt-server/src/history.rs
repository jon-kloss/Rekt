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
    let today = rekt_core::taxes::ny_date(Utc::now());
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
    symbols.extend(repo::recommendation_symbols(&state.db).await?); // outcome tracking
    symbols.push(BENCHMARK.to_string());
    symbols.extend(crate::market::index_symbols()); // market gauges need candles
    symbols.sort();
    symbols.dedup();

    let mut fetched_any = false;
    for symbol in symbols {
        // One symbol fetched at a time across ALL backfill runs: a
        // concurrent run (watchlist-add spawn vs scheduler) would re-fetch
        // the same range and contend for SQLite's single writer. Per-symbol
        // scope so a waiter is blocked for one fetch, not a whole run —
        // the loser re-reads the cached bounds and no-ops.
        let _guard = state.backfill_lock.lock().await;
        let last_cached = repo::last_candle_date(&state.db, &symbol).await?;
        let first_cached = repo::first_candle_date(&state.db, &symbol).await?;
        // How far back we've already TRIED to reach (persisted floor): a
        // provider with no data before `first_cached` would otherwise have the
        // backward gap re-requested every run forever.
        let floor_key = format!("candle_floor:{symbol}");
        let earliest_attempted = repo::get_setting(&state.db, &floor_key)
            .await?
            .and_then(|s| s.parse::<NaiveDate>().ok());
        // Mirror backfill_ranges' backward condition so we can record the floor.
        let attempting_backward = last_cached.is_some()
            && first_cached.is_some_and(|f| default_from < f)
            && earliest_attempted.is_none_or(|e| default_from < e);
        // Forward tail (resume after the last bar) AND, when older
        // transactions now need earlier data than was first cached, the
        // backward head — so the equity curve + SPY benchmark reach back to
        // the first transaction instead of stalling at the startup window.
        for (from, to) in backfill_ranges(
            default_from,
            last_cached,
            first_cached,
            today,
            earliest_attempted,
        ) {
            match bars.daily_candles(&symbol, from, to).await {
                Ok(candles) if !candles.is_empty() => {
                    let written = repo::upsert_candles(&state.db, &symbol, &candles).await?;
                    if written > 0 {
                        fetched_any = true;
                        tracing::info!(symbol, bars = written, from = %from, "candles backfilled");
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(symbol, error = %e, "candle backfill failed"),
            }
        }
        // Record that we've reached back to default_from (even if the provider
        // returned nothing earlier than first_cached) so we don't re-request it.
        if attempting_backward {
            repo::set_setting(&state.db, &floor_key, &default_from.to_string()).await?;
        }
    }
    if fetched_any {
        state.live.bump_candles_revision();
    }
    Ok(())
}

/// Background candle backfill for any newly-seen symbols. Idempotent,
/// resumable, and serialized by the backfill lock, so any mutation that may
/// introduce a symbol (transaction create, CSV import, watchlist add) can
/// fire it without duplicating work or blocking the request.
pub fn spawn_backfill(state: &AppState) {
    let bg = state.clone();
    tokio::spawn(async move {
        if let Err(e) = backfill_candles(&bg).await {
            tracing::warn!(error = %e, "background candle backfill failed");
        }
    });
}

/// The (from, to) ranges to fetch for one symbol given the default window,
/// the symbol's currently-cached bounds, today, and the earliest date we've
/// already ATTEMPTED to backfill (the floor — so a symbol whose provider has
/// no data before `first_cached` isn't re-requested every run). Returns 0–2
/// ranges: the forward tail (newest bars) and/or the backward head (older bars
/// a later-added transaction now needs). Pure so the gap logic is unit-tested.
fn backfill_ranges(
    default_from: NaiveDate,
    last_cached: Option<NaiveDate>,
    first_cached: Option<NaiveDate>,
    today: NaiveDate,
    earliest_attempted: Option<NaiveDate>,
) -> Vec<(NaiveDate, NaiveDate)> {
    let mut ranges = Vec::new();
    match last_cached {
        // Nothing cached yet: one fetch over the whole default window.
        None => {
            if default_from < today {
                ranges.push((default_from, today));
            }
        }
        Some(last) => {
            // Forward tail: resume after the last cached bar; a 3-day overlap
            // re-fetches the most recent bar in case it was written intraday.
            let fwd = last - Duration::days(3);
            if fwd < today {
                ranges.push((fwd, today));
            }
            // Backward head: the cached window starts later than now needed
            // (older tx added after the symbol was first cached) — fill the
            // earlier gap, overlapping the existing start by a few days but
            // never past today. Skip if we've already tried this far back and
            // the provider returned nothing (floor), so we don't re-request a
            // dead range every run.
            if let Some(first) = first_cached {
                let below_floor = earliest_attempted.is_none_or(|e| default_from < e);
                if default_from < first && below_floor {
                    ranges.push((default_from, (first + Duration::days(3)).min(today)));
                }
            }
        }
    }
    ranges
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

#[derive(Debug, serde::Deserialize)]
pub struct CandlesQuery {
    pub symbol: String,
    /// Bars to return, oldest first (default 252 ≈ one trading year).
    #[serde(default)]
    pub days: Option<i64>,
}

/// GET /api/candles?symbol=AAPL&days=252 — daily OHLCV for one symbol, for
/// the Terminal candlestick chart. We only store DAILY bars (Alpaca), so
/// intraday timeframes degrade to daily honestly. Fill markers are derived
/// client-side from the transaction log.
pub async fn candles(
    State(state): State<AppState>,
    Query(query): Query<CandlesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let symbol = crate::api::validate_symbol(&query.symbol)?;
    let days = query.days.unwrap_or(252).clamp(2, 2000);
    let bars = repo::recent_candles(&state.db, &symbol, days)
        .await
        .map_err(internal)?;
    // Compact keys (o/h/l/c/v/date) match the design's candle shape.
    let candles: Vec<serde_json::Value> = bars
        .iter()
        .map(|c| {
            serde_json::json!({
                "date": c.date,
                "o": c.open,
                "h": c.high,
                "l": c.low,
                "c": c.close,
                "v": c.volume,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "symbol": symbol,
        "timeframe": "daily",
        "candles": candles,
    })))
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
pub async fn history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let range = query.range.as_deref().unwrap_or("all");
    tracing::debug!(range, "GET /api/history");
    Ok(Json(history_payload(&state, range).await?))
}

/// The history response body, callable outside the HTTP layer too (the AI
/// analyst's get_performance tool reads the same numbers the UI shows).
///
/// The full series is cached on (tx_revision, candles_revision, day): range
/// clicks and reloads slice the cached points instead of replaying the log.
pub async fn history_payload(state: &AppState, range: &str) -> Result<serde_json::Value, ApiError> {
    let days = match range {
        "1m" => Some(31),
        "3m" => Some(92),
        "1y" => Some(366),
        "all" => None,
        other => {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unknown range {other:?} (use 1m, 3m, 1y or all)"),
            ))
        }
    };

    // NY calendar date: plain UTC would chart a phantom "tomorrow" between
    // 8pm and midnight ET.
    let today = rekt_core::taxes::ny_date(Utc::now());
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
        Some(hit) => {
            tracing::trace!(tx_rev, candles_rev, "history cache hit");
            hit
        }
        None => {
            tracing::debug!(
                tx_rev,
                candles_rev,
                "history cache miss — rebuilding series"
            );
            build_series(state, key).await?
        }
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
    Ok(serde_json::Value::Object(body))
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

#[cfg(test)]
mod tests {
    use super::backfill_ranges;
    use chrono::NaiveDate;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn nothing_cached_fetches_whole_window() {
        let r = backfill_ranges(d("2024-01-01"), None, None, d("2026-06-13"), None);
        assert_eq!(r, vec![(d("2024-01-01"), d("2026-06-13"))]);
    }

    #[test]
    fn up_to_date_symbol_only_refetches_forward_overlap() {
        // last cached two days ago: just the 3-day forward overlap tail.
        let r = backfill_ranges(
            d("2024-01-01"),
            Some(d("2026-06-11")),
            Some(d("2024-01-01")),
            d("2026-06-13"),
            None,
        );
        assert_eq!(r, vec![(d("2026-06-08"), d("2026-06-13"))]);
    }

    #[test]
    fn older_tx_added_later_triggers_backward_fill() {
        // The bug behind F2: SPY was cached only from the startup signal
        // window (2025-05-29). A transaction back to 2024-01-01 is then
        // added, so default_from moves earlier than first_cached and the
        // earlier gap must be fetched, not just the forward tail.
        let r = backfill_ranges(
            d("2024-01-01"),
            Some(d("2026-06-12")),
            Some(d("2025-05-29")),
            d("2026-06-13"),
            None,
        );
        assert_eq!(
            r,
            vec![
                (d("2026-06-09"), d("2026-06-13")), // forward tail
                (d("2024-01-01"), d("2025-06-01")), // backward head (+3d overlap)
            ]
        );
    }

    #[test]
    fn no_backward_fill_when_cache_already_covers_window() {
        // first_cached already at/earlier than default_from: forward only.
        let r = backfill_ranges(
            d("2024-01-01"),
            Some(d("2026-06-12")),
            Some(d("2023-12-25")),
            d("2026-06-13"),
            None,
        );
        assert_eq!(r, vec![(d("2026-06-09"), d("2026-06-13"))]);
    }

    #[test]
    fn fully_current_symbol_yields_overlap_tail_only() {
        // last cached today; forward tail (today-3) is still < today so a
        // tiny overlap fetch is expected, but no backward gap.
        let r = backfill_ranges(
            d("2024-01-01"),
            Some(d("2026-06-13")),
            Some(d("2024-01-01")),
            d("2026-06-13"),
            None,
        );
        assert_eq!(r, vec![(d("2026-06-10"), d("2026-06-13"))]);
    }

    #[test]
    fn backward_head_never_exceeds_today() {
        // first_cached set yesterday: first+3d would be in the future; cap at today.
        let r = backfill_ranges(
            d("2024-01-01"),
            Some(d("2026-06-12")),
            Some(d("2026-06-12")),
            d("2026-06-13"),
            None,
        );
        // forward tail + backward head capped at today (not 2026-06-15).
        assert_eq!(
            r,
            vec![
                (d("2026-06-09"), d("2026-06-13")),
                (d("2024-01-01"), d("2026-06-13")),
            ]
        );
    }

    #[test]
    fn floor_suppresses_repeat_backward_fetch() {
        // Already attempted back to default_from (provider had nothing earlier):
        // no backward range this run, just the forward tail.
        let r = backfill_ranges(
            d("2024-01-01"),
            Some(d("2026-06-12")),
            Some(d("2025-05-29")),
            d("2026-06-13"),
            Some(d("2024-01-01")),
        );
        assert_eq!(r, vec![(d("2026-06-09"), d("2026-06-13"))]);
    }

    #[test]
    fn deeper_tx_added_reattempts_below_floor() {
        // A still-earlier tx moves default_from below the floor — retry backward.
        let r = backfill_ranges(
            d("2023-06-01"),
            Some(d("2026-06-12")),
            Some(d("2025-05-29")),
            d("2026-06-13"),
            Some(d("2024-01-01")),
        );
        assert_eq!(
            r,
            vec![
                (d("2026-06-09"), d("2026-06-13")),
                (d("2023-06-01"), d("2025-06-01")),
            ]
        );
    }
}
