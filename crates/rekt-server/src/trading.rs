//! The order manager: tickets in, broker out, fills back into the
//! transaction log.
//!
//! Invariants (PLAN.md §4, docs/RESEARCH.md §3):
//! - client order ids are deterministic (`rekt-{mode}-{rowid}`) and
//!   persisted BEFORE submission — a crash can never duplicate an order.
//! - order-state updates never regress: terminal states are immutable and
//!   `partially_filled` can't fall back to `accepted` (out-of-order stream
//!   vs REST responses are expected, not exceptional).
//! - fills are idempotent on `execution_id` and stamped with the broker's
//!   mode — paper fills never pollute the live portfolio.
//! - externally-placed orders (broker UI) are materialized during
//!   reconciliation so their fills ingest like any other.
//! - no order submission until reconciliation has run; reconciliation
//!   re-runs on stream reconnect AND on a periodic timer, so a blocked
//!   websocket can't lock trading out forever.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::America::New_York;
use rekt_broker::{stream::BrokerEvent, AccountInfo, Broker, BrokerError, BrokerOrder, Execution};
use rekt_core::orders::{check_ticket, OrderStatus, OrderTicket, Side};
use rekt_core::portfolio::compute_basis;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use tokio::sync::RwLock;

use crate::api::{err, internal, ApiError};
use crate::{live, repo, AppState};

/// Trading control flags, all checked on the submission path.
#[derive(Default)]
pub struct TradingState {
    /// Set once reconciliation completes; orders 503 until then.
    pub ready: AtomicBool,
    /// The manual pause switch. Persisted (settings table) — a safety
    /// switch must survive restarts.
    pub paused: AtomicBool,
    /// Bumped on every order mutation; gates the snapshot orders fetch.
    orders_revision: AtomicU64,
    /// (revision, rendered orders JSON) — avoids a DB query per broadcast
    /// tick when no order changed.
    orders_cache: RwLock<(u64, serde_json::Value)>,
    /// Cached paper broker account (cash/equity/buying_power). Refreshed on
    /// reconcile, fills, and order submit — NEVER fetched on the broadcast
    /// path. Lets the order ticket size against the account that actually
    /// executes instead of the (segregated) tracked live portfolio.
    account_cache: RwLock<Option<rekt_broker::AccountInfo>>,
    /// (tx_revision, paper holdings JSON {symbol: qty}). Gated like the live
    /// basis cache so the broadcast path never re-reads the paper log unless
    /// a fill/tx landed.
    paper_positions_cache: RwLock<(u64, serde_json::Value)>,
}

impl TradingState {
    pub fn bump_orders(&self) {
        self.orders_revision.fetch_add(1, Ordering::Relaxed);
    }
}

const PAUSED_SETTING: &str = "trading.paused";
const CLIENT_ID_NONCE_SETTING: &str = "client_order_id.nonce";

