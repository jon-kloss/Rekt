//! Alpaca Trading API implementation of [`Broker`].
//!
//! Paper and live are API-identical — same code, different base URL + keys
//! (<https://docs.alpaca.markets/us/docs/working-with-orders>). This impl is
//! written against the raw HTTP API rather than the GPL `apca` crate
//! (docs/RESEARCH.md §7): owning the order-lifecycle code is the point.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rekt_core::orders::{OrderStatus, OrderTicket, Side};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{AccountInfo, Broker, BrokerError, BrokerOrder, Execution, TradeMode};

pub const PAPER_API: &str = "https://paper-api.alpaca.markets";
pub const LIVE_API: &str = "https://api.alpaca.markets";

pub struct Alpaca {
    client: reqwest::Client,
    base_url: String,
    key: String,
    secret: String,
    mode: TradeMode,
}

impl Alpaca {
    pub fn paper(key: String, secret: String) -> Self {
        Self::new(PAPER_API.to_string(), key, secret, TradeMode::Paper)
    }

    pub fn new(base_url: String, key: String, secret: String, mode: TradeMode) -> Self {
        Self {
            // A broker call that hangs must not hang REKT (see PR #4 review:
            // submit paths and reconciliation depend on bounded latency).
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
            base_url,
            key,
            secret,
            mode,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{}", self.base_url, path))
            .header("APCA-API-KEY-ID", &self.key)
            .header("APCA-API-SECRET-KEY", &self.secret)
    }

    async fn handle<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, BrokerError> {
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(BrokerError::RateLimited);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(BrokerError::NotFound("resource".into()));
        }
        let body = response
            .text()
            .await
            .map_err(|e| BrokerError::Upstream(e.to_string()))?;
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
            || status == reqwest::StatusCode::FORBIDDEN
        {
            // Alpaca puts the human-readable reason in {"message": ...}.
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(String::from))
                .unwrap_or(body);
            return Err(BrokerError::Rejected(msg));
        }
        if !status.is_success() {
            return Err(BrokerError::Upstream(format!("alpaca {status}: {body}")));
        }
        serde_json::from_str(&body).map_err(|e| BrokerError::Upstream(format!("bad body: {e}")))
    }
}

/// Alpaca's order JSON (subset we use).
#[derive(Debug, Deserialize)]
pub struct AlpacaOrder {
    pub id: String,
    pub client_order_id: String,
    pub status: String,
    #[serde(default)]
    pub filled_qty: Option<String>,
    #[serde(default)]
    pub filled_avg_price: Option<String>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    // Descriptive fields — present on order objects, used to materialize
    // externally-placed orders during reconciliation.
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub qty: Option<String>,
    #[serde(default, rename = "type")]
    pub order_type: Option<String>,
    #[serde(default)]
    pub limit_price: Option<String>,
    #[serde(default)]
    pub time_in_force: Option<String>,
}

/// Map Alpaca's order status vocabulary onto our state machine.
/// <https://docs.alpaca.markets/us/docs/orders-at-alpaca>
pub fn map_status(s: &str) -> OrderStatus {
    match s {
        "new"
        | "accepted"
        | "pending_new"
        | "accepted_for_bidding"
        | "done_for_day"
        | "stopped"
        | "calculated" => OrderStatus::Accepted,
        "partially_filled" => OrderStatus::PartiallyFilled,
        "filled" => OrderStatus::Filled,
        "canceled" => OrderStatus::Canceled,
        "expired" => OrderStatus::Expired,
        "replaced" => OrderStatus::Replaced,
        "rejected" => OrderStatus::Rejected,
        "pending_cancel" => OrderStatus::PendingCancel,
        "pending_replace" => OrderStatus::PendingCancel, // closest local state
        other => {
            tracing::warn!(
                status = other,
                "unknown alpaca order status — treating as accepted"
            );
            OrderStatus::Accepted
        }
    }
}

fn parse_dec_opt(s: &Option<String>) -> Option<Decimal> {
    s.as_deref().and_then(|v| v.parse().ok())
}

impl AlpacaOrder {
    pub fn into_broker_order(self) -> BrokerOrder {
        BrokerOrder {
            status: map_status(&self.status),
            filled_qty: parse_dec_opt(&self.filled_qty).unwrap_or_default(),
            avg_fill_price: parse_dec_opt(&self.filled_avg_price),
            updated_ts: self.updated_at,
            symbol: self.symbol,
            side: match self.side.as_deref() {
                Some("buy") => Some(Side::Buy),
                Some("sell") | Some("sell_short") => Some(Side::Sell),
                _ => None,
            },
            qty: parse_dec_opt(&self.qty),
            order_type: self.order_type,
            limit_price: parse_dec_opt(&self.limit_price),
            tif: self.time_in_force,
            broker_order_id: self.id,
            client_order_id: self.client_order_id,
        }
    }
}

