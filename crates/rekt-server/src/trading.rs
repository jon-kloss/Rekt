//! The order manager: tickets in, broker out, fills back into the
//! transaction log.
//!
//! Invariants (PLAN.md §4, docs/RESEARCH.md §3):
//! - client order ids are deterministic (`rekt-{mode}-{rowid}`) and
//!   persisted BEFORE submission — a crash can never duplicate an order.
//! - fills are idempotent on `execution_id`; ingesting the same execution
//!   twice (stream replay, reconciliation overlap) is a no-op.
//! - no order submission until startup reconciliation has run
//!   (`trading_ready`), and reconciliation re-runs on every stream
//!   reconnect — local state is never trusted across a gap.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use chrono::{DateTime, Utc};
use chrono_tz::America::New_York;
use rekt_broker::{stream::BrokerEvent, Broker, BrokerError, BrokerOrder, Execution};
use rekt_core::orders::{check_ticket, OrderStatus, OrderTicket, Side};
use rekt_core::portfolio::compute_basis;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};

use crate::api::{err, ApiError};
use crate::{live, repo, AppState};

/// Trading control flags, all checked on the submission path.
#[derive(Default)]
pub struct TradingState {
    /// Set once startup reconciliation completes; orders 503 until then.
    pub ready: AtomicBool,
    /// The manual pause switch (and future circuit breakers).
    pub paused: AtomicBool,
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

fn dec(s: String) -> Result<Decimal> {
    s.parse().with_context(|| format!("corrupt decimal {s:?}"))
}

fn dec_opt(s: Option<String>) -> Result<Option<Decimal>> {
    s.map(dec).transpose()
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
                qty: dec(r.get("qty"))?,
                limit_price: dec_opt(r.get("limit_price"))?,
                tif: r.get("tif"),
                status: r.get("status"),
                filled_qty: dec(r.get("filled_qty"))?,
                avg_fill_price: dec_opt(r.get("avg_fill_price"))?,
                mode: r.get("mode"),
                submitted_ts: r.get("submitted_ts"),
                updated_ts: r.get("updated_ts"),
            })
        })
        .collect()
}

