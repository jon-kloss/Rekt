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

use axum::extract::ws::{Message, WebSocket};
use chrono::{DateTime, Utc};
use rekt_core::market::us_market_status;
use rekt_core::portfolio::{compute_basis, value, PriceView};
use rust_decimal::Decimal;
use tokio::sync::{broadcast, mpsc, watch, RwLock};

use crate::{repo, AppState};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub price: Decimal,
    pub prev_close: Option<Decimal>,
    pub ts: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub struct Live {
    prices: RwLock<HashMap<String, CacheEntry>>,
    dirty: AtomicBool,
    /// Bumped on every transaction mutation; lets clients detect that the
    /// transaction list (not just prices) changed and refetch it.
    tx_revision: AtomicU64,
    /// Bumped when the candle cache gains data (signals must recompute).
    candles_revision: AtomicU64,
    /// (tx_rev, candles_rev, per-symbol signals) — recomputed only when a
    /// revision moves, not on every price tick.
    signals_cache: RwLock<(u64, u64, HashMap<String, serde_json::Value>)>,
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

    /// Call when new candles land (daily backfill).
    pub fn bump_candles_revision(&self) {
        self.candles_revision.fetch_add(1, Ordering::Relaxed);
        self.mark_dirty();
    }
}

/// Quant-signal badges per open position, cached by (tx, candles) revision
/// so the per-second broadcaster never recomputes or re-queries needlessly.
async fn position_signals(
    state: &AppState,
    symbols: &[String],
) -> HashMap<String, serde_json::Value> {
    let tx_rev = state.live.tx_revision.load(Ordering::Relaxed);
    let candle_rev = state.live.candles_revision.load(Ordering::Relaxed);
    {
        let cache = state.live.signals_cache.read().await;
        if cache.0 == tx_rev && cache.1 == candle_rev {
            return cache.2.clone();
        }
    }
    let mut map = HashMap::new();
    for symbol in symbols {
        match repo::recent_closes(&state.db, symbol, 250).await {
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
    *state.live.signals_cache.write().await = (tx_rev, candle_rev, map.clone());
    map
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

    // Upstream ingest (only with a provider key).
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

        // Seed prev_close + a starting price for newly tracked symbols via
        // REST (the trade stream carries neither).
        let seed_state = state.clone();
        let mut seed_rx = symbols_rx;
        tokio::spawn(async move {
            loop {
                let symbols: Vec<String> = seed_rx.borrow_and_update().clone();
                seed_missing_quotes(&seed_state, &symbols).await;
                if seed_rx.changed().await.is_err() {
                    return;
                }
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
                    let _ = bcast_state.snapshots.send(snapshot.to_string());
                }
                Err(e) => tracing::error!(error = %e, "snapshot recompute failed"),
            }
        }
    });

    state
}

pub async fn seed_missing_quotes(state: &AppState, symbols: &[String]) {
    let Some(provider) = &state.market else {
        return;
    };

    // One lock to find the gaps, then fetch concurrently, then one lock to
    // store — never hold the cache across network calls.
    let missing: Vec<String> = {
        let prices = state.live.prices.read().await;
        symbols
            .iter()
            .filter(|s| !prices.contains_key(*s))
            .cloned()
            .collect()
    };
    if missing.is_empty() {
        return;
    }

    let fetches = missing.iter().map(|symbol| provider.quote(symbol));
    let results = futures_util::future::join_all(fetches).await;

    let mut prices = state.live.prices.write().await;
    let mut seeded = false;
    for (symbol, result) in missing.iter().zip(results) {
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

/// Recompute the desired upstream subscription set after any transaction
/// mutation: every symbol ever transacted (any mode — paper orders need
/// prices too) + the watchlist. A superset of open positions is fine; a
/// few extra subscriptions cost nothing at personal scale.
pub async fn refresh_symbols(state: &AppState) -> anyhow::Result<()> {
    let mut symbols = repo::all_symbols(&state.db).await?;
    symbols.extend(repo::watchlist_symbols(&state.db).await?);
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
pub async fn portfolio_snapshot(state: &AppState) -> anyhow::Result<serde_json::Value> {
    // The headline portfolio is REAL holdings only; paper activity lives in
    // the trading block (PLAN.md §7 segregation).
    let txs = repo::fetch_mode_txs(&state.db, "live").await?;
    let prices = state.live.price_views().await;
    let now = Utc::now();
    let tx_revision = state.live.tx_revision.load(Ordering::Relaxed);
    let trading = crate::trading::snapshot_block(state).await;
    let payload = match compute_basis(&txs) {
        Ok(basis) => {
            let view = value(&basis, &prices);
            let open_symbols: Vec<String> =
                view.positions.iter().map(|p| p.symbol.clone()).collect();
            let signals = position_signals(state, &open_symbols).await;
            let mut view_value = serde_json::to_value(&view).unwrap_or_default();
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
            serde_json::json!({
                "type": "portfolio",
                "ts": now,
                "market": us_market_status(now),
                "live_feed": state.finnhub_token.is_some(),
                "tx_revision": tx_revision,
                "trading": trading,
                "portfolio": view_value,
            })
        }
        // A historically invalid log (e.g. an oversell snuck in via CSV
        // edits) must be visible, not a blank dashboard.
        Err(e) => serde_json::json!({
            "type": "error",
            "ts": now,
            "tx_revision": tx_revision,
            "trading": trading,
            "error": format!("transaction log is inconsistent: {e}"),
        }),
    };
    Ok(payload)
}

/// Browser websocket: send one snapshot immediately, then forward broadcasts.
pub async fn client_ws(socket: WebSocket, state: AppState) {
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
