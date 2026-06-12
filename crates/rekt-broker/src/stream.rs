//! Alpaca `trade_updates` websocket: order lifecycle events in real time.
//!
//! Protocol (<https://docs.alpaca.markets/us/docs/websocket-streaming>):
//! connect `wss://{host}/stream`, send auth, then listen to the
//! `trade_updates` stream. Duplicate events are guaranteed on reconnect —
//! the consumer must dedupe (fills by execution_id; see rekt-server).
//!
//! On every (re)connect this emits [`BrokerEvent::Connected`] so the
//! consumer can run reconciliation — never trust local state across a gap.

use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::alpaca::AlpacaOrder;

#[derive(Debug)]
pub enum BrokerEvent {
    /// Stream (re)connected and authenticated — reconcile now.
    Connected,
    /// An order update; `execution` is set for fill / partial_fill events.
    OrderUpdate {
        order: Box<AlpacaOrder>,
        event: String,
        execution: Option<StreamExecution>,
    },
}

#[derive(Debug, Clone)]
pub struct StreamExecution {
    pub execution_id: String,
    pub qty: Decimal,
    pub price: Decimal,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct StreamEnvelope {
    stream: String,
    data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct TradeUpdate {
    event: String,
    order: AlpacaOrder,
    #[serde(default)]
    execution_id: Option<String>,
    #[serde(default)]
    qty: Option<String>,
    #[serde(default)]
    price: Option<String>,
    #[serde(default)]
    timestamp: Option<DateTime<Utc>>,
}

/// Parse one frame. Returns None for non-trade_updates frames (auth acks,
/// listening confirmations).
pub fn parse_frame(text: &str) -> Option<BrokerEvent> {
    let envelope: StreamEnvelope = serde_json::from_str(text).ok()?;
    if envelope.stream != "trade_updates" {
        return None;
    }
    let update: TradeUpdate = serde_json::from_value(envelope.data).ok()?;
    let execution = match update.event.as_str() {
        "fill" | "partial_fill" => {
            let execution_id = update.execution_id?;
            Some(StreamExecution {
                execution_id,
                qty: update.qty.as_deref()?.parse().ok()?,
                price: update.price.as_deref()?.parse().ok()?,
                ts: update.timestamp.unwrap_or_else(Utc::now),
            })
        }
        _ => None,
    };
    Some(BrokerEvent::OrderUpdate {
        order: Box::new(update.order),
        event: update.event,
        execution,
    })
}

const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Run until the event receiver closes.
pub async fn run_trade_updates(
    base_url: String, // e.g. https://paper-api.alpaca.markets
    key: String,
    secret: String,
    events: mpsc::Sender<BrokerEvent>,
) {
    let ws_url = format!("{}/stream", base_url.replacen("https://", "wss://", 1));
    let mut backoff = Duration::from_secs(1);

    loop {
        if events.is_closed() {
            return;
        }
        let (mut ws, _) = match tokio_tungstenite::connect_async(&ws_url).await {
            Ok(ok) => ok,
            Err(e) => {
                tracing::warn!(error = %e, retry_in = ?backoff, "trade_updates connect failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        let auth = serde_json::json!({
            "action": "auth", "key": key, "secret": secret
        });
        let listen = serde_json::json!({
            "action": "listen", "data": { "streams": ["trade_updates"] }
        });
        if ws.send(Message::text(auth.to_string())).await.is_err()
            || ws.send(Message::text(listen.to_string())).await.is_err()
        {
            continue;
        }
        backoff = Duration::from_secs(1);
        tracing::info!("trade_updates stream connected");
        if events.send(BrokerEvent::Connected).await.is_err() {
            return;
        }

        while let Some(frame) = ws.next().await {
            match frame {
                // Alpaca sends both text and binary frames on this stream.
                Ok(Message::Text(text)) => {
                    if let Some(event) = parse_frame(&text) {
                        if events.send(event).await.is_err() {
                            return;
                        }
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        if let Some(event) = parse_frame(text) {
                            if events.send(event).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                Ok(Message::Ping(payload)) => {
                    let _ = ws.send(Message::Pong(payload)).await;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "trade_updates stream error — reconnecting");
                    break;
                }
            }
        }
        tracing::warn!("trade_updates stream closed — reconnecting");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rekt_core::orders::OrderStatus;

    #[test]
    fn parses_fill_event_with_execution() {
        let frame = r#"{
            "stream": "trade_updates",
            "data": {
                "event": "fill",
                "execution_id": "exec-123",
                "qty": "5",
                "price": "170.05",
                "timestamp": "2026-06-12T15:30:00Z",
                "order": {
                    "id": "broker-1",
                    "client_order_id": "rekt-paper-3",
                    "status": "filled",
                    "filled_qty": "5",
                    "filled_avg_price": "170.05"
                }
            }
        }"#;
        let Some(BrokerEvent::OrderUpdate {
            order,
            event,
            execution,
        }) = parse_frame(frame)
        else {
            panic!("expected order update");
        };
        assert_eq!(event, "fill");
        let execution = execution.unwrap();
        assert_eq!(execution.execution_id, "exec-123");
        assert_eq!(execution.price.to_string(), "170.05");
        assert_eq!(order.into_broker_order().status, OrderStatus::Filled);
    }

    #[test]
    fn ignores_auth_acks_and_cancel_has_no_execution() {
        assert!(
            parse_frame(r#"{"stream":"authorization","data":{"status":"authorized"}}"#).is_none()
        );
        let frame = r#"{
            "stream": "trade_updates",
            "data": {
                "event": "canceled",
                "order": { "id": "b1", "client_order_id": "rekt-paper-4", "status": "canceled" }
            }
        }"#;
        let Some(BrokerEvent::OrderUpdate { execution, .. }) = parse_frame(frame) else {
            panic!("expected order update");
        };
        assert!(execution.is_none());
    }
}