#[derive(Debug, Deserialize)]
struct AlpacaAccount {
    cash: String,
    buying_power: String,
    equity: String,
    #[serde(default)]
    daytrade_count: i64,
}

/// FILL entry from GET /v2/account/activities.
#[derive(Debug, Deserialize)]
pub struct AlpacaActivity {
    pub id: String,
    pub order_id: String,
    pub symbol: String,
    pub side: String,
    pub qty: String,
    pub price: String,
    pub transaction_time: DateTime<Utc>,
}

impl AlpacaActivity {
    pub fn into_execution(self) -> Option<Execution> {
        Some(Execution {
            side: match self.side.as_str() {
                "buy" => Side::Buy,
                "sell" | "sell_short" => Side::Sell,
                _ => return None,
            },
            qty: self.qty.parse().ok()?,
            price: self.price.parse().ok()?,
            execution_id: self.id,
            broker_order_id: self.order_id,
            symbol: self.symbol,
            ts: self.transaction_time,
        })
    }
}

#[async_trait]
impl Broker for Alpaca {
    fn name(&self) -> &'static str {
        "alpaca"
    }

    fn mode(&self) -> TradeMode {
        self.mode
    }

    async fn submit_order(
        &self,
        client_order_id: &str,
        ticket: &OrderTicket,
    ) -> Result<BrokerOrder, BrokerError> {
        let mut body = serde_json::json!({
            "symbol": ticket.symbol,
            "qty": ticket.qty.to_string(),
            "side": ticket.side.as_str(),
            "type": ticket.order_type.as_str(),
            "time_in_force": ticket.tif.as_str(),
            "client_order_id": client_order_id,
        });
        if let Some(limit) = ticket.limit_price {
            body["limit_price"] = serde_json::Value::String(limit.to_string());
        }
        tracing::debug!(
            mode = ?self.mode,
            client_order_id,
            symbol = %ticket.symbol,
            qty = %ticket.qty,
            side = ticket.side.as_str(),
            order_type = ticket.order_type.as_str(),
            "submit_order → POST /v2/orders"
        );
        let response = self
            .request(reqwest::Method::POST, "/v2/orders")
            .json(&body)
            .send()
            .await
            .map_err(|e| BrokerError::Upstream(e.to_string()))?;
        let order: AlpacaOrder = Self::handle(response).await?;
        tracing::debug!(
            client_order_id,
            broker_order_id = %order.id,
            status = %order.status,
            "submit_order accepted"
        );
        Ok(order.into_broker_order())
    }

    async fn cancel_order(&self, broker_order_id: &str) -> Result<(), BrokerError> {
        tracing::debug!(broker_order_id, "cancel_order → DELETE /v2/orders/:id");
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/v2/orders/{broker_order_id}"),
            )
            .send()
            .await
            .map_err(|e| BrokerError::Upstream(e.to_string()))?;
        match response.status() {
            s if s.is_success() => Ok(()),
            reqwest::StatusCode::NOT_FOUND => {
                Err(BrokerError::NotFound(broker_order_id.to_string()))
            }
            reqwest::StatusCode::UNPROCESSABLE_ENTITY => Err(BrokerError::Rejected(
                "order is no longer cancelable".into(),
            )),
            s => Err(BrokerError::Upstream(format!("alpaca {s}"))),
        }
    }

    async fn cancel_all(&self) -> Result<(), BrokerError> {
        tracing::debug!("cancel_all → DELETE /v2/orders");
        let response = self
            .request(reqwest::Method::DELETE, "/v2/orders")
            .send()
            .await
            .map_err(|e| BrokerError::Upstream(e.to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(BrokerError::Upstream(format!(
                "alpaca {}",
                response.status()
            )))
        }
    }

    async fn order_by_client_id(
        &self,
        client_order_id: &str,
    ) -> Result<Option<BrokerOrder>, BrokerError> {
        tracing::debug!(client_order_id, "order_by_client_id");
        let response = self
            .request(reqwest::Method::GET, "/v2/orders:by_client_order_id")
            .query(&[("client_order_id", client_order_id)])
            .send()
            .await
            .map_err(|e| BrokerError::Upstream(e.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            tracing::debug!(client_order_id, "order_by_client_id: not found");
            return Ok(None);
        }
        let order: AlpacaOrder = Self::handle(response).await?;
        Ok(Some(order.into_broker_order()))
    }

    async fn list_orders(&self) -> Result<Vec<BrokerOrder>, BrokerError> {
        let response = self
            .request(reqwest::Method::GET, "/v2/orders")
            .query(&[("status", "all"), ("limit", "500"), ("direction", "desc")])
            .send()
            .await
            .map_err(|e| BrokerError::Upstream(e.to_string()))?;
        let orders: Vec<AlpacaOrder> = Self::handle(response).await?;
        tracing::debug!(count = orders.len(), "list_orders");
        Ok(orders
            .into_iter()
            .map(AlpacaOrder::into_broker_order)
            .collect())
    }

    async fn executions_since(
        &self,
        after: Option<DateTime<Utc>>,
    ) -> Result<Vec<Execution>, BrokerError> {
        const PAGE_SIZE: usize = 100;
        let mut executions = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query: Vec<(String, String)> = vec![
                ("activity_types".into(), "FILL".into()),
                ("page_size".into(), PAGE_SIZE.to_string()),
                ("direction".into(), "asc".into()),
            ];
            if let Some(after) = after {
                query.push(("after".into(), after.to_rfc3339()));
            }
            if let Some(token) = &page_token {
                query.push(("page_token".into(), token.clone()));
            }
            let response = self
                .request(reqwest::Method::GET, "/v2/account/activities")
                .query(&query)
                .send()
                .await
                .map_err(|e| BrokerError::Upstream(e.to_string()))?;
            let activities: Vec<AlpacaActivity> = Self::handle(response).await?;
            let page_len = activities.len();
            page_token = activities.last().map(|a| a.id.clone());
            executions.extend(
                activities
                    .into_iter()
                    .filter_map(AlpacaActivity::into_execution),
            );
            // A short page means we've drained the range.
            if page_len < PAGE_SIZE || page_token.is_none() {
                tracing::debug!(
                    after = ?after,
                    fills = executions.len(),
                    "executions_since drained"
                );
                return Ok(executions);
            }
        }
    }

    async fn account(&self) -> Result<AccountInfo, BrokerError> {
        tracing::debug!("account → GET /v2/account");
        let response = self
            .request(reqwest::Method::GET, "/v2/account")
            .send()
            .await
            .map_err(|e| BrokerError::Upstream(e.to_string()))?;
        let account: AlpacaAccount = Self::handle(response).await?;
        let parse = |s: &str, what: &str| {
            s.parse::<Decimal>()
                .map_err(|_| BrokerError::Upstream(format!("bad {what}: {s}")))
        };
        let info = AccountInfo {
            cash: parse(&account.cash, "cash")?,
            buying_power: parse(&account.buying_power, "buying_power")?,
            equity: parse(&account.equity, "equity")?,
            daytrade_count: account.daytrade_count,
        };
        tracing::debug!(
            equity = %info.equity,
            buying_power = %info.buying_power,
            daytrade_count = info.daytrade_count,
            "account fetched"
        );
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alpaca_order_and_maps_status() {
        let json = r#"{
            "id": "61e69015-8549-4bfd-b9c3-01e75843f47d",
            "client_order_id": "rekt-paper-7",
            "status": "partially_filled",
            "filled_qty": "3",
            "filled_avg_price": "100.25",
            "updated_at": "2026-06-12T15:00:00Z"
        }"#;
        let order: AlpacaOrder = serde_json::from_str(json).unwrap();
        let bo = order.into_broker_order();
        assert_eq!(bo.status, OrderStatus::PartiallyFilled);
        assert_eq!(bo.filled_qty.to_string(), "3");
        assert_eq!(bo.avg_fill_price.unwrap().to_string(), "100.25");
        assert_eq!(bo.client_order_id, "rekt-paper-7");
    }

    #[test]
    fn maps_every_documented_alpaca_status() {
        for (alpaca, ours) in [
            ("new", OrderStatus::Accepted),
            ("pending_new", OrderStatus::Accepted),
            ("partially_filled", OrderStatus::PartiallyFilled),
            ("filled", OrderStatus::Filled),
            ("canceled", OrderStatus::Canceled),
            ("expired", OrderStatus::Expired),
            ("replaced", OrderStatus::Replaced),
            ("rejected", OrderStatus::Rejected),
            ("pending_cancel", OrderStatus::PendingCancel),
            ("done_for_day", OrderStatus::Accepted),
        ] {
            assert_eq!(map_status(alpaca), ours, "status {alpaca}");
        }
    }

    #[test]
    fn parses_fill_activity() {
        let json = r#"{
            "id": "20260612000000000::abc",
            "order_id": "61e69015-8549-4bfd-b9c3-01e75843f47d",
            "symbol": "AAPL",
            "side": "buy",
            "qty": "5",
            "price": "170.05",
            "transaction_time": "2026-06-12T15:30:00Z",
            "activity_type": "FILL"
        }"#;
        let activity: AlpacaActivity = serde_json::from_str(json).unwrap();
        let execution = activity.into_execution().unwrap();
        assert_eq!(execution.execution_id, "20260612000000000::abc");
        assert_eq!(execution.price.to_string(), "170.05");
        assert_eq!(execution.side, Side::Buy);
    }
}