/// Per-database nonce mixed into every `client_order_id`. The broker account
/// outlives any single database (a paper account persists across resets,
/// backups, and restores), and a deterministic `rekt-{mode}-{rowid}` id would
/// REPLAY ids 1, 2, 3… on a fresh/restored DB — which the broker rejects with
/// "client_order_id must be unique". Generated once and persisted, so ids stay
/// stable for crash-recovery WITHIN an install but never collide across DB
/// instances on the same broker account.
async fn ensure_client_id_nonce(pool: &SqlitePool) -> Result<String> {
    if let Some(nonce) = repo::get_setting(pool, CLIENT_ID_NONCE_SETTING).await? {
        return Ok(nonce);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let nonce = format!("{:x}{:x}", std::process::id(), nanos);
    repo::set_setting(pool, CLIENT_ID_NONCE_SETTING, &nonce).await?;
    tracing::info!(%nonce, "minted per-database client_order_id nonce");
    Ok(nonce)
}

/// Restore persisted flags at startup.
pub async fn load_persisted(state: &AppState) -> Result<()> {
    let paused = repo::get_setting(&state.db, PAUSED_SETTING)
        .await?
        .map(|v| v == "true")
        .unwrap_or(false);
    state.trading.paused.store(paused, Ordering::Relaxed);
    if paused {
        tracing::warn!("trading is PAUSED (persisted) — resume explicitly to trade");
    }
    Ok(())
}

// ---------------------------------------------------------------- repo --

#[derive(Debug, Clone, serde::Serialize)]
pub struct OrderRow {
    pub id: i64,
    pub client_order_id: String,
    pub broker_order_id: Option<String>,
    pub symbol: String,
    pub side: String,
    pub order_type: String,
    pub qty: Decimal,
    pub limit_price: Option<Decimal>,
    pub tif: String,
    pub status: String,
    pub filled_qty: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub mode: String,
    pub submitted_ts: String,
    pub updated_ts: String,
}

fn dec_opt(s: Option<String>) -> Result<Option<Decimal>> {
    s.map(|v| repo::parse_dec(&v, "orders")).transpose()
}

async fn fetch_orders(pool: &SqlitePool, limit: i64) -> Result<Vec<OrderRow>> {
    let rows = sqlx::query(
        r#"SELECT o.id, o.client_order_id, o.broker_order_id, i.symbol, o.side,
                  o.order_type, o.qty, o.limit_price, o.tif, o.status,
                  o.filled_qty, o.avg_fill_price, o.mode, o.submitted_ts, o.updated_ts
           FROM orders o JOIN instruments i ON i.id = o.instrument_id
           ORDER BY o.id DESC LIMIT ?"#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(OrderRow {
                id: r.get("id"),
                client_order_id: r.get("client_order_id"),
                broker_order_id: r.get("broker_order_id"),
                symbol: r.get("symbol"),
                side: r.get("side"),
                order_type: r.get("order_type"),
                qty: repo::parse_dec(&r.get::<String, _>("qty"), "orders.qty")?,
                limit_price: dec_opt(r.get("limit_price"))?,
                tif: r.get("tif"),
                status: r.get("status"),
                filled_qty: repo::parse_dec(
                    &r.get::<String, _>("filled_qty"),
                    "orders.filled_qty",
                )?,
                avg_fill_price: dec_opt(r.get("avg_fill_price"))?,
                mode: r.get("mode"),
                submitted_ts: r.get("submitted_ts"),
                updated_ts: r.get("updated_ts"),
            })
        })
        .collect()
}

/// Start of today, America/New_York (midnight is never ambiguous there —
/// US DST transitions happen at 2am).
fn ny_day_start() -> DateTime<Utc> {
    Utc::now()
        .with_timezone(&New_York)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_local_timezone(New_York)
        .single()
        .map(|local| local.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

/// Orders submitted today (America/New_York calendar day) — daily cap input.
async fn orders_today(pool: &SqlitePool) -> Result<u32> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM orders WHERE submitted_ts >= ?")
        .bind(ny_day_start().to_rfc3339())
        .fetch_one(pool)
        .await?;
    Ok(row.get::<i64, _>("n") as u32)
}

/// Persist the order intent and mint its deterministic client id.
async fn insert_pending_order(
    pool: &SqlitePool,
    ticket: &OrderTicket,
    mode: &str,
) -> Result<(i64, String)> {
    // Off-transaction: a per-DB nonce so ids never replay against the broker.
    let nonce = ensure_client_id_nonce(pool).await?;
    let mut dbtx = pool.begin().await?;
    let instrument_id = repo::ensure_instrument(&mut dbtx, &ticket.symbol).await?;
    let now = Utc::now().to_rfc3339();
    let id = sqlx::query(
        r#"INSERT INTO orders (client_order_id, instrument_id, side, order_type, qty,
                               limit_price, tif, status, mode, submitted_ts, updated_ts)
           VALUES (?, ?, ?, ?, ?, ?, ?, 'pending_submit', ?, ?, ?)"#,
    )
    .bind(format!("rekt-{mode}-pending")) // placeholder, replaced below
    .bind(instrument_id)
    .bind(ticket.side.as_str())
    .bind(ticket.order_type.as_str())
    .bind(ticket.qty.to_string())
    .bind(ticket.limit_price.map(|p| p.to_string()))
    .bind(ticket.tif.as_str())
    .bind(mode)
    .bind(&now)
    .bind(&now)
    .execute(&mut *dbtx)
    .await?
    .last_insert_rowid();

    // Deterministic within this DB: per-database nonce + the rowid we just
    // claimed. The nonce keeps ids unique across DB resets/restores (see
    // ensure_client_id_nonce) while staying stable for crash-recovery.
    let client_order_id = format!("rekt-{mode}-{nonce}-{id}");
    sqlx::query("UPDATE orders SET client_order_id = ? WHERE id = ?")
        .bind(&client_order_id)
        .bind(id)
        .execute(&mut *dbtx)
        .await?;
    dbtx.commit().await?;
    Ok((id, client_order_id))
}

