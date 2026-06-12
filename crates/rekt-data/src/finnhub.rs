//! Finnhub REST implementation of [`MarketData`].
//!
//! Free tier: 60 REST calls/min, real-time US quotes. Docs:
//! <https://finnhub.io/docs/api/quote>

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rekt_core::Quote;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{DataError, MarketData};

const BASE_URL: &str = "https://finnhub.io/api/v1";

pub struct Finnhub {
    client: reqwest::Client,
    token: String,
    base_url: String,
}

impl Finnhub {
    pub fn new(token: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            token,
            base_url: BASE_URL.to_string(),
        }
    }

    /// Override the base URL (tests, proxies).
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }
}

/// Finnhub `/quote` response. Field names are Finnhub's single-letter
/// scheme: c=current, d=change, dp=percent change, pc=previous close,
/// t=unix timestamp.
#[derive(Debug, Deserialize)]
struct FinnhubQuote {
    #[serde(with = "rust_decimal::serde::float")]
    c: Decimal,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    d: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    dp: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float")]
    pc: Decimal,
    t: i64,
}

/// Finnhub returns 200 with all-zero fields for unknown symbols; detect
/// that shape rather than trusting the status code.
fn into_quote(symbol: &str, raw: FinnhubQuote) -> Result<Quote, DataError> {
    if raw.c == Decimal::ZERO && raw.t == 0 {
        return Err(DataError::SymbolNotFound(symbol.to_string()));
    }
    Ok(Quote {
        symbol: symbol.to_string(),
        price: raw.c,
        change: raw.d.unwrap_or_default(),
        percent_change: raw.dp.unwrap_or_default(),
        prev_close: raw.pc,
        ts: DateTime::<Utc>::from_timestamp(raw.t, 0)
            .ok_or_else(|| DataError::Upstream(format!("bad timestamp: {}", raw.t)))?,
    })
}

#[async_trait]
impl MarketData for Finnhub {
    fn name(&self) -> &'static str {
        "finnhub"
    }

    async fn quote(&self, symbol: &str) -> Result<Quote, DataError> {
        let url = format!("{}/quote", self.base_url);
        let response = self
            .client
            .get(url)
            .query(&[("symbol", symbol), ("token", &self.token)])
            .send()
            .await
            .map_err(|e| DataError::Upstream(e.to_string()))?;

        match response.status() {
            reqwest::StatusCode::TOO_MANY_REQUESTS => return Err(DataError::RateLimited),
            status if !status.is_success() => {
                return Err(DataError::Upstream(format!("finnhub returned {status}")));
            }
            _ => {}
        }

        let raw: FinnhubQuote = response
            .json()
            .await
            .map_err(|e| DataError::Upstream(e.to_string()))?;
        into_quote(symbol, raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_finnhub_quote_shape() {
        // Captured shape from https://finnhub.io/docs/api/quote
        let raw: FinnhubQuote = serde_json::from_str(
            r#"{"c":261.74,"d":1.69,"dp":0.6499,"h":263.31,"l":260.68,"o":261.07,"pc":260.05,"t":1582641000}"#,
        )
        .unwrap();
        let quote = into_quote("AAPL", raw).unwrap();
        assert_eq!(quote.symbol, "AAPL");
        assert_eq!(quote.price.to_string(), "261.74");
        assert_eq!(quote.prev_close.to_string(), "260.05");
        assert_eq!(quote.ts.timestamp(), 1582641000);
    }

    #[test]
    fn unknown_symbol_all_zero_response_is_not_found() {
        let raw: FinnhubQuote =
            serde_json::from_str(r#"{"c":0,"d":null,"dp":null,"h":0,"l":0,"o":0,"pc":0,"t":0}"#)
                .unwrap();
        assert!(matches!(
            into_quote("NOPE", raw),
            Err(DataError::SymbolNotFound(_))
        ));
    }
}