/// Orders submitted today (America/New_York calendar day) — daily cap input.
async fn orders_today(pool: &SqlitePool) -> Result<u32> {
    let day_start = Utc::now()
        .with_timezone(&New_York)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_local_timezone(New_York)
        .single()
        .map(|local| local.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let row = sqlx::query("SELECT COUNT(*) AS n FROM orders WHERE submitted_ts >= ?")
        .bind(day_start.to_rfc3339())
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
    let mut dbtx = pool.begin().await?;
    sqlx::query("INSERT OR IGNORE INTO instruments (symbol) VALUES (?)")
        .bind(&ticket.symbol)
        .execute(&mut *dbtx)
        .await?;
    let instrument_id: i64 = sqlx::query("SELECT id FROM instruments WHERE symbol = ?")
        .bind(&ticket.symbol)
        .fetch_one(&mut *dbtx)
        .await?
        .get("id");
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

    // Deterministic, derived from the rowid we just claimed.
    let client_order_id = format!("rekt-{mode}-{id}");
    sqlx::query("UPDATE orders SET client_order_id = ? WHERE id = ?")
        .bind(&client_order_id)
        .bind(id)
        .execute(&mut *dbtx)
        .await?;
    dbtx.commit().await?;
    Ok((id, client_order_id))
}

async fn apply_broker_order(pool: &SqlitePool, bo: &BrokerOrder) -> Result<bool> {
    let result = sqlx::query(
        r#"UPDATE orders
           SET broker_order_id = ?, status = ?, filled_qty = ?, avg_fill_price = ?, updated_ts = ?
           WHERE client_order_id = ?"#,
    )
    .bind(&bo.broker_order_id)
    .bind(bo.status.as_str())
    .bind(bo.filled_qty.to_string())
    .bind(bo.avg_fill_price.map(|p| p.to_string()))
    .bind(bo.updated_ts.unwrap_or_else(Utc::now).to_rfc3339())
    .bind(&bo.client_order_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Idempotently ingest one execution: a fills row (UNIQUE execution_id) and
/// its mirrored transaction. Returns true if it was new.
async fn ingest_execution(pool: &SqlitePool, execution: &Execution) -> Result<bool> {
    let order = sqlx::query("SELECT id, instrument_id FROM orders WHERE broker_order_id = ?")
        .bind(&execution.broker_order_id)
        .fetch_optional(pool)
        .await?;
    let Some(order) = order else {
        // An order we don't know (placed in Alpaca's own UI). Reconciliation
        // materializes orders before fills; if we still miss it, log loudly
        // rather than guessing.
        tracing::warn!(
            broker_order_id = %execution.broker_order_id,
            "fill for unknown order — run reconciliation"
        );
        return Ok(false);
    };
    let order_id: i64 = order.get("id");
    let instrument_id: i64 = order.get("instrument_id");

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
        let kind = match execution.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        sqlx::query(
            r#"INSERT INTO transactions
               (instrument_id, kind, qty, price, fees, taxes, ts, source, fill_id, note)
               VALUES (?, ?, ?, ?, '0', '0', ?, 'broker_fill', ?, ?)"#,
        )
        .bind(instrument_id)
        .bind(kind)
        .bind(execution.qty.to_string())
        .bind(execution.price.to_string())
        .bind(execution.ts.to_rfc3339())
        .bind(fill_rowid)
        .bind(format!("fill {}", execution.execution_id))
        .execute(&mut *dbtx)
        .await?;
    }
    dbtx.commit().await?;
    Ok(inserted)
}

// ------------------------------------------------------------- engine --

/// Apply a stream event to local state. Pure-ish and directly testable.
pub async fn apply_event(state: &AppState, event: BrokerEvent) -> Result<()> {
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
            if !apply_broker_order(&state.db, &bo).await? {
                tracing::warn!(client_order_id = %bo.client_order_id,
                    "update for unknown order — reconciling");
                reconcile(state).await?;
            }
            if let Some(stream_execution) = execution {
                // Resolve symbol/side from our order row via the broker id.
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
                            execution_id: stream_execution.execution_id,
                            broker_order_id: bo.broker_order_id.clone(),
                            symbol: row.get("symbol"),
                            side,
                            qty: stream_execution.qty,
                            price: stream_execution.price,
                            ts: stream_execution.ts,
                        },
                    )
                    .await?;
                    if ingested {
                        state.live.bump_tx_revision();
                        live::refresh_symbols(state).await?;
                    }
                }
            }
            state.live.mark_dirty();
        }
    }
    Ok(())
}