#[derive(Debug, PartialEq)]
enum ApplyOutcome {
    Updated,
    /// The row exists but the incoming state would regress it (stale
    /// REST response after a faster stream event) — skipped.
    Stale,
    Unknown,
}

/// Apply a broker order state with a staleness guard: terminal states are
/// immutable, and `partially_filled` never regresses to pre-fill states.
async fn apply_broker_order(pool: &SqlitePool, bo: &BrokerOrder) -> Result<ApplyOutcome> {
    let result = sqlx::query(
        r#"UPDATE orders
           SET broker_order_id = ?, status = ?, filled_qty = ?, avg_fill_price = ?, updated_ts = ?
           WHERE client_order_id = ?
             AND status NOT IN ('filled','canceled','rejected','expired','replaced','failed')
             AND NOT (status = 'partially_filled'
                      AND ? IN ('pending_submit','submitted','accepted'))"#,
    )
    .bind(&bo.broker_order_id)
    .bind(bo.status.as_str())
    .bind(bo.filled_qty.to_string())
    .bind(bo.avg_fill_price.map(|p| p.to_string()))
    .bind(bo.updated_ts.unwrap_or_else(Utc::now).to_rfc3339())
    .bind(&bo.client_order_id)
    .bind(bo.status.as_str())
    .execute(pool)
    .await?;
    if result.rows_affected() > 0 {
        return Ok(ApplyOutcome::Updated);
    }
    let exists = sqlx::query("SELECT 1 FROM orders WHERE client_order_id = ?")
        .bind(&bo.client_order_id)
        .fetch_optional(pool)
        .await?
        .is_some();
    Ok(if exists {
        ApplyOutcome::Stale
    } else {
        ApplyOutcome::Unknown
    })
}

/// Materialize an order REKT didn't place (broker UI, another tool) so its
/// fills can ingest — RESEARCH.md §3.3. Keyed on the broker's own client
/// order id (unique per account); idempotent via the UNIQUE constraint.
async fn materialize_external_order(
    pool: &SqlitePool,
    bo: &BrokerOrder,
    mode: &str,
) -> Result<bool> {
    let (Some(symbol), Some(side), Some(qty)) = (&bo.symbol, bo.side, bo.qty) else {
        tracing::warn!(
            broker_order_id = %bo.broker_order_id,
            "external order lacks descriptive fields — cannot materialize"
        );
        return Ok(false);
    };
    let mut dbtx = pool.begin().await?;
    let instrument_id = repo::ensure_instrument(&mut dbtx, symbol).await?;
    let now = Utc::now().to_rfc3339();
    let inserted = sqlx::query(
        r#"INSERT OR IGNORE INTO orders
           (client_order_id, broker_order_id, instrument_id, side, order_type, qty,
            limit_price, tif, status, mode, note, submitted_ts, updated_ts)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'external', ?, ?)"#,
    )
    .bind(&bo.client_order_id)
    .bind(&bo.broker_order_id)
    .bind(instrument_id)
    .bind(side.as_str())
    .bind(bo.order_type.as_deref().unwrap_or("market"))
    .bind(qty.to_string())
    .bind(bo.limit_price.map(|p| p.to_string()))
    .bind(bo.tif.as_deref().unwrap_or("day"))
    .bind(bo.status.as_str())
    .bind(mode)
    .bind(&now)
    .bind(&now)
    .execute(&mut *dbtx)
    .await?
    .rows_affected()
        > 0;
    dbtx.commit().await?;
    if inserted {
        tracing::info!(
            broker_order_id = %bo.broker_order_id,
            symbol,
            "materialized externally-placed order"
        );
    }
    Ok(inserted)
}

