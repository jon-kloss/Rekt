//! Finnhub websocket trade stream with auto-reconnect and dynamic
//! (re)subscription.
//!
//! Protocol (<https://finnhub.io/docs/api/websocket-trades>):
//! - connect `wss://ws.finnhub.io/?token=KEY`
//! - send `{"type":"subscribe","symbol":"AAPL"}` per symbol
//! - receive `{"type":"trade","data":[{"s":"AAPL","p":170.05,"t":1718200000000,"v":10}]}`
//!   and occasional `{"type":"ping"}`

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

/// A single trade print from the live feed.
#[derive(Debug, Clone)]
pub struct Trade {
    pub symbol: String,
    pub price: Decimal,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct WsEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: Vec<WsTrade>,
}

#[derive(Debug, Deserialize)]
struct WsTrade {
    s: String,
    p: f64,
    /// Milliseconds since epoch.
    t: i64,
}

/// Parse one websocket text frame into trades. Non-trade frames (pings,
/// errors) yield an empty vec.
pub fn parse_trades(text: &str) -> Vec<Trade> {
    let Ok(envelope) = serde_json::from_str::<WsEnvelope>(text) else {
        return Vec::new();
    };
    if envelope.kind != "trade" {
        return Vec::new();
    }
    envelope
        .data
        .into_iter()
        .filter_map(|t| {
            Some(Trade {
                symbol: t.s,
                price: Decimal::try_from(t.p).ok()?,
                ts: DateTime::<Utc>::from_timestamp_millis(t.t)?,
            })
        })
        .collect()
}

const WS_URL: &str = "wss://ws.finnhub.io";
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Run the stream until the trade receiver closes. `symbols` is a watch
/// channel of the desired subscription set; changes are diffed into
/// subscribe/unsubscribe messages without reconnecting.
pub async fn run_finnhub_stream(
    token: String,
    mut symbols: watch::Receiver<Vec<String>>,
    trades: mpsc::Sender<Trade>,
) {
    let mut backoff = Duration::from_secs(1);

    loop {
        if trades.is_closed() {
            return;
        }

        let url = format!("{WS_URL}/?token={token}");
        let connection = tokio_tungstenite::connect_async(&url).await;
        let (mut ws, _) = match connection {
            Ok(ok) => ok,
            Err(e) => {
                tracing::warn!(error = %e, retry_in = ?backoff, "finnhub ws connect failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        tracing::info!("finnhub ws connected");

        // Subscribe the current set; track it for diffing on changes.
        let mut subscribed: HashSet<String> = HashSet::new();
        let desired: Vec<String> = symbols.borrow().clone();
        for symbol in desired {
            let msg = format!(r#"{{"type":"subscribe","symbol":"{symbol}"}}"#);
            if ws.send(Message::text(msg)).await.is_err() {
                break;
            }
            subscribed.insert(symbol);
        }

        loop {
            tokio::select! {
                changed = symbols.changed() => {
                    if changed.is_err() {
                        return; // symbol source dropped — shut down
                    }
                    let desired: HashSet<String> =
                        symbols.borrow_and_update().iter().cloned().collect();
                    let mut failed = false;
                    for symbol in desired.difference(&subscribed) {
                        let msg = format!(r#"{{"type":"subscribe","symbol":"{symbol}"}}"#);
                        failed |= ws.send(Message::text(msg)).await.is_err();
                    }
                    for symbol in subscribed.difference(&desired) {
                        let msg = format!(r#"{{"type":"unsubscribe","symbol":"{symbol}"}}"#);
                        failed |= ws.send(Message::text(msg)).await.is_err();
                    }
                    subscribed = desired;
                    if failed {
                        break; // socket is wedged — reconnect
                    }
                }
                frame = ws.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            for trade in parse_trades(&text) {
                                if trades.send(trade).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = ws.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "finnhub ws error — reconnecting");
                            break;
                        }
                        None => {
                            tracing::warn!("finnhub ws closed — reconnecting");
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trade_frame() {
        let trades = parse_trades(
            r#"{"data":[{"c":null,"p":170.05,"s":"AAPL","t":1718200000000,"v":25}],"type":"trade"}"#,
        );
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].symbol, "AAPL");
        assert_eq!(trades[0].price.to_string(), "170.05");
        assert_eq!(trades[0].ts.timestamp_millis(), 1718200000000);
    }

    #[test]
    fn ignores_ping_and_garbage() {
        assert!(parse_trades(r#"{"type":"ping"}"#).is_empty());
        assert!(parse_trades("not json").is_empty());
    }
}