/// Startup / reconnect reconciliation:
/// 1. resolve in-flight `pending_submit` orders (did they reach the broker?)
/// 2. pull broker mass status and sync every known order's state
/// 3. pull fill activities and ingest any we missed (idempotent)
///
/// Trading unlocks only after this completes.
pub async fn reconcile(state: &AppState) -> Result<()> {
    let Some(broker) = &state.broker else {
        return Ok(());
    };

    // (1) In-flight orders from a previous crash/restart.
    let pending = sqlx::query("SELECT client_order_id FROM orders WHERE status = 'pending_submit'")
        .fetch_all(&state.db)
        .await?;
    for row in pending {
        let client_order_id: String = row.get("client_order_id");
        match broker.order_by_client_id(&client_order_id).await {
            Ok(Some(bo)) => {
                apply_broker_order(&state.db, &bo).await?;
            }
            Ok(None) => {
                sqlx::query(
                    "UPDATE orders SET status = 'failed', updated_ts = ? WHERE client_order_id = ?",
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

    // (2) Mass status sync for our orders.
    match broker.list_orders().await {
        Ok(orders) => {
            for bo in orders {
                if bo.client_order_id.starts_with("rekt-") {
                    apply_broker_order(&state.db, &bo).await?;
                } else {
                    tracing::warn!(
                        broker_order_id = %bo.broker_order_id,
                        "order placed outside REKT — tracked at the broker only"
                    );
                }
            }
        }
        Err(e) => tracing::error!(error = %e, "mass status fetch failed"),
    }

    // (3) Fill backfill since our newest known fill.
    let last_fill: Option<String> = sqlx::query("SELECT MAX(ts) AS ts FROM fills")
        .fetch_one(&state.db)
        .await?
        .get("ts");
    let after = last_fill
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc));
    match broker.executions_since(after).await {
        Ok(executions) => {
            let mut ingested_any = false;
            for execution in executions {
                ingested_any |= ingest_execution(&state.db, &execution).await?;
            }
            if ingested_any {
                state.live.bump_tx_revision();
                live::refresh_symbols(state).await?;
            }
        }
        Err(e) => tracing::error!(error = %e, "fill backfill failed"),
    }

    state.trading.ready.store(true, Ordering::Relaxed);
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

fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "internal error");
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
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
    let broker = broker_or_503(&state)?;
    if !state.trading.ready.load(Ordering::Relaxed) {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "trading locked: broker reconciliation has not completed yet",
        ));
    }

    let ticket = OrderTicket {
        symbol: input.symbol.trim().to_uppercase(),
        side: input.side,
        order_type: input.order_type,
        qty: input.qty,
        limit_price: input.limit_price,
        tif: input.tif.unwrap_or(rekt_core::orders::TimeInForce::Day),
    };

    // Guardrails — unconditional, before anything touches the broker.
    let _guard = state.mutations.lock().await;
    let txs = repo::fetch_all_txs(&state.db).await.map_err(internal)?;
    let basis = compute_basis(&txs).map_err(|e| {
        err(
            StatusCode::CONFLICT,
            format!("transaction log inconsistent: {e}"),
        )
    })?;
    let prices = state.live.price_views().await;
    let last_price = prices.get(&ticket.symbol).map(|p| p.price);
    let equity = basis.cash
        + basis
            .positions
            .iter()
            .filter_map(|(s, p)| prices.get(s).map(|q| q.price * p.qty))
            .sum::<Decimal>();
    let today = orders_today(&state.db).await.map_err(internal)?;
    let rails = &state.guardrails;
    let notional = check_ticket(
        &ticket,
        &basis,
        last_price,
        equity,
        rails,
        today,
        state.trading.paused.load(Ordering::Relaxed),
    )
    .map_err(|v| err(StatusCode::UNPROCESSABLE_ENTITY, v.to_string()))?;

    // Persist intent first (deterministic client id), then submit.
    let mode = broker.mode().as_str();
    let (order_id, client_order_id) = insert_pending_order(&state.db, &ticket, mode)
        .await
        .map_err(internal)?;

    match broker.submit_order(&client_order_id, &ticket).await {
        Ok(bo) => {
            apply_broker_order(&state.db, &bo).await.map_err(internal)?;
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
            sqlx::query("UPDATE orders SET status = 'rejected', updated_ts = ? WHERE id = ?")
                .bind(Utc::now().to_rfc3339())
                .bind(order_id)
                .execute(&state.db)
                .await
                .map_err(internal)?;
            state.live.mark_dirty();
            Err(err(StatusCode::UNPROCESSABLE_ENTITY, msg))
        }
        Err(e) => {
            // Unknown outcome (timeout/5xx): leave pending_submit — the
            // deterministic client id makes the retry/reconcile safe, and
            // reconciliation will resolve it either way.
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
            "order has no broker id yet — reconcile first",
        ));
    };

    // Mark in-flight locally first so a racing fill is detected explicitly.
    sqlx::query("UPDATE orders SET status = 'pending_cancel', updated_ts = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    state.live.mark_dirty();
    broker
        .cancel_order(&broker_order_id)
        .await
        .map_err(map_broker_err)?;
    Ok(StatusCode::ACCEPTED)
}

/// Kill switch: cancel everything open at the broker.
pub async fn cancel_all(State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let broker = broker_or_503(&state)?;
    broker.cancel_all().await.map_err(map_broker_err)?;
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
) -> Json<serde_json::Value> {
    state.trading.paused.store(input.paused, Ordering::Relaxed);
    state.live.mark_dirty();
    Json(serde_json::json!({ "paused": input.paused }))
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

/// Trading block for the dashboard snapshot.
pub async fn snapshot_block(state: &AppState) -> serde_json::Value {
    let Some(broker) = &state.broker else {
        return serde_json::json!({ "enabled": false });
    };
    let orders = fetch_orders(&state.db, 20).await.unwrap_or_default();
    serde_json::json!({
        "enabled": true,
        "mode": broker.mode().as_str(),
        "ready": state.trading.ready.load(Ordering::Relaxed),
        "paused": state.trading.paused.load(Ordering::Relaxed),
        "orders": orders,
    })
}