/// Idempotently ingest one execution: a fills row (UNIQUE execution_id) and
/// its mirrored transaction, stamped with the broker mode. Returns true if
/// it was new.
///
/// Fees/taxes are 0 here: Alpaca's stream/activity payloads don't carry
/// regulatory fees (SEC/TAF arrive as separate FEE activities). Live mode
/// must ingest those before launch — tracked in PLAN.md Phase 2 follow-ups.
async fn ingest_execution(
    pool: &SqlitePool,
    execution: &Execution,
    mode: &'static str,
) -> Result<bool> {
    let order = sqlx::query("SELECT id FROM orders WHERE broker_order_id = ?")
        .bind(&execution.broker_order_id)
        .fetch_optional(pool)
        .await?;
    let Some(order) = order else {
        // Reconciliation materializes orders (ours and external) before
        // fills; reaching this means we have a gap to repair.
        tracing::warn!(
            broker_order_id = %execution.broker_order_id,
            "fill for unknown order — will retry after next reconciliation"
        );
        return Ok(false);
    };
    let order_id: i64 = order.get("id");

    let mut dbtx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO fills (execution_id, order_id, qty, price, ts) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&execution.execution_id)
    .bind(order_id)
    .bind(execution.qty.to_string())
    .bind(execution.price.to_string())
    .bind(execution.ts.to_rfc3339())
    .execute(&mut *dbtx)
    .await?
    .rows_affected()
        > 0;

    if inserted {
        let fill_rowid: i64 = sqlx::query("SELECT id FROM fills WHERE execution_id = ?")
            .bind(&execution.execution_id)
            .fetch_one(&mut *dbtx)
            .await?
            .get("id");
        let new_tx = repo::NewTx {
            kind: match execution.side {
                Side::Buy => rekt_core::portfolio::TxKind::Buy,
                Side::Sell => rekt_core::portfolio::TxKind::Sell,
            },
            symbol: Some(execution.symbol.clone()),
            qty: execution.qty,
            price: execution.price,
            fees: Decimal::ZERO,
            taxes: Decimal::ZERO,
            ts: execution.ts,
            note: format!("fill {}", execution.execution_id),
            source: repo::TxSource::BrokerFill,
            fill_id: Some(fill_rowid),
            mode,
        };
        repo::insert_one(&mut dbtx, &new_tx).await?;
    }
    dbtx.commit().await?;
    Ok(inserted)
}

// ------------------------------------------------------------- engine --

/// Apply a stream event to local state.
pub async fn apply_event(state: &AppState, event: BrokerEvent) -> Result<()> {
    let Some(broker) = &state.broker else {
        return Ok(());
    };
    let mode = broker.mode().as_str();

    match event {
        BrokerEvent::Connected => {
            tracing::info!("broker stream connected — reconciling");
            reconcile(state).await?;
        }
        BrokerEvent::OrderUpdate {
            order,
            event,
            execution,
        } => {
            let bo = order.into_broker_order();
            tracing::info!(client_order_id = %bo.client_order_id, event = %event, "order update");

            let outcome = {
                // Serialize with manual mutations: fill ingestion writes the
                // transaction log too.
                let _guard = state.mutations.lock().await;
                let outcome = apply_broker_order(&state.db, &bo).await?;
                if outcome != ApplyOutcome::Unknown {
                    if let Some(stream_execution) = &execution {
                        let row = sqlx::query(
                            r#"SELECT i.symbol, o.side FROM orders o
                               JOIN instruments i ON i.id = o.instrument_id
                               WHERE o.client_order_id = ?"#,
                        )
                        .bind(&bo.client_order_id)
                        .fetch_optional(&state.db)
                        .await?;
                        if let Some(row) = row {
                            let side = if row.get::<String, _>("side") == "sell" {
                                Side::Sell
                            } else {
                                Side::Buy
                            };
                            let ingested = ingest_execution(
                                &state.db,
                                &Execution {
                                    execution_id: stream_execution.execution_id.clone(),
                                    broker_order_id: bo.broker_order_id.clone(),
                                    symbol: row.get("symbol"),
                                    side,
                                    qty: stream_execution.qty,
                                    price: stream_execution.price,
                                    ts: stream_execution.ts,
                                },
                                mode,
                            )
                            .await?;
                            if ingested {
                                state.live.bump_tx_revision();
                                // A fill changed paper cash/positions — refresh
                                // the cached account so the ticket sizes correctly.
                                refresh_account_cache(state).await;
                            }
                        }
                    }
                }
                outcome
            };
            if outcome == ApplyOutcome::Unknown {
                tracing::warn!(client_order_id = %bo.client_order_id,
                    "update for unknown order — reconciling");
                reconcile(state).await?;
            }
            live::refresh_symbols(state).await?;
            state.trading.bump_orders();
            state.live.mark_dirty();
        }
    }
    Ok(())
}

