//! REKT domain types and pure portfolio logic.
//!
//! This crate is deliberately I/O-free: no network, no database, no async
//! runtime. The money math that must be right lives here, where it can be
//! tested exhaustively without mocks.

pub mod market;
pub mod portfolio;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A point-in-time quote for a single instrument.
///
/// Prices are `Decimal`, never floats — see PLAN.md §7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub symbol: String,
    /// Last traded / current price.
    pub price: Decimal,
    /// Absolute change vs. previous close.
    pub change: Decimal,
    /// Percent change vs. previous close.
    pub percent_change: Decimal,
    pub prev_close: Decimal,
    /// Exchange timestamp of the quote.
    pub ts: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn quote_serde_roundtrip_preserves_decimal_precision() {
        let quote = Quote {
            symbol: "AAPL".into(),
            price: dec("170.05"),
            change: dec("-1.10"),
            percent_change: dec("-0.64"),
            prev_close: dec("171.15"),
            ts: DateTime::parse_from_rfc3339("2026-06-12T15:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        let json = serde_json::to_string(&quote).unwrap();
        let back: Quote = serde_json::from_str(&json).unwrap();
        assert_eq!(quote, back);
        // The classic float bug: 170.05 must survive as exactly 170.05.
        assert_eq!(back.price.to_string(), "170.05");
    }
}
