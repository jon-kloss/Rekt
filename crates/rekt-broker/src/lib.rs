//! Broker abstraction: the boundary the type system enforces.
//!
//! Mirrors the `MarketData` trait design (PLAN.md §4): Alpaca first, any
//! future broker is a new impl. Notably, `rekt-analyst` (Phase 5) must
//! never depend on this crate — the AI has no code path to execution.

pub mod alpaca;
pub mod stream;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rekt_core::orders::{OrderStatus, OrderTicket};
use rust_decimal::Decimal;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeMode {
    Paper,
    Live,
}

impl TradeMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TradeMode::Paper => "paper",
            TradeMode::Live => "live",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("broker rejected the request: {0}")]
    Rejected(String),
    #[error("order not found: {0}")]
    NotFound(String),
    #[error("broker rate limit hit")]
    RateLimited,
    #[error("upstream error: {0}")]
    Upstream(String),
}

/// Broker's view of an order (returned by submit and mass-status queries).
#[derive(Debug, Clone)]
pub struct BrokerOrder {
    pub broker_order_id: String,
    pub client_order_id: String,
    pub status: OrderStatus,
    pub filled_qty: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub updated_ts: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountInfo {
    pub cash: Decimal,
    pub buying_power: Decimal,
    pub equity: Decimal,
    /// Day-trade count in the rolling 5-day window (PDT rule visibility).
    pub daytrade_count: i64,
}

/// One execution against an order (from reconciliation / activities).
#[derive(Debug, Clone)]
pub struct Execution {
    pub execution_id: String,
    pub broker_order_id: String,
    pub symbol: String,
    pub side: rekt_core::orders::Side,
    pub qty: Decimal,
    pub price: Decimal,
    pub ts: DateTime<Utc>,
}

#[async_trait]
pub trait Broker: Send + Sync {
    fn name(&self) -> &'static str;
    fn mode(&self) -> TradeMode;

    /// Submit an order carrying our deterministic `client_order_id`.
    async fn submit_order(
        &self,
        client_order_id: &str,
        ticket: &OrderTicket,
    ) -> Result<BrokerOrder, BrokerError>;

    async fn cancel_order(&self, broker_order_id: &str) -> Result<(), BrokerError>;

    /// Kill switch: cancel every open order.
    async fn cancel_all(&self) -> Result<(), BrokerError>;

    /// Look up one order by our client id (crash-recovery: did a
    /// pending_submit order actually reach the broker?).
    async fn order_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<BrokerOrder>, BrokerError>;

    /// Mass status for reconciliation: all orders the broker knows about,
    /// newest first.
    async fn list_orders(&self) -> Result<Vec<BrokerOrder>, BrokerError>;

    /// All fills since `after` (reconciliation; dedupe by execution_id).
    async fn executions_since(
        &self,
        after: Option<DateTime<Utc>>,
    ) -> Result<Vec<Execution>, BrokerError>;

    async fn account(&self) -> Result<AccountInfo, BrokerError>;
}
