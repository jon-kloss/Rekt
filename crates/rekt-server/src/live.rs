//! The live pipeline: upstream trades → price cache → throttled portfolio
//! broadcast → browser websockets.
//!
//! Flow (PLAN.md §6): one ingest task owns the upstream Finnhub websocket;
//! ticks land in the in-memory price cache; a broadcaster recomputes the
//! portfolio at most once per second *and only when something changed*,
//! pushing a full snapshot JSON to every connected browser. Browsers never
//! talk to providers directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::ws::{Message, WebSocket};
use chrono::{DateTime, NaiveDate, Utc};
use rekt_core::history::DayPoint;
use rekt_core::market::us_market_status;
use rekt_core::portfolio::{compute_basis, value, PortfolioBasis, PriceView};
use rust_decimal::Decimal;
use tokio::sync::{broadcast, mpsc, watch, RwLock};

use crate::{repo, AppState};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub price: Decimal,
    pub prev_close: Option<Decimal>,
    pub ts: Option<DateTime<Utc>>,
}

/// Bars fed to the signal engine: ~250 trading days ≈ one year. The UI
/// drawdown badge advertises exactly this window — keep them in sync.
pub const SIGNAL_WINDOW_BARS: i64 = 250;

/// Cached /api/history payload: (tx_rev, candles_rev, series end date) →
/// full point series + range-independent metadata (totals etc.).
type HistoryCache = ((u64, u64, NaiveDate), Arc<Vec<DayPoint>>, serde_json::Value);

#[derive(Default)]
pub struct Live {
    prices: RwLock<HashMap<String, CacheEntry>>,
    dirty: AtomicBool,
    /// Bumped on every transaction mutation; lets clients detect that the
    /// transaction list (not just prices) changed and refetch it.
    tx_revision: AtomicU64,
    /// Bumped when the candle cache gains data (signals must recompute).
    candles_revision: AtomicU64,
    /// (tx_rev, candles_rev, watchlist_rev, per-symbol signals) — recomputed
    /// only when a revision moves, not on every price tick.
    signals_cache: RwLock<(u64, u64, u64, HashMap<String, serde_json::Value>)>,
    /// tx_rev → replayed basis: the broadcaster runs up to once a second on
    /// price ticks, and the transaction log only changes when tx_rev moves.
    basis_cache: RwLock<Option<(u64, Arc<PortfolioBasis>)>>,
    /// See [`HistoryCache`] — makes the 4 range buttons (and any reload)
    /// free once the series is built for the current revisions.
    pub(crate) history_cache: RwLock<Option<HistoryCache>>,
    /// Bumped on alert create/delete/trigger/dismiss/rearm.
    alerts_revision: AtomicU64,
    /// alerts_rev → serialized alert list for the snapshot payload.
    alerts_cache: RwLock<Option<(u64, serde_json::Value)>>,
    /// Bumped on watchlist add/remove.
    watchlist_revision: AtomicU64,
    /// watchlist_rev → symbol list (the per-tick watchlist block reads this,
    /// not the database).
    watchlist_cache: RwLock<Option<(u64, Arc<Vec<String>>)>>,
    /// candles_rev → last-known daily close per symbol, used to value
    /// positions when the live feed has no quote (market closed, or no
    /// market-data key). Keyed by candles_revision so it never touches the
    /// DB on the per-tick broadcaster path.
    fallback_cache: RwLock<Option<(u64, HashMap<String, PriceView>)>>,
}

pub struct LiveHandles {
    pub live: Arc<Live>,
    /// Browser-facing snapshot fan-out.
    pub snapshots: broadcast::Sender<String>,
    /// Desired upstream subscription set (held + watched symbols).
    pub symbols: watch::Sender<Vec<String>>,
}

