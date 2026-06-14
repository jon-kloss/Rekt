//! Deterministic signal screener: turn a symbol's [`SignalSummary`] into a
//! ranked buy/sell CANDIDATE, tuned by an aggressiveness level. Pure and
//! explainable — the AI layer narrates these candidates, it never invents
//! them, so a small/local model is enough. No market data is fabricated:
//! missing signals simply don't contribute a reason.

use rust_decimal::Decimal;
use serde::Serialize;

use crate::signals::SignalSummary;

/// How eagerly the screener surfaces candidates. Looser thresholds (more
/// aggressive) surface thinner setups and chase momentum; tighter thresholds
/// only flag clear extremes. Default is [`Aggressiveness::Balanced`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggressiveness {
    Conservative,
    Balanced,
    Aggressive,
}

impl Aggressiveness {
    /// Parse a stored setting; anything unrecognized falls back to Balanced.
    pub fn parse_or_balanced(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "conservative" => Self::Conservative,
            "aggressive" => Self::Aggressive,
            _ => Self::Balanced,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::Aggressive => "aggressive",
        }
    }

    /// RSI at or below this reads as an oversold BUY setup.
    fn buy_rsi(&self) -> Decimal {
        Decimal::from(match self {
            Self::Conservative => 30,
            Self::Balanced => 38,
            Self::Aggressive => 45,
        })
    }

    /// RSI at or above this reads as an overbought SELL setup.
    fn sell_rsi(&self) -> Decimal {
        Decimal::from(match self {
            Self::Conservative => 75,
            Self::Balanced => 68,
            Self::Aggressive => 62,
        })
    }

    /// % above the 50-day SMA that reads as "extended" (trim/sell lean).
    fn extended_pct(&self) -> Decimal {
        Decimal::from(match self {
            Self::Conservative => 25,
            Self::Balanced => 18,
            Self::Aggressive => 12,
        })
    }

    /// Whether momentum/breakout buys count (Conservative only buys dips/oversold).
    fn momentum_ok(&self) -> bool {
        !matches!(self, Self::Conservative)
    }
}

/// A screened idea: which side, a score for ranking, and the plain-signal
/// reasons behind it (what the AI is asked to turn into a thesis).
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    /// "buy" or "sell".
    pub action: &'static str,
    pub score: Decimal,
    pub reasons: Vec<String>,
}

fn whole(d: Decimal) -> Decimal {
    d.round_dp(0)
}

