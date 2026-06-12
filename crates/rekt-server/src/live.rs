//! The live pipeline: upstream trades → price cache → throttled portfolio
//! broadcast → browser websockets.
//!
//! Flow (PLAN.md §6): one ingest task owns the upstream Finnhub websocket;
//! ticks land in the in-memory price cache; a broadcaster recomputes the
//! portfolio at most once per second *and only when something changed*,
//! pushing a full snapshot JSON to every connected browser. Browsers never
//! talk to providers directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
                let mut prices = ingest_live.prices.write().await;
                let entry = prices.entry(trade.symbol).or_insert(CacheEntry {
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
            if !bcast_state.live.dirty.swap(false, Ordering::Relaxed) {
                continue;
            }
            if bcast_state.snapshots.receiver_count() == 0 {
                continue;
            }
            match portfolio_snapshot_json(&bcast_state).await {
                Ok(json) => {
                    let _ = bcast_state.snapshots.send(json);
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
    for symbol in symbols {
        let known = state.live.prices.read().await.contains_key(symbol);
        if known {
            continue;
        }
        match provider.quote(symbol).await {
            Ok(quote) => {
                state.live.prices.write().await.insert(
                    symbol.clone(),
                    CacheEntry {
                        price: quote.price,
                        prev_close: Some(quote.prev_close),
                        ts: Some(quote.ts),
                    },
                );
                state.live.mark_dirty();
            }
            Err(e) => tracing::warn!(symbol, error = %e, "seed quote failed"),
        }
    }
}

/// Recompute the desired upstream subscription set after any transaction
/// mutation: symbols with open positions + the watchlist.
pub async fn refresh_symbols(state: &AppState) -> anyhow::Result<()> {
    let txs = repo::fetch_all_txs(&state.db).await?;
    let mut symbols: Vec<String> = match compute_basis(&txs) {
        Ok(basis) => basis
            .positions
            .iter()
            .filter(|(_, p)| p.qty > Decimal::ZERO)
            .map(|(s, _)| s.clone())
            .collect(),
        Err(_) => Vec::new(), // invalid log is surfaced by the API path, not here
    };
    symbols.sort();
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

/// Full dashboard payload: portfolio view + market status.
pub async fn portfolio_snapshot_json(state: &AppState) -> anyhow::Result<String> {
    let txs = repo::fetch_all_txs(&state.db).await?;
    let prices = state.live.price_views().await;
    let now = Utc::now();
    let payload = match compute_basis(&txs) {
        Ok(basis) => {
            let view = value(&basis, &prices);
            serde_json::json!({
                "type": "portfolio",
                "ts": now,
                "market": us_market_status(now),
                "live_feed": state.finnhub_token.is_some(),
                "portfolio": view,
            })
        }
        // A historically invalid log (e.g. an oversell snuck in via CSV
        // edits) must be visible, not a blank dashboard.
        Err(e) => serde_json::json!({
            "type": "error",
            "ts": now,
            "error": format!("transaction log is inconsistent: {e}"),
        }),
    };
    Ok(payload.to_string())
}

/// Browser websocket: send one snapshot immediately, then forward broadcasts.
pub async fn client_ws(socket: WebSocket, state: AppState) {
    let mut socket = socket;
    if let Ok(snapshot) = portfolio_snapshot_json(&state).await {
        if socket.send(Message::text(snapshot)).await.is_err() {
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
