//! Market data providers behind a single trait.
//!
//! The trait boundary is the design's insurance policy: free tiers get
//! nerfed (PLAN.md §3), so a provider change must be a new impl, not a
//! refactor. Implementations: [`finnhub::Finnhub`]; Alpaca lands in Phase 2.

pub mod finnhub;

use async_trait::async_trait;
use rekt_core::Quote;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error("provider rate limit hit")]
    RateLimited,
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
}