/// Startup / reconnect / periodic reconciliation:
/// 1. broker mass status → sync our orders, materialize external ones
/// 2. resolve `pending_submit` orders the mass status didn't cover
/// 3. fill backfill (cursor overlaps 1h; `execution_id` dedupe absorbs it)
///
/// Trading unlocks only after this completes. Safe to run concurrently
/// with everything else: order updates are staleness-guarded and fill
/// ingestion is idempotent + serialized via the mutations lock.
pub async fn reconcile(state: &AppState) -> Result<()> {
    let Some(broker) = &state.broker else {
        return Ok(());
    };
    let mode = broker.mode().as_str();

    // (1) Mass status: one HTTP call covers our orders AND externals.
    let mut seen_client_ids = std::collections::HashSet::new();
    match broker.list_orders().await {
        Ok(orders) => {
            for bo in orders {
                seen_client_ids.insert(bo.client_order_id.clone());
                if bo.client_order_id.starts_with("rekt-") {
                    apply_broker_order(&state.db, &bo).await?;
                } else if materialize_external_order(&state.db, &bo, mode).await? {
                    // freshly materialized — bring its status up to date too
                    apply_broker_order(&state.db, &bo).await?;
                }
            }
        }
        Err(e) => tracing::error!(error = %e, "mass status fetch failed"),
    }

    // (2) pending_submit orders the mass status didn't mention: ask
    // individually; if the broker never saw them, mark failed.
    let pending = sqlx::query("SELECT client_order_id FROM orders WHERE status = 'pending_submit'")
        .fetch_all(&state.db)
        .await?;
    for row in pending {
        let client_order_id: String = row.get("client_order_id");
        if seen_client_ids.contains(&client_order_id) {
            continue; // already synced in step 1
        }
        match broker.order_by_client_id(&client_order_id).await {
            Ok(Some(bo)) => {
                apply_broker_order(&state.db, &bo).await?;
            }
            Ok(None) => {
                sqlx::query(
                    "UPDATE orders SET status = 'failed', updated_ts = ?
                     WHERE client_order_id = ? AND status = 'pending_submit'",
                )
                .bind(Utc::now().to_rfc3339())
                .bind(&client_order_id)
                .execute(&state.db)
                .await?;
                tracing::warn!(
                    client_order_id,
                    "pending order never reached broker — marked failed"
                );
            }
            Err(e) => tracing::warn!(client_order_id, error = %e, "pending-order lookup failed"),
        }
    }

    // (3) Fill backfill. Cursor overlaps by an hour: Alpaca's `after` is
    // exclusive, and same-timestamp executions would otherwise be missed
    // forever. Over-fetching is free — execution_id dedupes.
    let last_fill: Option<String> = sqlx::query("SELECT MAX(ts) AS ts FROM fills")
        .fetch_one(&state.db)
        .await?
        .get("ts");
    let after = last_fill
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc) - Duration::hours(1));
    match broker.executions_since(after).await {
        Ok(executions) => {
            let _guard = state.mutations.lock().await;
            let mut ingested_any = false;
            for execution in executions {
                ingested_any |= ingest_execution(&state.db, &execution, mode).await?;
            }
            if ingested_any {
                state.live.bump_tx_revision();
                live::refresh_symbols(state).await?;
            }
        }
        Err(e) => tracing::error!(error = %e, "fill backfill failed"),
    }

    // Prime the cached paper account so the order ticket can size against it
    // (buying power / oversell / position cap) from the first snapshot.
    refresh_account_cache(state).await;
    state.trading.ready.store(true, Ordering::Relaxed);
    state.trading.bump_orders();
    state.live.mark_dirty();
    tracing::info!("reconciliation complete — trading unlocked");
    Ok(())
}

