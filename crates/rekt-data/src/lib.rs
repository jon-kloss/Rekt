//! Market data providers behind a single trait.
//!
//! The trait boundary is the design's insurance policy: free tiers get
//! nerfed (PLAN.md §3), so a provider change must be a new impl, not a
//! refactor. Implementations: [`finnhub::Finnhub`]; Alpaca lands in Phase 2.

pub mod alpaca_data;
pub mod finnhub;
pub mod stream;

use async_trait::async_trait;
use chrono::NaiveDate;
use rekt_core::splits::SplitEvent;
use rekt_core::{Candle, Quote};

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error("provider rate limit hit")]
    RateLimited,
    #[error("this provider does not support {0} (free tier or API limitation)")]
    Unsupported(&'static str),
    #[error("upstream error: {0}")]
    Upstream(String),
}

/// A source of market data. Implementations must be cheap to clone or
/// wrapped in `Arc` by callers.
#[async_trait]
pub trait MarketData: Send + Sync {
    /// Provider name for logs and the UI's data-freshness indicators.
    fn name(&self) -> &'static str;

    /// Fetch a current quote for one symbol.
    async fn quote(&self, symbol: &str) -> Result<Quote, DataError>;

    /// Daily OHLCV bars in [start, end], oldest first. Default: unsupported
    /// (Finnhub's free tier dropped candles; Alpaca provides them).
    async fn daily_candles(
        &self,
        _symbol: &str,
        _start: NaiveDate,
        _end: NaiveDate,
    ) -> Result<Vec<Candle>, DataError> {
        Err(DataError::Unsupported("daily candles"))
    }

    /// Forward/reverse stock splits with an ex-date in [start, end] for any of
    /// `symbols` (batched into one request). Default: unsupported (only the
    /// corporate-actions-capable provider implements it).
    async fn splits(
        &self,
        _symbols: &[String],
        _start: NaiveDate,
        _end: NaiveDate,
    ) -> Result<Vec<SplitEvent>, DataError> {
        Err(DataError::Unsupported("corporate splits"))
    }
}
