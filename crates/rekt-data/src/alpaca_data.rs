//! Alpaca Market Data implementation of [`MarketData`] — the guaranteed
//! fallback provider (PLAN.md §3): a trading account exists anyway, and the
//! free tier includes IEX-feed quotes and split-adjusted daily bars.
//!
//! Docs: <https://docs.alpaca.markets/us/docs/about-market-data-api>

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use rekt_core::splits::SplitEvent;
use rekt_core::{Candle, Quote};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{DataError, MarketData};

pub const DATA_API: &str = "https://data.alpaca.markets";

pub struct AlpacaData {
    client: reqwest::Client,
    base_url: String,
    key: String,
    secret: String,
}

impl AlpacaData {
    pub fn new(key: String, secret: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
            base_url: DATA_API.to_string(),
            key,
            secret,
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<T, DataError> {
        // Credentials are in the request headers, never the logged path/query.
        tracing::debug!(provider = "alpaca", path, "GET");
        let response = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .header("APCA-API-KEY-ID", &self.key)
            .header("APCA-API-SECRET-KEY", &self.secret)
            .query(query)
            .send()
            .await
            .map_err(|e| DataError::Upstream(e.to_string()))?;
        tracing::debug!(provider = "alpaca", path, status = %response.status(), "response");
        match response.status() {
            reqwest::StatusCode::TOO_MANY_REQUESTS => return Err(DataError::RateLimited),
            reqwest::StatusCode::NOT_FOUND => {
                return Err(DataError::SymbolNotFound("(see request)".into()))
            }
            status if !status.is_success() => {
                let body = response.text().await.unwrap_or_default();
                return Err(DataError::Upstream(format!("alpaca data {status}: {body}")));
            }
            _ => {}
        }
        response
            .json()
            .await
            .map_err(|e| DataError::Upstream(format!("bad body: {e}")))
    }
}

#[derive(Debug, Deserialize)]
pub struct AlpacaBar {
    pub t: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::float")]
    pub o: Decimal,
    #[serde(with = "rust_decimal::serde::float")]
    pub h: Decimal,
    #[serde(with = "rust_decimal::serde::float")]
    pub l: Decimal,
    #[serde(with = "rust_decimal::serde::float")]
    pub c: Decimal,
    #[serde(default)]
    pub v: f64, // Alpaca sends volume as a JSON number that can exceed u32
}

impl AlpacaBar {
    pub fn into_candle(self) -> Candle {
        Candle {
            date: self.t.date_naive(),
            open: self.o,
            high: self.h,
            low: self.l,
            close: self.c,
            // Round (saturating) rather than truncate: fractional volumes
            // appear in adjusted bars.
            volume: self.v.round() as i64,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BarsResponse {
    #[serde(default)]
    bars: Option<Vec<AlpacaBar>>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// Subset of GET /v2/stocks/{symbol}/snapshot we use for quotes.
#[derive(Debug, Deserialize)]
pub struct SnapshotResponse {
    #[serde(rename = "latestTrade")]
    pub latest_trade: Option<SnapshotTrade>,
    #[serde(rename = "prevDailyBar")]
    pub prev_daily_bar: Option<AlpacaBar>,
}

#[derive(Debug, Deserialize)]
pub struct SnapshotTrade {
    #[serde(with = "rust_decimal::serde::float")]
    pub p: Decimal,
    pub t: DateTime<Utc>,
}

#[async_trait]
impl MarketData for AlpacaData {
    fn name(&self) -> &'static str {
        "alpaca"
    }

    async fn quote(&self, symbol: &str) -> Result<Quote, DataError> {
        let snapshot: SnapshotResponse = self
            .get(&format!("/v2/stocks/{symbol}/snapshot"), &[])
            .await?;
        let trade = snapshot
            .latest_trade
            .ok_or_else(|| DataError::SymbolNotFound(symbol.to_string()))?;
        let prev_close = snapshot.prev_daily_bar.map(|bar| bar.c).unwrap_or(trade.p);
        Ok(Quote {
            symbol: symbol.to_string(),
            price: trade.p,
            change: trade.p - prev_close,
            percent_change: if prev_close > Decimal::ZERO {
                ((trade.p / prev_close) - Decimal::ONE) * Decimal::ONE_HUNDRED
            } else {
                Decimal::ZERO
            },
            prev_close,
            ts: trade.t,
        })
    }

    /// Split-adjusted daily bars, paginated to exhaustion.
    async fn daily_candles(
        &self,
        symbol: &str,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<Candle>, DataError> {
        let mut candles = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query: Vec<(String, String)> = vec![
                ("timeframe".into(), "1Day".into()),
                ("start".into(), start.to_string()),
                ("end".into(), end.to_string()),
                ("adjustment".into(), "split".into()),
                ("feed".into(), "iex".into()),
                ("limit".into(), "1000".into()),
            ];
            if let Some(token) = &page_token {
                query.push(("page_token".into(), token.clone()));
            }
            let response: BarsResponse = self
                .get(&format!("/v2/stocks/{symbol}/bars"), &query)
                .await?;
            candles.extend(
                response
                    .bars
                    .unwrap_or_default()
                    .into_iter()
                    .map(AlpacaBar::into_candle),
            );
            match response.next_page_token {
                Some(token) => page_token = Some(token),
                None => {
                    tracing::debug!(
                        provider = "alpaca",
                        symbol,
                        %start,
                        %end,
                        bars = candles.len(),
                        "daily candles fetched"
                    );
                    return Ok(candles);
                }
            }
        }
    }

    /// Forward/reverse splits with an ex-date in [start, end], via the
    /// corporate-actions API (free tier). Ratio = new_rate / old_rate.
    async fn splits(
        &self,
        symbols: &[String],
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<SplitEvent>, DataError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        let symbols_param = symbols
            .iter()
            .map(|s| s.to_uppercase())
            .collect::<Vec<_>>()
            .join(",");
        let mut events = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query: Vec<(String, String)> = vec![
                ("symbols".into(), symbols_param.clone()),
                ("types".into(), "forward_split,reverse_split".into()),
                ("start".into(), start.to_string()),
                ("end".into(), end.to_string()),
                ("limit".into(), "1000".into()),
            ];
            if let Some(token) = &page_token {
                query.push(("page_token".into(), token.clone()));
            }
            let response: CorporateActionsResponse =
                self.get("/v1/corporate-actions", &query).await?;
            events.extend(response.corporate_actions.into_events());
            match response.next_page_token {
                Some(token) => page_token = Some(token),
                None => {
                    tracing::debug!(provider = "alpaca", splits = events.len(), "splits fetched");
                    return Ok(events);
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct CorporateActionsResponse {
    corporate_actions: CorporateActions,
    next_page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CorporateActions {
    #[serde(default)]
    forward_splits: Vec<SplitRecord>,
    #[serde(default)]
    reverse_splits: Vec<SplitRecord>,
}

impl CorporateActions {
    fn into_events(self) -> impl Iterator<Item = SplitEvent> {
        self.forward_splits
            .into_iter()
            .chain(self.reverse_splits)
            .filter(|r| r.old_rate > Decimal::ZERO)
            .map(|r| SplitEvent {
                symbol: r.symbol,
                ex_date: r.ex_date,
                // normalize so 1/10 prints "0.1", not "0.10".
                ratio: (r.new_rate / r.old_rate).normalize(),
            })
    }
}

#[derive(Debug, Deserialize)]
struct SplitRecord {
    symbol: String,
    ex_date: NaiveDate,
    #[serde(with = "rust_decimal::serde::float")]
    new_rate: Decimal,
    #[serde(with = "rust_decimal::serde::float")]
    old_rate: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bars_payload() {
        let json = r#"{
            "bars": [
                {"t": "2026-06-10T04:00:00Z", "o": 100.5, "h": 102.0, "l": 99.0, "c": 101.25, "v": 1234567, "n": 10, "vw": 100.9}
            ],
            "symbol": "AAPL",
            "next_page_token": null
        }"#;
        let response: BarsResponse = serde_json::from_str(json).unwrap();
        let candle = response.bars.unwrap().remove(0).into_candle();
        assert_eq!(candle.date.to_string(), "2026-06-10");
        assert_eq!(candle.close.to_string(), "101.25");
        assert_eq!(candle.volume, 1234567);
    }

    #[test]
    fn parses_corporate_actions_into_split_events() {
        // Captured shape from data.alpaca.markets/v1/corporate-actions (NVDA
        // 10:1 forward + a synthetic 1:10 reverse to cover both branches).
        let json = r#"{
            "corporate_actions": {
                "forward_splits": [
                    {"symbol": "NVDA", "ex_date": "2024-06-10", "new_rate": 10, "old_rate": 1}
                ],
                "reverse_splits": [
                    {"symbol": "ZZZ", "ex_date": "2024-03-15", "new_rate": 1, "old_rate": 10}
                ]
            },
            "next_page_token": null
        }"#;
        let resp: CorporateActionsResponse = serde_json::from_str(json).unwrap();
        let events: Vec<SplitEvent> = resp.corporate_actions.into_events().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].symbol, "NVDA");
        assert_eq!(events[0].ratio.to_string(), "10");
        assert_eq!(events[0].ex_date.to_string(), "2024-06-10");
        // Reverse split: 1/10 = 0.1.
        assert_eq!(events[1].ratio.to_string(), "0.1");
    }

    #[test]
    fn missing_split_arrays_default_to_empty() {
        let resp: CorporateActionsResponse =
            serde_json::from_str(r#"{"corporate_actions": {}, "next_page_token": null}"#).unwrap();
        assert_eq!(resp.corporate_actions.into_events().count(), 0);
    }

    #[test]
    fn parses_snapshot_into_quote_fields() {
        let json = r#"{
            "latestTrade": {"p": 170.05, "t": "2026-06-12T15:30:00Z", "s": 100},
            "prevDailyBar": {"t": "2026-06-11T04:00:00Z", "o": 168.0, "h": 169.5, "l": 167.0, "c": 168.5, "v": 1000}
        }"#;
        let snapshot: SnapshotResponse = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.latest_trade.unwrap().p.to_string(), "170.05");
        assert_eq!(snapshot.prev_daily_bar.unwrap().c.to_string(), "168.5");
    }
}