// ----------------------------------------------------------- handlers --

fn broker_or_503(state: &AppState) -> Result<&dyn Broker, ApiError> {
    match &state.broker {
        Some(broker) => Ok(broker.as_ref()),
        None => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no broker configured — set ALPACA_PAPER_KEY and ALPACA_PAPER_SECRET",
        )),
    }
}

fn map_broker_err(e: BrokerError) -> ApiError {
    match e {
        BrokerError::Rejected(msg) => err(StatusCode::UNPROCESSABLE_ENTITY, msg),
        BrokerError::NotFound(what) => err(StatusCode::NOT_FOUND, format!("not found: {what}")),
        BrokerError::RateLimited => err(StatusCode::TOO_MANY_REQUESTS, "broker rate limit hit"),
        BrokerError::Upstream(msg) => err(StatusCode::BAD_GATEWAY, msg),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct TicketInput {
    pub symbol: String,
    pub side: Side,
    pub order_type: rekt_core::orders::OrderType,
    pub qty: Decimal,
    #[serde(default)]
    pub limit_price: Option<Decimal>,
    #[serde(default)]
    pub tif: Option<rekt_core::orders::TimeInForce>,
}

pub async fn submit_order(
    State(state): State<AppState>,
    Json(input): Json<TicketInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    tracing::debug!(
        symbol = %input.symbol,
        side = ?input.side,
        order_type = ?input.order_type,
        qty = %input.qty,
        "POST /api/orders"
    );
    let broker = broker_or_503(&state)?;
    if !state.trading.ready.load(Ordering::Relaxed) {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "trading locked: broker reconciliation has not completed yet",
        ));
    }

    let ticket = OrderTicket {
        // Same one symbol rule as watchlist/alerts — a malformed symbol
        // must fail here, not as a broker rejection after a round-trip.
        symbol: crate::api::validate_symbol(&input.symbol)?,
        side: input.side,
        order_type: input.order_type,
        qty: input.qty,
        limit_price: input.limit_price,
        tif: input.tif.unwrap_or(rekt_core::orders::TimeInForce::Day),
    };
    let mode = broker.mode().as_str();

    // Network calls happen OUTSIDE the mutations lock: the broker account
    // is the equity source for the position cap (the local paper log has
    // no cash deposits).
    let account_equity = match broker.account().await {
        Ok(account) => {
            let equity = account.equity;
            // Reuse this required fetch to refresh the ticket's cached account.
            cache_account(&state, account).await;
            Some(equity)
        }
        Err(e) => {
            tracing::warn!(error = %e, "account fetch failed — position cap will use local basis");
            None
        }
    };

    // Guardrails + intent persistence under the lock (consistent view of
    // the log); the broker submit happens after the lock is released.
    let (order_id, client_order_id, notional) = {
        let _guard = state.mutations.lock().await;
        let txs = repo::fetch_mode_txs(&state.db, mode)
            .await
            .map_err(internal)?;
        let basis = compute_basis(&txs).map_err(|e| {
            err(
                StatusCode::CONFLICT,
                format!("{mode} transaction log inconsistent: {e}"),
            )
        })?;
        let prices = state.live.price_views().await;
        let last_price = prices.get(&ticket.symbol).map(|p| p.price);
        let equity = account_equity.unwrap_or_else(|| {
            basis.cash
                + basis
                    .positions
                    .iter()
                    .filter_map(|(s, p)| prices.get(s).map(|q| q.price * p.qty))
                    .sum::<Decimal>()
        });
        let today = orders_today(&state.db).await.map_err(internal)?;
        let notional = check_ticket(
            &ticket,
            &rekt_core::orders::TicketContext {
                basis: &basis,
                last_price,
                equity,
                orders_today: today,
                realized_today: basis.realized_since(ny_day_start()),
                trading_paused: state.trading.paused.load(Ordering::Relaxed),
            },
            &state.guardrails,
        )
        .map_err(|v| err(StatusCode::UNPROCESSABLE_ENTITY, v.to_string()))?;

        let (order_id, client_order_id) = insert_pending_order(&state.db, &ticket, mode)
            .await
            .map_err(internal)?;
        (order_id, client_order_id, notional)
    };
    state.trading.bump_orders();

    match broker.submit_order(&client_order_id, &ticket).await {
        Ok(bo) => {
            // Staleness-guarded: if the fill stream already advanced this
            // order, the slower REST response won't regress it.
            apply_broker_order(&state.db, &bo).await.map_err(internal)?;
            state.trading.bump_orders();
            state.live.mark_dirty();
            Ok((
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": order_id,
                    "client_order_id": client_order_id,
                    "status": bo.status.as_str(),
                    "est_notional": notional,
                })),
            ))
        }
        Err(BrokerError::Rejected(msg)) => {
            sqlx::query(
                "UPDATE orders SET status = 'rejected', updated_ts = ?
                 WHERE id = ? AND status = 'pending_submit'",
            )
            .bind(Utc::now().to_rfc3339())
            .bind(order_id)
            .execute(&state.db)
            .await
            .map_err(internal)?;
            state.trading.bump_orders();
            state.live.mark_dirty();
            Err(err(StatusCode::UNPROCESSABLE_ENTITY, msg))
        }
        Err(e) => {
            // Unknown outcome (timeout/5xx): leave pending_submit — the
            // deterministic client id makes reconciliation resolve it safely.
            tracing::error!(client_order_id, error = %e, "submit outcome unknown — left pending");
            Err(map_broker_err(e))
        }
    }
}

