//! Quant signal engine — deterministic indicators over daily closes.
//!
//! These are facts, not predictions (PLAN.md §5 layer 1): they state what
//! *is* and feed both the UI badges and, later, the AI analyst's inputs.

use rust_decimal::Decimal;
use serde::Serialize;

/// Simple moving average of the last `n` closes.
pub fn sma(closes: &[Decimal], n: usize) -> Option<Decimal> {
    if n == 0 || closes.len() < n {
        return None;
    }
    let sum: Decimal = closes[closes.len() - n..].iter().sum();
    Some(sum / Decimal::from(n as u64))
}

/// Wilder-smoothed RSI over `n` periods (classically 14).
pub fn rsi(closes: &[Decimal], n: usize) -> Option<Decimal> {
    if n == 0 || closes.len() < n + 1 {
        return None;
    }
    let mut avg_gain = Decimal::ZERO;
    let mut avg_loss = Decimal::ZERO;
    for window in closes[..n + 1].windows(2) {
        let change = window[1] - window[0];
        if change > Decimal::ZERO {
            avg_gain += change;
        } else {
            avg_loss -= change;
        }
    }
    let n_dec = Decimal::from(n as u64);
    avg_gain /= n_dec;
    avg_loss /= n_dec;

    for window in closes[n..].windows(2) {
        let change = window[1] - window[0];
        let (gain, loss) = if change > Decimal::ZERO {
            (change, Decimal::ZERO)
        } else {
            (Decimal::ZERO, -change)
        };
        avg_gain = (avg_gain * (n_dec - Decimal::ONE) + gain) / n_dec;
        avg_loss = (avg_loss * (n_dec - Decimal::ONE) + loss) / n_dec;
    }

    if avg_loss == Decimal::ZERO {
        return Some(Decimal::ONE_HUNDRED);
    }
    let rs = avg_gain / avg_loss;
    Some(Decimal::ONE_HUNDRED - Decimal::ONE_HUNDRED / (Decimal::ONE + rs))
}

/// Maximum peak-to-trough drawdown over the series, as a positive percent.
pub fn max_drawdown_pct(closes: &[Decimal]) -> Option<Decimal> {
    let first = *closes.first()?;
    let mut peak = first;
    let mut worst = Decimal::ZERO;
    for &close in closes {
        if close > peak {
            peak = close;
        } else if peak > Decimal::ZERO {
            let dd = (peak - close) / peak * Decimal::ONE_HUNDRED;
            if dd > worst {
                worst = dd;
            }
        }
    }
    Some(worst)
}

/// Per-symbol signal summary for the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct SignalSummary {
    /// % distance of last close from each SMA (positive = above).
    pub vs_sma20_pct: Option<Decimal>,
    pub vs_sma50_pct: Option<Decimal>,
    pub vs_sma200_pct: Option<Decimal>,
    pub rsi14: Option<Decimal>,
    /// Max drawdown over the supplied window, positive percent.
    pub drawdown_pct: Option<Decimal>,
}

pub fn summarize(closes: &[Decimal]) -> SignalSummary {
    let last = closes.last().copied();
    let vs = |n: usize| -> Option<Decimal> {
        let last = last?;
        let sma = sma(closes, n)?;
        (sma > Decimal::ZERO).then(|| ((last / sma) - Decimal::ONE) * Decimal::ONE_HUNDRED)
    };
    SignalSummary {
        vs_sma20_pct: vs(20).map(|v| v.round_dp(1)),
        vs_sma50_pct: vs(50).map(|v| v.round_dp(1)),
        vs_sma200_pct: vs(200).map(|v| v.round_dp(1)),
        rsi14: rsi(closes, 14).map(|v| v.round_dp(0)),
        drawdown_pct: max_drawdown_pct(closes).map(|v| v.round_dp(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::dec;

    fn series(values: &[i64]) -> Vec<Decimal> {
        values.iter().map(|v| Decimal::from(*v)).collect()
    }

    #[test]
    fn sma_basics_and_insufficient_data() {
        let closes = series(&[1, 2, 3, 4, 5]);
        assert_eq!(sma(&closes, 5).unwrap(), dec("3"));
        assert_eq!(sma(&closes, 2).unwrap(), dec("4.5"));
        assert!(sma(&closes, 6).is_none());
    }

    #[test]
    fn rsi_extremes() {
        // Monotonic rise → RSI 100; monotonic fall → RSI 0.
        let up: Vec<Decimal> = (1..=20).map(Decimal::from).collect();
        assert_eq!(rsi(&up, 14).unwrap(), Decimal::ONE_HUNDRED);
        let down: Vec<Decimal> = (1..=20).rev().map(Decimal::from).collect();
        assert_eq!(rsi(&down, 14).unwrap().round_dp(0), dec("0"));
        assert!(rsi(&up[..10], 14).is_none());
    }

    #[test]
    fn drawdown_finds_worst_peak_to_trough() {
        // Peak 200 → trough 100 = 50%, even though it recovers.
        let closes = series(&[100, 200, 150, 100, 180]);
        assert_eq!(max_drawdown_pct(&closes).unwrap(), dec("50"));
        // Monotonic rise → zero drawdown.
        assert_eq!(
            max_drawdown_pct(&series(&[1, 2, 3])).unwrap(),
            Decimal::ZERO
        );
    }

    #[test]
    fn summary_distances() {
        // 25 closes ending at 120 with SMA20 around 110ish: vs_sma20 > 0.
        let mut closes = vec![Decimal::from(100); 24];
        closes.push(dec("120"));
        let summary = summarize(&closes);
        assert!(summary.vs_sma20_pct.unwrap() > Decimal::ZERO);
        assert!(summary.vs_sma200_pct.is_none()); // not enough history
    }
}