impl Live {
    pub async fn price_views(&self) -> HashMap<String, PriceView> {
        self.prices
            .read()
            .await
            .iter()
            .map(|(symbol, entry)| {
                (
                    symbol.clone(),
                    PriceView {
                        price: entry.price,
                        prev_close: entry.prev_close,
                        ts: entry.ts,
                    },
                )
            })
            .collect()
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Call after any transaction mutation (create/delete/import).
    pub fn bump_tx_revision(&self) {
        self.tx_revision.fetch_add(1, Ordering::Relaxed);
        self.mark_dirty();
    }

    /// Current transaction revision — gates caches keyed on the tx log
    /// (e.g. the paper-positions cache in the trading snapshot block).
    pub fn tx_revision(&self) -> u64 {
        self.tx_revision.load(Ordering::Relaxed)
    }

    /// Call when new candles land (daily backfill).
    pub fn bump_candles_revision(&self) {
        self.candles_revision.fetch_add(1, Ordering::Relaxed);
        self.mark_dirty();
    }

    /// Call on any alert mutation or trigger.
    pub fn bump_alerts_revision(&self) {
        self.alerts_revision.fetch_add(1, Ordering::Relaxed);
        self.mark_dirty();
    }

    /// Call on watchlist add/remove.
    pub fn bump_watchlist_revision(&self) {
        self.watchlist_revision.fetch_add(1, Ordering::Relaxed);
        self.mark_dirty();
    }

    /// Current (tx, candles) revisions — cache keys for derived data.
    pub fn revisions(&self) -> (u64, u64) {
        (
            self.tx_revision.load(Ordering::Relaxed),
            self.candles_revision.load(Ordering::Relaxed),
        )
    }

    /// Test hook: inject a price as if a tick had arrived.
    #[cfg(test)]
    pub async fn set_price(&self, symbol: &str, price: Decimal) {
        // trace, not debug: one per inbound trade tick.
        tracing::trace!(symbol, price = %price, "price tick");
        self.prices.write().await.insert(
            symbol.to_uppercase(),
            CacheEntry {
                price,
                prev_close: None,
                ts: Some(Utc::now()),
            },
        );
        self.mark_dirty();
    }
}

/// Quant-signal badges per symbol (open positions + watchlist), cached by
/// (tx, candles, watchlist) revision so the per-second broadcaster never
/// recomputes or re-queries needlessly.
async fn position_signals(
    state: &AppState,
    symbols: &[String],
) -> HashMap<String, serde_json::Value> {
    let tx_rev = state.live.tx_revision.load(Ordering::Relaxed);
    let candle_rev = state.live.candles_revision.load(Ordering::Relaxed);
    let watch_rev = state.live.watchlist_revision.load(Ordering::Relaxed);
    {
        let cache = state.live.signals_cache.read().await;
        if cache.0 == tx_rev && cache.1 == candle_rev && cache.2 == watch_rev {
            return cache.3.clone();
        }
    }
    let mut map = HashMap::new();
    for symbol in symbols {
        match repo::recent_closes(&state.db, symbol, SIGNAL_WINDOW_BARS).await {
            Ok(closes) if !closes.is_empty() => {
                let summary = rekt_core::signals::summarize(&closes);
                if let Ok(value) = serde_json::to_value(summary) {
                    map.insert(symbol.clone(), value);
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(symbol, error = %e, "signal closes fetch failed"),
        }
    }
    *state.live.signals_cache.write().await = (tx_rev, candle_rev, watch_rev, map.clone());
    map
}

/// Watchlist symbols via the revision-keyed cache (no DB hit per tick).
async fn watchlist_symbols_cached(state: &AppState) -> anyhow::Result<Arc<Vec<String>>> {
    let rev = state.live.watchlist_revision.load(Ordering::Relaxed);
    {
        let cache = state.live.watchlist_cache.read().await;
        if let Some((r, symbols)) = &*cache {
            if *r == rev {
                return Ok(symbols.clone());
            }
        }
    }
    let symbols = Arc::new(repo::watchlist_symbols(&state.db).await?);
    *state.live.watchlist_cache.write().await = Some((rev, symbols.clone()));
    Ok(symbols)
}

/// Serialized alert list via the revision-keyed cache.
async fn alerts_block(state: &AppState) -> anyhow::Result<serde_json::Value> {
    let rev = state.live.alerts_revision.load(Ordering::Relaxed);
    {
        let cache = state.live.alerts_cache.read().await;
        if let Some((r, value)) = &*cache {
            if *r == rev {
                return Ok(value.clone());
            }
        }
    }
    let alerts = repo::list_alerts(&state.db).await?;
    let value = serde_json::to_value(&alerts).context("serialize alerts")?;
    *state.live.alerts_cache.write().await = Some((rev, value.clone()));
    Ok(value)
}

/// Spawn the whole pipeline; returns handles for the router + mutation paths.
pub fn start(state_factory: impl FnOnce(LiveHandles) -> AppState) -> AppState {
    let live = Arc::new(Live::default());
    let (snapshots, _) = broadcast::channel(16);
    let (symbols_tx, symbols_rx) = watch::channel(Vec::new());

    let state = state_factory(LiveHandles {
        live: live.clone(),
        snapshots: snapshots.clone(),
        symbols: symbols_tx,
    });

    // Upstream ingest (only with a streaming provider key).
    if let Some(token) = state.finnhub_token.clone() {
        let (trades_tx, mut trades_rx) = mpsc::channel(1024);
        tokio::spawn(rekt_data::stream::run_finnhub_stream(
            token,
            symbols_rx.clone(),
            trades_tx,
        ));
        let ingest_live = live.clone();
        tokio::spawn(async move {
            while let Some(trade) = trades_rx.recv().await {
                // Normalize: cache keys must match the uppercased position
                // keys or live prices silently miss the lookup.
                let symbol = trade.symbol.to_uppercase();
                let mut prices = ingest_live.prices.write().await;
                let entry = prices.entry(symbol).or_insert(CacheEntry {
                    price: trade.price,
                    prev_close: None,
                    ts: Some(trade.ts),
                });
                entry.price = trade.price;
                entry.ts = Some(trade.ts);
                drop(prices);
                ingest_live.mark_dirty();
            }
        });
    }

    // Seed prev_close + a starting price for newly tracked symbols via REST
    // (the trade stream carries neither, and without a stream this is the
    // only way symbols get a first price at all).
    if state.market.is_some() {
        let seed_state = state.clone();
        let mut seed_rx = symbols_rx.clone();
        tokio::spawn(async move {
            loop {
                let symbols: Vec<String> = seed_rx.borrow_and_update().clone();
                refresh_quotes(&seed_state, &symbols, true).await;
                if seed_rx.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    // No stream → poll: refresh EVERY tracked quote periodically so an
    // Alpaca-only (or stream-down) deployment doesn't serve the startup
    // seed as if it were live forever.
    if state.finnhub_token.is_none() && state.market.is_some() {
        let poll_state = state.clone();
        let poll_rx = symbols_rx;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await; // the seed task covers startup
            loop {
                tick.tick().await;
                let symbols: Vec<String> = poll_rx.borrow().clone();
                refresh_quotes(&poll_state, &symbols, false).await;
            }
        });
    }

    // Broadcaster: ≤1 snapshot/sec, only when dirty, only with listeners.
    let bcast_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            // Listener check FIRST: consuming the dirty flag with nobody
            // connected would drop the update — a later subscriber would
            // wait for the next trade instead of getting fresh state.
            if bcast_state.snapshots.receiver_count() == 0 {
                continue;
            }
            if !bcast_state.live.dirty.swap(false, Ordering::Relaxed) {
                continue;
            }
            match portfolio_snapshot(&bcast_state).await {
                Ok(snapshot) => {
                    let receivers = bcast_state.snapshots.receiver_count();
                    // trace, not debug: this fires up to once a second.
                    tracing::trace!(receivers, "broadcasting portfolio snapshot");
                    let _ = bcast_state.snapshots.send(snapshot.to_string());
                }
                Err(e) => tracing::error!(error = %e, "snapshot recompute failed"),
            }
        }
    });

    state
}

pub async fn seed_missing_quotes(state: &AppState, symbols: &[String]) {
    refresh_quotes(state, symbols, true).await;
}

/// REST-fetch quotes: only the symbols missing from the cache
/// (`only_missing`, the cheap seed path) or all of them (the polling
/// fallback's staleness refresh).
pub async fn refresh_quotes(state: &AppState, symbols: &[String], only_missing: bool) {
    let Some(provider) = &state.market else {
        return;
    };

    // One lock to find the gaps, then fetch concurrently, then one lock to
    // store — never hold the cache across network calls.
    let wanted: Vec<String> = if only_missing {
        let prices = state.live.prices.read().await;
        symbols
            .iter()
            .filter(|s| !prices.contains_key(*s))
            .cloned()
            .collect()
    } else {
        symbols.to_vec()
    };
    if wanted.is_empty() {
        return;
    }

    let fetches = wanted.iter().map(|symbol| provider.quote(symbol));
    let results = futures_util::future::join_all(fetches).await;

    let mut prices = state.live.prices.write().await;
    let mut seeded = false;
    for (symbol, result) in wanted.iter().zip(results) {
        match result {
            Ok(quote) => {
                prices.insert(
                    symbol.clone(),
                    CacheEntry {
                        price: quote.price,
                        prev_close: Some(quote.prev_close),
                        ts: Some(quote.ts),
                    },
                );
                seeded = true;
            }
            Err(e) => tracing::warn!(symbol, error = %e, "seed quote failed"),
        }
    }
    drop(prices);
    if seeded {
        state.live.mark_dirty();
    }
}

/// Recompute the desired upstream subscription set after any transaction,
/// watchlist or alert mutation: every symbol ever transacted (any mode —
/// paper orders need prices too) + the watchlist + active-alert symbols
/// (conditions need data to evaluate). A superset of open positions is
/// fine; a few extra subscriptions cost nothing at personal scale.
pub async fn refresh_symbols(state: &AppState) -> anyhow::Result<()> {
    let mut symbols = repo::all_symbols(&state.db).await?;
    symbols.extend(repo::watchlist_symbols(&state.db).await?);
    symbols.extend(repo::alert_symbols(&state.db).await?);
    symbols.sort();
    symbols.dedup();
    state.symbols.send_if_modified(|current| {
        if *current == symbols {
            false
        } else {
            *current = symbols;
            true
        }
    });
    state.live.mark_dirty();
    Ok(())
}

/// Full dashboard payload: portfolio view + market status + tx revision +
/// trading state (mode, open orders, pause flag).
/// Fill in a last-known daily close (and the prior close, for day P&L) for
/// any `symbols` the live price cache hasn't quoted. Best-available, honest
/// pricing: a real close with its trade date as the timestamp, never a
/// fabricated number. Cached by `candles_rev` so the per-tick broadcaster
/// never re-queries; the DB is touched only when the candle set changed and
/// a symbol is genuinely missing a live quote.
async fn enrich_with_last_close(
    state: &AppState,
    prices: &mut HashMap<String, PriceView>,
    symbols: &[String],
    candles_rev: u64,
) -> anyhow::Result<()> {
    let missing: Vec<String> = symbols
        .iter()
        .filter(|s| !prices.contains_key(*s))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    {
        let cache = state.live.fallback_cache.read().await;
        if let Some((rev, map)) = &*cache {
            if *rev == candles_rev && missing.iter().all(|s| map.contains_key(s)) {
                for s in &missing {
                    if let Some(pv) = map.get(s) {
                        prices.insert(s.clone(), pv.clone());
                    }
                }
                return Ok(());
            }
        }
    }
    let mut built: HashMap<String, PriceView> = HashMap::new();
    for sym in &missing {
        let cs = repo::recent_candles(&state.db, sym, 2).await?;
        if let Some(last) = cs.last() {
            let prev = if cs.len() >= 2 {
                Some(cs[cs.len() - 2].close)
            } else {
                None
            };
            let ts = last
                .date
                .and_hms_opt(20, 0, 0)
                .map(|t| DateTime::<Utc>::from_naive_utc_and_offset(t, Utc));
            built.insert(
                sym.clone(),
                PriceView {
                    price: last.close,
                    prev_close: prev,
                    ts,
                },
            );
        }
    }
    for (s, pv) in &built {
        prices.insert(s.clone(), pv.clone());
    }
    *state.live.fallback_cache.write().await = Some((candles_rev, built));
    Ok(())
}

pub async fn portfolio_snapshot(state: &AppState) -> anyhow::Result<serde_json::Value> {
    let mut prices = state.live.price_views().await;
    let now = Utc::now();
    let tx_revision = state.live.tx_revision.load(Ordering::Relaxed);
    // Carried in the payload so clients can refresh candle-derived views
    // (history chart, REKT meter) when backfill lands without a tx change.
    let candles_revision = state.live.candles_revision.load(Ordering::Relaxed);
    let trading = crate::trading::snapshot_block(state).await;
    // stream = ticks push in live; poll = REST refresh every minute;
    // none = whatever was seeded (honesty for the UI badge).
    let quotes = if state.finnhub_token.is_some() {
        "stream"
    } else if state.market.is_some() {
        "poll"
    } else {
        "none"
    };

    // The headline portfolio is REAL holdings only; paper activity lives in
    // the trading block (PLAN.md §7 segregation). The replayed basis only
    // changes when tx_revision moves — don't re-query the log per price tick.
    let cached = {
        let cache = state.live.basis_cache.read().await;
        match &*cache {
            Some((rev, basis)) if *rev == tx_revision => Some(Ok(basis.clone())),
            _ => None,
        }
    };
    let basis_result = match cached {
        Some(hit) => hit,
        None => {
            let txs = repo::fetch_mode_txs(&state.db, "live").await?;
            match compute_basis(&txs) {
                Ok(basis) => {
                    let basis = Arc::new(basis);
                    *state.live.basis_cache.write().await = Some((tx_revision, basis.clone()));
                    Ok(basis)
                }
                Err(e) => Err(e),
            }
        }
    };

    let watch_symbols = watchlist_symbols_cached(state).await?;
    let alerts = alerts_block(state).await?;

    let payload = match basis_result {
        Ok(basis) => {
            // Value positions/watchlist at the last stored close when the
            // live feed hasn't quoted them (market closed / no feed key).
            let mut needed: Vec<String> = basis.positions.keys().cloned().collect();
            needed.extend(watch_symbols.iter().cloned());
            if let Err(e) =
                enrich_with_last_close(state, &mut prices, &needed, candles_revision).await
            {
                tracing::warn!(error = %e, "last-close fallback pricing failed");
            }
            let view = value(&basis, &prices);
            let mut signal_symbols: Vec<String> =
                view.positions.iter().map(|p| p.symbol.clone()).collect();
            signal_symbols.extend(watch_symbols.iter().cloned());
            signal_symbols.sort();
            signal_symbols.dedup();
            let signals = position_signals(state, &signal_symbols).await;
            let mut view_value = serde_json::to_value(&view).context("serialize portfolio view")?;
            if let Some(positions) = view_value
                .get_mut("positions")
                .and_then(|p| p.as_array_mut())
            {
                for position in positions {
                    if let Some(symbol) = position["symbol"].as_str() {
                        if let Some(signal) = signals.get(symbol) {
                            position["signals"] = signal.clone();
                        }
                    }
                }
            }
            // Watchlist rows: cached symbols + in-memory prices/signals only.
            let watchlist: Vec<serde_json::Value> = watch_symbols
                .iter()
                .map(|symbol| {
                    let quote = prices.get(symbol);
                    serde_json::json!({
                        "symbol": symbol,
                        "price": quote.map(|q| q.price),
                        "prev_close": quote.and_then(|q| q.prev_close),
                        "price_ts": quote.and_then(|q| q.ts),
                        "signals": signals.get(symbol),
                    })
                })
                .collect();
            serde_json::json!({
                "type": "portfolio",
                "ts": now,
                "market": us_market_status(now),
                "live_feed": state.finnhub_token.is_some(),
                "quotes": quotes,
                "tx_revision": tx_revision,
                "candles_revision": candles_revision,
                "active_portfolio": state.active_portfolio,
                "trading": trading,
                "portfolio": view_value,
                "watchlist": watchlist,
                "alerts": alerts,
            })
        }
        // A historically invalid log (e.g. an oversell snuck in via CSV
        // edits) must be visible, not a blank dashboard.
        Err(e) => serde_json::json!({
            "type": "error",
            "ts": now,
            "tx_revision": tx_revision,
            "candles_revision": candles_revision,
            "active_portfolio": state.active_portfolio,
            "trading": trading,
            "alerts": alerts,
            "error": format!("transaction log is inconsistent: {e}"),
        }),
    };
    Ok(payload)
}

/// Browser websocket: send one snapshot immediately, then forward broadcasts.
pub async fn client_ws(socket: WebSocket, state: AppState) {
    tracing::debug!("websocket client connected");
    let mut socket = socket;
    if let Ok(snapshot) = portfolio_snapshot(&state).await {
        if socket
            .send(Message::text(snapshot.to_string()))
            .await
            .is_err()
        {
            return;
        }
    }
    let mut rx = state.snapshots.subscribe();
    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Ok(json) => {
                        if socket.send(Message::text(json)).await.is_err() {
                            return;
                        }
                    }
                    // Lagged is fine: every message is a full snapshot, so
                    // the next received one carries complete fresh state.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            // Drain (and detect close from) the client side.
            incoming = socket.recv() => {
                if incoming.is_none() {
                    return;
                }
            }
        }
    }
}