pub async fn list_orders(State(state): State<AppState>) -> Result<Json<Vec<OrderRow>>, ApiError> {
    Ok(Json(fetch_orders(&state.db, 100).await.map_err(internal)?))
}

pub async fn cancel_order(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    tracing::debug!(order = id, "DELETE /api/orders/:id");
    let broker = broker_or_503(&state)?;
    let row = sqlx::query("SELECT broker_order_id, status FROM orders WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no order {id}")))?;
    let status: String = row.get("status");
    let parsed: OrderStatus = status.parse().map_err(internal)?;
    if parsed.is_terminal() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("order is already {status}"),
        ));
    }
    let broker_order_id: Option<String> = row.get("broker_order_id");
    let Some(broker_order_id) = broker_order_id else {
        return Err(err(
            StatusCode::CONFLICT,
            "order has no broker id yet — reconciliation will resolve it shortly",
        ));
    };

    // Mark in-flight locally first; a racing fill overwrites this via the
    // (non-regressing) apply path, and a failed cancel is corrected by the
    // next reconcile.
    sqlx::query(
        "UPDATE orders SET status = 'pending_cancel', updated_ts = ?
         WHERE id = ? AND status NOT IN ('filled','canceled','rejected','expired','replaced','failed')",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(internal)?;
    state.trading.bump_orders();
    state.live.mark_dirty();
    broker
        .cancel_order(&broker_order_id)
        .await
        .map_err(map_broker_err)?;
    Ok(StatusCode::ACCEPTED)
}

/// Kill switch: cancel everything open at the broker, and mark local open
/// orders pending_cancel so the UI reflects intent even if the stream is
/// down (reconcile finalizes states).
pub async fn cancel_all(State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    tracing::debug!("POST /api/orders/cancel_all (kill switch)");
    let broker = broker_or_503(&state)?;
    broker.cancel_all().await.map_err(map_broker_err)?;
    sqlx::query(
        "UPDATE orders SET status = 'pending_cancel', updated_ts = ?
         WHERE status IN ('submitted','accepted','partially_filled')",
    )
    .bind(Utc::now().to_rfc3339())
    .execute(&state.db)
    .await
    .map_err(internal)?;
    state.trading.bump_orders();
    state.live.mark_dirty();
    Ok(StatusCode::ACCEPTED)
}

#[derive(Debug, serde::Deserialize)]
pub struct PauseInput {
    pub paused: bool,
}

pub async fn set_paused(
    State(state): State<AppState>,
    Json(input): Json<PauseInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    tracing::debug!(paused = input.paused, "POST /api/trading/pause");
    // Persist FIRST — a safety switch that doesn't survive a crash isn't one.
    repo::set_setting(
        &state.db,
        PAUSED_SETTING,
        if input.paused { "true" } else { "false" },
    )
    .await
    .map_err(internal)?;
    state.trading.paused.store(input.paused, Ordering::Relaxed);
    state.live.mark_dirty();
    Ok(Json(serde_json::json!({ "paused": input.paused })))
}