/// Score a symbol's signals into a buy/sell candidate, or `None` if nothing
/// crosses the (aggressiveness-tuned) bar. The stronger side wins.
pub fn screen(s: &SignalSummary, aggr: Aggressiveness) -> Option<Candidate> {
    let (rsi, v50, v200, dd) = (s.rsi14, s.vs_sma50_pct, s.vs_sma200_pct, s.drawdown_pct);
    let zero = Decimal::ZERO;

    let mut buy = zero;
    let mut buy_reasons: Vec<String> = Vec::new();
    let mut sell = zero;
    let mut sell_reasons: Vec<String> = Vec::new();

    // --- BUY side ---
    if let Some(r) = rsi {
        if r <= aggr.buy_rsi() {
            buy += aggr.buy_rsi() - r;
            buy_reasons.push(format!("RSI {} oversold", whole(r)));
        }
    }
    if let (Some(a), Some(b)) = (v50, v200) {
        if a < zero && b > zero {
            // pullback inside an uptrend — a quality dip, capped so a crash
            // doesn't outscore a clean setup
            buy += (-a).min(Decimal::from(20));
            buy_reasons.push(format!(
                "{}% below 50d but above 200d (uptrend dip)",
                whole(-a)
            ));
        }
    }
    if aggr.momentum_ok() {
        if let (Some(a), Some(b), Some(r)) = (v50, v200, rsi) {
            if a > zero && b > zero && r >= Decimal::from(50) && r < aggr.sell_rsi() {
                buy += Decimal::from(6);
                buy_reasons.push(format!("uptrend (above 50d & 200d), RSI {}", whole(r)));
            }
        }
    }

    // --- SELL side ---
    if let Some(r) = rsi {
        if r >= aggr.sell_rsi() {
            sell += r - aggr.sell_rsi();
            sell_reasons.push(format!("RSI {} overbought", whole(r)));
        }
    }
    if let Some(a) = v50 {
        if a >= aggr.extended_pct() {
            sell += a - aggr.extended_pct();
            sell_reasons.push(format!("{}% above 50d (extended)", whole(a)));
        }
    }
    if let (Some(a), Some(b)) = (v50, v200) {
        if a < zero && b < Decimal::from(-10) {
            sell += (-b) - Decimal::from(10);
            sell_reasons.push("below 50d & 200d (downtrend)".to_string());
        }
    }
    if let (Some(d), Some(b)) = (dd, v200) {
        if d >= Decimal::from(30) && b < zero {
            // deep drawdown in a downtrend — a cut-loser lean, weighted modestly
            sell += (d - Decimal::from(30)) / Decimal::from(4);
            sell_reasons.push(format!("{}% drawdown in a downtrend", whole(d)));
        }
    }

    // The stronger side wins; require a minimal score so noise doesn't surface.
    let min = Decimal::ONE;
    if buy >= sell && buy >= min {
        Some(Candidate {
            action: "buy",
            score: buy.round_dp(1),
            reasons: buy_reasons,
        })
    } else if sell > buy && sell >= min {
        Some(Candidate {
            action: "sell",
            score: sell.round_dp(1),
            reasons: sell_reasons,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(rsi: i64, v50: i64, v200: i64, dd: i64) -> SignalSummary {
        SignalSummary {
            vs_sma20_pct: None,
            vs_sma50_pct: Some(Decimal::from(v50)),
            vs_sma200_pct: Some(Decimal::from(v200)),
            rsi14: Some(Decimal::from(rsi)),
            drawdown_pct: Some(Decimal::from(dd)),
        }
    }

    #[test]
    fn oversold_is_a_buy() {
        let c = screen(&sig(25, -5, 10, 12), Aggressiveness::Balanced).unwrap();
        assert_eq!(c.action, "buy");
        assert!(c.reasons.iter().any(|r| r.contains("RSI 25 oversold")));
        // pullback-in-uptrend reason also fires (v50<0, v200>0)
        assert!(c.reasons.iter().any(|r| r.contains("uptrend dip")));
    }

    #[test]
    fn overbought_and_extended_is_a_sell() {
        let c = screen(&sig(78, 30, 20, 2), Aggressiveness::Balanced).unwrap();
        assert_eq!(c.action, "sell");
        assert!(c.reasons.iter().any(|r| r.contains("overbought")));
        assert!(c.reasons.iter().any(|r| r.contains("extended")));
    }

    #[test]
    fn aggressiveness_widens_the_oversold_threshold() {
        // RSI 42: not oversold at Conservative (<=30) or Balanced (<=38),
        // but is at Aggressive (<=45).
        let neutral_trend = sig(42, 3, 6, 5); // mild uptrend, no dip
        assert!(screen(&neutral_trend, Aggressiveness::Conservative).is_none());
        let aggr = screen(&neutral_trend, Aggressiveness::Aggressive).unwrap();
        assert_eq!(aggr.action, "buy");
    }

    #[test]
    fn conservative_ignores_pure_momentum() {
        // Healthy uptrend, RSI 58, no oversold/overbought: a momentum buy only
        // at Balanced+, nothing at Conservative.
        let momo = sig(58, 8, 15, 3);
        assert!(screen(&momo, Aggressiveness::Conservative).is_none());
        assert_eq!(
            screen(&momo, Aggressiveness::Balanced).unwrap().action,
            "buy"
        );
    }

    #[test]
    fn nothing_notable_yields_no_candidate() {
        // RSI 45 (not oversold, below the 50 momentum line), barely above the
        // MAs (no dip), shallow drawdown — nothing to say.
        assert!(screen(&sig(45, 1, 2, 8), Aggressiveness::Balanced).is_none());
    }

    #[test]
    fn missing_signals_dont_panic() {
        let empty = SignalSummary {
            vs_sma20_pct: None,
            vs_sma50_pct: None,
            vs_sma200_pct: None,
            rsi14: None,
            drawdown_pct: None,
        };
        assert!(screen(&empty, Aggressiveness::Aggressive).is_none());
    }
}
