//! Missing-split detection.
//!
//! REKT stores SPLIT-ADJUSTED daily candles but the user's transactions are
//! entered at AS-TRADED (unadjusted) quantities. So if a stock split while you
//! held it and you never recorded the split, your share count is stuck at the
//! pre-split number while the price data is post-split — corrupting both the
//! live position valuation AND the tax basis. This compares the broker's
//! corporate-action record against the log and flags splits you're missing, so
//! the engine that already handles `TxKind::Split` correctly actually gets the
//! data it needs.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::portfolio::{Tx, TxKind};
use crate::taxes::ny_date;

/// A corporate split as reported by the market-data provider.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SplitEvent {
    pub symbol: String,
    /// Ex-date (the NY trade date the adjustment takes effect).
    pub ex_date: NaiveDate,
    /// Share multiplier = new_rate / old_rate (10 for a 10:1 forward split,
    /// 0.1 for a 1:10 reverse split) — the same number `TxKind::Split` uses.
    pub ratio: Decimal,
}

/// A split the provider knows about that the transaction log is missing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MissingSplit {
    pub symbol: String,
    pub ex_date: NaiveDate,
    pub ratio: Decimal,
    /// Shares held going into the ex-date (the as-traded, pre-split count) —
    /// for the UI to explain the impact.
    pub shares_held: Decimal,
}

/// A `Split` tx within this many days of an ex-date counts as "already
/// recorded" — broker ex-dates and a user's hand-entered date can differ by a
/// day or two.
const MATCH_WINDOW_DAYS: i64 = 5;

fn norm(symbol: Option<&str>) -> Option<String> {
    symbol
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_uppercase)
}

/// Splits the provider reports for symbols the user held across the ex-date,
/// that aren't already in the log. `txs` is the full chronological log.
pub fn detect_missing_splits(txs: &[Tx], events: &[SplitEvent]) -> Vec<MissingSplit> {
    let mut out = Vec::new();
    for ev in events {
        let sym = ev.symbol.trim().to_uppercase();

        // Already recorded? A Split for this symbol near the ex-date.
        let recorded = txs.iter().any(|t| {
            t.kind == TxKind::Split
                && norm(t.symbol.as_deref()).as_deref() == Some(sym.as_str())
                && (ny_date(t.ts) - ev.ex_date).num_days().abs() <= MATCH_WINDOW_DAYS
        });
        if recorded {
            continue;
        }

        // Shares held STRICTLY BEFORE the ex-date. The exact count is unreliable
        // when an earlier split is also missing, but ">0 vs not" is all we need
        // to decide whether the user was a holder of record. An already-recorded
        // earlier split is applied so the count stays meaningful.
        let mut held = Decimal::ZERO;
        for t in txs {
            if norm(t.symbol.as_deref()).as_deref() != Some(sym.as_str()) {
                continue;
            }
            if ny_date(t.ts) >= ev.ex_date {
                continue;
            }
            match t.kind {
                TxKind::Buy => held += t.qty,
                TxKind::Sell => held -= t.qty,
                TxKind::Split => held *= t.qty,
                _ => {}
            }
        }

        if held > Decimal::ZERO {
            out.push(MissingSplit {
                symbol: sym,
                ex_date: ev.ex_date,
                ratio: ev.ratio,
                shares_held: held,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::dec;
    use chrono::{DateTime, Utc};

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn tx(id: i64, kind: TxKind, symbol: &str, qty: &str, when: &str) -> Tx {
        Tx {
            id,
            kind,
            symbol: Some(symbol.into()),
            qty: dec(qty),
            price: dec("100"),
            fees: Decimal::ZERO,
            taxes: Decimal::ZERO,
            ts: ts(when),
            note: String::new(),
        }
    }

    fn split(symbol: &str, ex: &str, ratio: &str) -> SplitEvent {
        SplitEvent {
            symbol: symbol.into(),
            ex_date: ex.parse().unwrap(),
            ratio: dec(ratio),
        }
    }

    #[test]
    fn flags_a_split_held_across_and_not_recorded() {
        let txs = vec![tx(1, TxKind::Buy, "NVDA", "10", "2024-01-10T15:00:00Z")];
        let got = detect_missing_splits(&txs, &[split("NVDA", "2024-06-10", "10")]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].symbol, "NVDA");
        assert_eq!(got[0].ratio, dec("10"));
        assert_eq!(got[0].shares_held, dec("10"));
    }

    #[test]
    fn already_recorded_split_is_not_flagged() {
        // Entered a day off the broker's ex-date — still matches the window.
        let txs = vec![
            tx(1, TxKind::Buy, "NVDA", "10", "2024-01-10T15:00:00Z"),
            tx(2, TxKind::Split, "NVDA", "10", "2024-06-11T15:00:00Z"),
        ];
        assert!(detect_missing_splits(&txs, &[split("NVDA", "2024-06-10", "10")]).is_empty());
    }

    #[test]
    fn not_held_on_ex_date_is_not_flagged() {
        // Sold out before the split, and a position opened only after it.
        let sold_before = vec![
            tx(1, TxKind::Buy, "NVDA", "10", "2024-01-10T15:00:00Z"),
            tx(2, TxKind::Sell, "NVDA", "10", "2024-03-01T15:00:00Z"),
        ];
        assert!(
            detect_missing_splits(&sold_before, &[split("NVDA", "2024-06-10", "10")]).is_empty()
        );

        let bought_after = vec![tx(1, TxKind::Buy, "NVDA", "100", "2024-07-01T15:00:00Z")];
        assert!(
            detect_missing_splits(&bought_after, &[split("NVDA", "2024-06-10", "10")]).is_empty()
        );
    }

    #[test]
    fn reverse_split_and_symbol_case_are_handled() {
        let txs = vec![tx(1, TxKind::Buy, "aapl", "100", "2024-01-10T15:00:00Z")];
        let got = detect_missing_splits(&txs, &[split("AAPL", "2024-06-10", "0.1")]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].ratio, dec("0.1"));
    }

    #[test]
    fn only_holders_among_many_symbols_are_flagged() {
        let txs = vec![
            tx(1, TxKind::Buy, "NVDA", "10", "2024-01-10T15:00:00Z"),
            tx(2, TxKind::Buy, "MSFT", "5", "2024-01-10T15:00:00Z"),
        ];
        let events = [
            split("NVDA", "2024-06-10", "10"),
            split("TSLA", "2024-06-10", "3"),
        ];
        let got = detect_missing_splits(&txs, &events);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].symbol, "NVDA");
    }
}