pub async fn account(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let broker = broker_or_503(&state)?;
    let info = broker.account().await.map_err(map_broker_err)?;
    Ok(Json(serde_json::json!({
        "mode": broker.mode().as_str(),
        "cash": info.cash,
        "buying_power": info.buying_power,
        "equity": info.equity,
        "daytrade_count": info.daytrade_count,
    })))
}

/// Trading block for the dashboard snapshot. The orders list is cached by
/// revision so the per-second broadcaster doesn't hit the DB when nothing
/// order-related changed.
pub async fn snapshot_block(state: &AppState) -> serde_json::Value {
    let Some(broker) = &state.broker else {
        return serde_json::json!({ "enabled": false });
    };
    let revision = state.trading.orders_revision.load(Ordering::Relaxed);
    let orders = {
        let cache = state.trading.orders_cache.read().await;
        if cache.0 == revision && !cache.1.is_null() {
            cache.1.clone()
        } else {
            drop(cache);
            let fresh = serde_json::to_value(fetch_orders(&state.db, 20).await.unwrap_or_default())
                .unwrap_or_default();
            *state.trading.orders_cache.write().await = (revision, fresh.clone());
            fresh
        }
    };
    // Paper account + holdings the order ticket can ACTUALLY trade. The
    // headline portfolio stays the (segregated) live tracked one; these let
    // the client size buying power / oversell / position-cap against the
    // account that executes. Account comes from a cache refreshed off the
    // broadcast path; positions are gated on tx_revision (paper fills bump
    // it) so we never re-read the log per tick.
    let tx_rev = state.live.tx_revision();
    let positions = {
        let cache = state.trading.paper_positions_cache.read().await;
        if cache.0 == tx_rev && !cache.1.is_null() {
            cache.1.clone()
        } else {
            drop(cache);
            let pos = paper_positions_json(state, broker.mode().as_str()).await;
            *state.trading.paper_positions_cache.write().await = (tx_rev, pos.clone());
            pos
        }
    };
    let account = state
        .trading
        .account_cache
        .read()
        .await
        .as_ref()
        .map(|a| serde_json::json!({ "cash": a.cash, "equity": a.equity, "buying_power": a.buying_power }));
    serde_json::json!({
        "enabled": true,
        "mode": broker.mode().as_str(),
        "ready": state.trading.ready.load(Ordering::Relaxed),
        "paused": state.trading.paused.load(Ordering::Relaxed),
        "orders": orders,
        "account": account,
        "positions": positions,
    })
}

/// Paper holdings as a `{symbol: qty}` JSON object (zero-qty filtered out),
/// from the paper-mode transaction log. Used only off-tick (gated by the
/// caller on tx_revision).
async fn paper_positions_json(state: &AppState, mode: &str) -> serde_json::Value {
    let txs = match repo::fetch_mode_txs(&state.db, mode).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "paper positions fetch failed");
            return serde_json::json!({});
        }
    };
    match compute_basis(&txs) {
        Ok(basis) => {
            let map: serde_json::Map<String, serde_json::Value> = basis
                .positions
                .iter()
                .filter(|(_, p)| p.qty != Decimal::ZERO)
                .map(|(s, p)| (s.clone(), serde_json::json!(p.qty)))
                .collect();
            serde_json::Value::Object(map)
        }
        Err(_) => serde_json::json!({}),
    }
}

/// Refresh the cached paper account from the broker. Called off the
/// broadcast path (reconcile, fills, order submit) so snapshots read the
/// cache only and never make a network call per tick.
pub async fn refresh_account_cache(state: &AppState) {
    let Some(broker) = &state.broker else {
        return;
    };
    match broker.account().await {
        Ok(acct) => *state.trading.account_cache.write().await = Some(acct),
        Err(e) => tracing::warn!(error = %e, "paper account cache refresh failed"),
    }
}

/// Store an already-fetched account into the cache (submit path reuses the
/// account call it must make anyway).
pub async fn cache_account(state: &AppState, account: AccountInfo) {
    *state.trading.account_cache.write().await = Some(account);
}
