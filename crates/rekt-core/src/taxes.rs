//! US capital-gains reporting: Form 8949 rows + Schedule D totals, with
//! wash-sale detection (IRC §1091).
//!
//! Everything is derived from the transaction log via [`compute_basis`]'s
//! disposal records, over the FULL log — wash-sale windows cross year
//! boundaries, so a December loss can be washed by a January rebuy.
//!
//! Honest limitations (stated in the UI too):
//! - A disallowed wash loss is REPORTED (column g, code W) but the basis
//!   carry-forward into the replacement lot is NOT propagated into future
//!   basis math — verify against your broker's 1099-B before filing.
//! - "Substantially identical" matching is exact-symbol only.
//! - Wash matching assumes no stock split lands inside a ±30-day window
//!   (share counts on either side of a split don't compare).
//! - This is bookkeeping, not tax advice.

use chrono::{DateTime, Datelike, Days, Months, NaiveDate, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::portfolio::{compute_basis, Disposal, PortfolioError, Tx, TxKind};

/// The America/New_York calendar date of an instant — tax forms care about
/// the US trade date, and a 1 AM UTC fill is still yesterday in New York.
pub fn ny_date(ts: DateTime<Utc>) -> NaiveDate {
    ts.with_timezone(&New_York).date_naive()
}

/// One Form 8949 row (one closed lot chunk).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Form8949Row {
    pub symbol: String,
    pub qty: Decimal,
    /// Column (b), NY trade date.
    pub acquired: NaiveDate,
    /// Column (c), NY trade date.
    pub sold: NaiveDate,
    /// Column (d), net of fees.
    pub proceeds: Decimal,
    /// Column (e), fees capitalized in.
    pub basis: Decimal,
    /// proceeds − basis, before any wash adjustment.
    pub gain: Decimal,
    /// Disallowed wash-sale loss as a positive amount (column g).
    pub disallowed: Decimal,
    /// Column (f): "W" when part of the loss is disallowed, else "".
    pub code: &'static str,
    /// Part II (held more than one year) vs Part I.
    pub long_term: bool,
}

/// Schedule D summary line for one holding-period bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TermTotals {
    pub proceeds: Decimal,
    pub basis: Decimal,
    /// Raw gain (proceeds − basis) before wash adjustments.
    pub gain: Decimal,
    /// Total disallowed wash losses (positive).
    pub disallowed: Decimal,
    /// gain + disallowed — the number that lands on Schedule D.
    pub reportable: Decimal,
}

