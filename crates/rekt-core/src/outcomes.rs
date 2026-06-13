//! Recommendation outcome scoring: did the analyst's call age well?
//!
//! Outcomes are DERIVED, never stored: the baseline is the first daily
//! close on/after the recommendation's NY date, measured against the
//! latest close, with the verdict direction-adjusted per action — a sell
//! call was right when the price FELL. No close on/after the
//! recommendation date yet means no outcome (honesty over a fabricated
//! baseline), and "hold"/"watch" have no testable direction.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::history::Closes;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecOutcome {
    /// First close on/after the recommendation date.
    pub baseline: Decimal,
    pub latest: Decimal,
    /// Percent change baseline → latest, 2 dp.
    pub return_pct: Decimal,
    /// buy/add want up, sell/trim want down, hold/watch are untestable.
    pub favorable: Option<bool>,
}

pub fn recommendation_outcome(
    action: &str,
    rec_date: NaiveDate,
    closes: &Closes,
) -> Option<RecOutcome> {
    let (baseline_date, baseline) = closes.range(rec_date..).next()?;
    let (latest_date, latest) = closes.iter().next_back()?;
    // One close is no window: a call can't be judged on zero elapsed
    // trading days, and Some(false) here would systematically deflate the
    // hit rate for every fresh recommendation.
    if baseline_date == latest_date {
        return None;
    }
    if *baseline <= Decimal::ZERO {
        return None; // corrupt candle — no verdict beats a wrong one
    }
    let return_pct = ((latest - baseline) / baseline * Decimal::ONE_HUNDRED).round_dp(2);
    // A genuine multi-day flat stays Some(false): a directional call that
    // produced no move gave no edge.
    let favorable = match action {
        "buy" | "add" => Some(return_pct > Decimal::ZERO),
        "sell" | "trim" => Some(return_pct < Decimal::ZERO),
        _ => None,
    };
    Some(RecOutcome {
        baseline: *baseline,
        latest: *latest,
        return_pct,
        favorable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::dec;

    fn day(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn closes(points: &[(&str, &str)]) -> Closes {
        points.iter().map(|(d, c)| (day(d), dec(c))).collect()
    }

    #[test]
    fn buy_wants_up_and_sell_wants_down() {
        let series = closes(&[("2026-05-01", "100"), ("2026-06-01", "110")]);
        let buy = recommendation_outcome("buy", day("2026-05-01"), &series).unwrap();
        assert_eq!(buy.return_pct, dec("10"));
        assert_eq!(buy.favorable, Some(true));
        let sell = recommendation_outcome("sell", day("2026-05-01"), &series).unwrap();
        assert_eq!(sell.favorable, Some(false));
        // hold/watch carry no direction to test.
        let hold = recommendation_outcome("hold", day("2026-05-01"), &series).unwrap();
        assert_eq!(hold.favorable, None);
    }

    #[test]
    fn weekend_recommendation_baselines_on_the_next_close() {
        // Recommended Saturday; first close is Monday's.
        let series = closes(&[
            ("2026-05-01", "90"), // Friday — must NOT be the baseline
            ("2026-05-04", "100"),
            ("2026-05-11", "95"),
        ]);
        let outcome = recommendation_outcome("sell", day("2026-05-02"), &series).unwrap();
        assert_eq!(outcome.baseline, dec("100"));
        assert_eq!(outcome.return_pct, dec("-5"));
        assert_eq!(outcome.favorable, Some(true));
    }

    #[test]
    fn no_close_after_the_recommendation_means_no_outcome() {
        let series = closes(&[("2026-05-01", "100")]);
        assert!(recommendation_outcome("buy", day("2026-05-02"), &series).is_none());
        assert!(recommendation_outcome("buy", day("2026-05-02"), &Closes::new()).is_none());
    }

    #[test]
    fn a_single_close_is_no_window_to_judge() {
        // Recommended today, one close so far: zero elapsed trading days
        // is "not judged yet", never an unfavorable mark.
        let series = closes(&[("2026-05-01", "100")]);
        assert!(recommendation_outcome("buy", day("2026-05-01"), &series).is_none());
        assert!(recommendation_outcome("sell", day("2026-05-01"), &series).is_none());
    }

    #[test]
    fn a_genuine_multi_day_flat_is_not_favorable_either_way() {
        let series = closes(&[("2026-05-01", "100"), ("2026-05-08", "100")]);
        let outcome = recommendation_outcome("buy", day("2026-05-01"), &series).unwrap();
        assert_eq!(outcome.return_pct, dec("0"));
        assert_eq!(outcome.favorable, Some(false));
        let sell = recommendation_outcome("sell", day("2026-05-01"), &series).unwrap();
        assert_eq!(sell.favorable, Some(false));
    }
}
