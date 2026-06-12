//! Alert rules — pure trigger logic for alerts-to-action (PLAN.md §4).
//!
//! An alert states a condition over observable market facts (price, rolling
//! drawdown). Evaluation here is deterministic and I/O-free; the server owns
//! persistence, notification, and the pre-staged ticket that a trigger
//! surfaces. Nothing in this module can place an order.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertCondition {
    /// Last price at or below the threshold ($).
    PriceBelow,
    /// Last price at or above the threshold ($).
    PriceAbove,
    /// Rolling max drawdown at or above the threshold (%).
    DrawdownAbove,
}

impl AlertCondition {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertCondition::PriceBelow => "price_below",
            AlertCondition::PriceAbove => "price_above",
            AlertCondition::DrawdownAbove => "drawdown_above",
        }
    }

    /// True if the condition reads the live price (vs daily closes).
    pub fn needs_price(&self) -> bool {
        matches!(
            self,
            AlertCondition::PriceBelow | AlertCondition::PriceAbove
        )
    }

    /// Human description for notifications and logs: "AAPL <= $170".
    /// Deliberately ASCII: this string travels in an HTTP header (ntfy
    /// Title), and `HeaderValue` rejects non-ASCII — `≤` would make every
    /// push fail. The UI renders its own pretty labels.
    pub fn describe(&self, symbol: &str, threshold: Decimal) -> String {
        match self {
            AlertCondition::PriceBelow => format!("{symbol} <= ${threshold}"),
            AlertCondition::PriceAbove => format!("{symbol} >= ${threshold}"),
            AlertCondition::DrawdownAbove => format!("{symbol} drawdown >= {threshold}%"),
        }
    }
}

impl std::str::FromStr for AlertCondition {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "price_below" => Ok(AlertCondition::PriceBelow),
            "price_above" => Ok(AlertCondition::PriceAbove),
            "drawdown_above" => Ok(AlertCondition::DrawdownAbove),
            other => Err(format!(
                "unknown alert condition: {other} (use price_below, price_above or drawdown_above)"
            )),
        }
    }
}

/// Evaluate one condition against the observed value for its input kind
/// (price for price conditions, rolling drawdown % for drawdown). Returns
/// the observed value when triggered so it can be recorded and reported.
pub fn check(condition: AlertCondition, threshold: Decimal, observed: Decimal) -> Option<Decimal> {
    let hit = match condition {
        AlertCondition::PriceBelow => observed <= threshold,
        AlertCondition::PriceAbove => observed >= threshold,
        AlertCondition::DrawdownAbove => observed >= threshold,
    };
    hit.then_some(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::dec;

    #[test]
    fn price_conditions_trigger_at_and_past_threshold() {
        let t = dec("170");
        assert_eq!(
            check(AlertCondition::PriceBelow, t, dec("169.99")),
            Some(dec("169.99"))
        );
        assert_eq!(
            check(AlertCondition::PriceBelow, t, dec("170")),
            Some(dec("170"))
        );
        assert_eq!(check(AlertCondition::PriceBelow, t, dec("170.01")), None);
        assert_eq!(
            check(AlertCondition::PriceAbove, t, dec("170")),
            Some(dec("170"))
        );
        assert_eq!(check(AlertCondition::PriceAbove, t, dec("169.99")), None);
    }

    #[test]
    fn drawdown_triggers_at_threshold() {
        assert_eq!(
            check(AlertCondition::DrawdownAbove, dec("20"), dec("23.5")),
            Some(dec("23.5"))
        );
        assert_eq!(
            check(AlertCondition::DrawdownAbove, dec("20"), dec("19.9")),
            None
        );
    }

    #[test]
    fn parsing_and_description_round_trip() {
        for s in ["price_below", "price_above", "drawdown_above"] {
            let c: AlertCondition = s.parse().unwrap();
            assert_eq!(c.as_str(), s);
        }
        assert!("price_at".parse::<AlertCondition>().is_err());
        assert_eq!(
            AlertCondition::PriceBelow.describe("AAPL", dec("170")),
            "AAPL <= $170"
        );
        assert_eq!(
            AlertCondition::DrawdownAbove.describe("TSLA", dec("20")),
            "TSLA drawdown >= 20%"
        );
        assert!(AlertCondition::PriceBelow.needs_price());
        assert!(!AlertCondition::DrawdownAbove.needs_price());
    }

    #[test]
    fn descriptions_are_header_safe_ascii() {
        // The description is used as an HTTP header value (ntfy Title);
        // any non-ASCII byte would make every push notification fail.
        for condition in [
            AlertCondition::PriceBelow,
            AlertCondition::PriceAbove,
            AlertCondition::DrawdownAbove,
        ] {
            let desc = condition.describe("BRK.B", dec("123.45"));
            assert!(desc.is_ascii(), "non-ASCII in {desc:?}");
        }
    }
}