impl TermTotals {
    fn add(&mut self, row: &Form8949Row) {
        self.proceeds += row.proceeds;
        self.basis += row.basis;
        self.gain += row.gain;
        self.disallowed += row.disallowed;
        self.reportable = self.gain + self.disallowed;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaxReport {
    pub year: i32,
    pub short: TermTotals,
    pub long: TermTotals,
    /// Sold-in-`year` rows, in sale order.
    pub rows: Vec<Form8949Row>,
}

/// Build the Form 8949 / Schedule D report for one calendar year.
///
/// `txs` must be the FULL chronologically ordered log (not pre-filtered to
/// the year): cost basis depends on every prior transaction and wash-sale
/// windows reach ±30 days across year boundaries.
pub fn tax_report(txs: &[Tx], year: i32) -> Result<TaxReport, PortfolioError> {
    let book = compute_basis(txs)?;
    let disallowed = wash_disallowed(&book.disposals, txs);

    let mut report = TaxReport {
        year,
        short: TermTotals::default(),
        long: TermTotals::default(),
        rows: Vec::new(),
    };
    for (d, dis) in book.disposals.iter().zip(&disallowed) {
        let sold = ny_date(d.sold);
        if sold.year() != year {
            continue;
        }
        let acquired = ny_date(d.acquired);
        // Long-term means held MORE than one year: sold strictly after the
        // first anniversary of the (split-preserving) acquisition date.
        let long_term = sold > acquired + Months::new(12);
        let row = Form8949Row {
            symbol: d.symbol.clone(),
            qty: d.qty,
            acquired,
            sold,
            proceeds: d.proceeds,
            basis: d.basis,
            gain: d.proceeds - d.basis,
            disallowed: *dis,
            code: if *dis > Decimal::ZERO { "W" } else { "" },
            long_term,
        };
        if long_term {
            report.long.add(&row);
        } else {
            report.short.add(&row);
        }
        report.rows.push(row);
    }
    Ok(report)
}

/// Per-disposal disallowed wash loss (aligned with `disposals`, which must
/// be in replay/sale order, as [`compute_basis`] produces them).
///
/// Model: every bought share carries one unit of "replacement capacity".
/// Walking sales chronologically, a disposal first burns capacity on its
/// own source shares (the shares you sold cannot replace themselves), then
/// a losing disposal consumes capacity from buys dated within ±30 NY days
/// of the sale. Each replaced share disallows its fraction of the loss; a
/// share can replace at most one sold share across the whole log.
fn wash_disallowed(disposals: &[Disposal], txs: &[Tx]) -> Vec<Decimal> {
    struct BuyCap {
        symbol: String,
        ts: DateTime<Utc>,
        date: NaiveDate,
        cap: Decimal,
    }
    let mut buys: Vec<BuyCap> = txs
        .iter()
        .filter(|t| t.kind == TxKind::Buy)
        .map(|t| BuyCap {
            symbol: t
                .symbol
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_uppercase(),
            ts: t.ts,
            date: ny_date(t.ts),
            cap: t.qty,
        })
        .collect();

    let mut out = vec![Decimal::ZERO; disposals.len()];
    for (i, d) in disposals.iter().enumerate() {
        // Sold shares can't be their own replacement: burn the source
        // buy's capacity (matched by symbol + acquisition instant).
        let mut burn = d.qty;
        for b in buys
            .iter_mut()
            .filter(|b| b.symbol == d.symbol && b.ts == d.acquired)
        {
            let take = burn.min(b.cap);
            b.cap -= take;
            burn -= take;
            if burn == Decimal::ZERO {
                break;
            }
        }

        let loss = d.basis - d.proceeds;
        if loss <= Decimal::ZERO {
            continue;
        }
        let sold = ny_date(d.sold);
        let lo = sold - Days::new(30);
        let hi = sold + Days::new(30);
        let mut replaced = Decimal::ZERO;
        for b in buys
            .iter_mut()
            .filter(|b| b.symbol == d.symbol && b.date >= lo && b.date <= hi)
        {
            if replaced >= d.qty {
                break;
            }
            let take = (d.qty - replaced).min(b.cap);
            b.cap -= take;
            replaced += take;
        }
        if replaced >= d.qty {
            out[i] = loss;
        } else if replaced > Decimal::ZERO {
            out[i] = (loss * replaced / d.qty).round_dp(6);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::dec;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn tx(id: i64, kind: TxKind, symbol: &str, qty: &str, price: &str, when: &str) -> Tx {
        Tx {
            id,
            kind,
            symbol: Some(symbol.into()),
            qty: dec(qty),
            price: dec(price),
            fees: Decimal::ZERO,
            taxes: Decimal::ZERO,
            ts: ts(when),
            note: String::new(),
        }
    }

    #[test]
    fn short_vs_long_term_boundary_is_strictly_more_than_one_year() {
        let txs = vec![
            tx(1, TxKind::Buy, "AAPL", "2", "100", "2025-03-10T15:00:00Z"),
            // Sold exactly on the first anniversary (NY date): short-term.
            tx(2, TxKind::Sell, "AAPL", "1", "150", "2026-03-10T15:00:00Z"),
            // Sold one day later: long-term.
            tx(3, TxKind::Sell, "AAPL", "1", "150", "2026-03-11T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        assert_eq!(report.rows.len(), 2);
        assert!(!report.rows[0].long_term);
        assert!(report.rows[1].long_term);
        assert_eq!(report.short.gain, dec("50"));
        assert_eq!(report.long.gain, dec("50"));
        assert_eq!(report.short.reportable, dec("50"));
    }

    #[test]
    fn split_preserves_the_holding_period() {
        let txs = vec![
            tx(1, TxKind::Buy, "NVDA", "10", "400", "2024-06-01T15:00:00Z"),
            tx(2, TxKind::Split, "NVDA", "4", "0", "2024-09-01T15:00:00Z"),
            tx(3, TxKind::Sell, "NVDA", "40", "150", "2026-02-02T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        assert_eq!(report.rows.len(), 1);
        let row = &report.rows[0];
        assert!(row.long_term, "holding clock starts at the original buy");
        assert_eq!(row.acquired, NaiveDate::from_ymd_opt(2024, 6, 1).unwrap());
        // Basis 4000 (split-adjusted unit cost), proceeds 6000.
        assert_eq!(row.gain, dec("2000"));
    }

    #[test]
    fn rebuy_within_thirty_days_disallows_the_loss() {
        let txs = vec![
            tx(1, TxKind::Buy, "TSLA", "10", "300", "2026-01-05T15:00:00Z"),
            tx(2, TxKind::Sell, "TSLA", "10", "250", "2026-03-02T15:00:00Z"),
            tx(3, TxKind::Buy, "TSLA", "10", "240", "2026-03-20T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        let row = &report.rows[0];
        assert_eq!(row.gain, dec("-500"));
        assert_eq!(row.disallowed, dec("500"));
        assert_eq!(row.code, "W");
        // Schedule D: the loss is fully disallowed.
        assert_eq!(report.short.reportable, dec("0"));
    }

    #[test]
    fn the_sold_shares_themselves_are_not_replacement_shares() {
        // Buy then sell the SAME shares at a loss 10 days later, with no
        // other purchase: no wash, even though the buy is in the window.
        let txs = vec![
            tx(1, TxKind::Buy, "VOO", "10", "500", "2026-04-01T15:00:00Z"),
            tx(2, TxKind::Sell, "VOO", "10", "450", "2026-04-10T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        assert_eq!(report.rows[0].disallowed, Decimal::ZERO);
        assert_eq!(report.rows[0].code, "");
        assert_eq!(report.short.reportable, dec("-500"));
    }

    #[test]
    fn partial_rebuy_disallows_a_proportional_slice() {
        let txs = vec![
            tx(1, TxKind::Buy, "AMD", "10", "200", "2025-11-03T15:00:00Z"),
            tx(2, TxKind::Sell, "AMD", "10", "150", "2026-02-02T15:00:00Z"),
            tx(3, TxKind::Buy, "AMD", "4", "140", "2026-02-10T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        let row = &report.rows[0];
        // Loss 500, 4 of 10 shares replaced → 200 disallowed.
        assert_eq!(row.disallowed, dec("200"));
        assert_eq!(row.code, "W");
        assert_eq!(report.short.reportable, dec("-300"));
    }

    #[test]
    fn wash_window_crosses_the_year_boundary() {
        // December loss washed by a January rebuy: the disallowance lands
        // in the December tax year, which is why the report needs the full
        // log rather than a year slice.
        let txs = vec![
            tx(1, TxKind::Buy, "MSFT", "5", "400", "2025-06-02T15:00:00Z"),
            tx(2, TxKind::Sell, "MSFT", "5", "350", "2025-12-20T15:00:00Z"),
            tx(3, TxKind::Buy, "MSFT", "5", "340", "2026-01-10T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2025).unwrap();
        assert_eq!(report.rows[0].disallowed, dec("250"));
        assert_eq!(report.short.reportable, dec("0"));
        // And 2026 has no rows at all (nothing sold).
        assert!(tax_report(&txs, 2026).unwrap().rows.is_empty());
    }

    #[test]
    fn replacement_capacity_is_consumed_once_across_disposals() {
        // One 5-share rebuy can't wash two 5-share losses.
        let txs = vec![
            tx(1, TxKind::Buy, "INTC", "10", "50", "2025-05-01T15:00:00Z"),
            tx(2, TxKind::Sell, "INTC", "5", "40", "2026-03-02T15:00:00Z"),
            tx(3, TxKind::Sell, "INTC", "5", "40", "2026-03-03T15:00:00Z"),
            tx(4, TxKind::Buy, "INTC", "5", "38", "2026-03-10T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        // First loss (50) fully washed; second keeps its full loss.
        assert_eq!(report.rows[0].disallowed, dec("50"));
        assert_eq!(report.rows[1].disallowed, Decimal::ZERO);
        assert_eq!(report.short.gain, dec("-100"));
        assert_eq!(report.short.reportable, dec("-50"));
    }

    #[test]
    fn gains_are_never_wash_adjusted_and_buys_outside_window_dont_count() {
        let txs = vec![
            tx(1, TxKind::Buy, "QQQ", "10", "400", "2025-01-06T15:00:00Z"),
            // A gain with a rebuy right next to it: no adjustment.
            tx(2, TxKind::Sell, "QQQ", "5", "450", "2025-06-02T15:00:00Z"),
            tx(3, TxKind::Buy, "QQQ", "5", "455", "2025-06-03T15:00:00Z"),
            // A loss whose only other buy is 31+ days away: no wash.
            tx(4, TxKind::Sell, "QQQ", "5", "380", "2025-09-02T15:00:00Z"),
            tx(5, TxKind::Buy, "QQQ", "5", "370", "2025-10-20T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2025).unwrap();
        assert_eq!(report.rows[0].disallowed, Decimal::ZERO);
        assert_eq!(report.rows[1].disallowed, Decimal::ZERO);
        assert_eq!(report.short.gain, dec("150")); // +250 − 100
    }

    #[test]
    fn ny_dates_pull_late_utc_fills_back_a_day() {
        // 2026-01-01T01:00Z is still 2025-12-31 in New York: the sale
        // belongs to tax year 2025.
        let txs = vec![
            tx(1, TxKind::Buy, "SPY", "1", "500", "2025-06-02T15:00:00Z"),
            tx(2, TxKind::Sell, "SPY", "1", "520", "2026-01-01T01:00:00Z"),
        ];
        assert_eq!(tax_report(&txs, 2025).unwrap().rows.len(), 1);
        assert!(tax_report(&txs, 2026).unwrap().rows.is_empty());
    }
}
