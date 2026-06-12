//! US capital-gains reporting: Form 8949 rows + Schedule D totals, with
//! wash-sale detection AND basis carry-forward (IRC §1091).
//!
//! The tax ledger is its own chronological replay of the FULL transaction
//! log — wash-sale windows cross year boundaries, and TAX basis diverges
//! from book basis once adjustments apply: a disallowed loss is added to
//! the replacement shares' basis and the surrendered shares' holding
//! period is tacked on, so the loss is deferred (re-emerging when the
//! replacement lot is sold), not erased. This matches broker 1099-B
//! treatment.
//!
//! Honest limitations (stated in the UI too):
//! - "Substantially identical" matching is exact-symbol only.
//! - Partially selling a recent purchase at a loss IS flagged as a wash:
//!   the still-held remainder of that purchase counts as replacement
//!   shares (mechanical §1091, matching broker 1099-B practice). Selling
//!   the entire purchase is not a wash (Rev. Rul. 56-602), and lots
//!   consumed by the same sell never replace each other.
//! - Replacement-share capacities are split-adjusted, but a wash whose
//!   ±30-day window straddles a split still compares share counts
//!   mechanically; a pending basis adjustment onto a post-split buy is
//!   carried in pre-split per-share terms. Reconcile with your 1099-B.
//! - This is bookkeeping, not tax advice.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Datelike, Days, Months, NaiveDate, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::portfolio::{PortfolioError, Tx, TxKind};

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
    /// Column (b), NY trade date — for replacement lots this is the
    /// TACKED date (shifted back by the surrendered shares' holding
    /// period), the date the holding-period clock effectively started.
    pub acquired: NaiveDate,
    /// Column (c), NY trade date.
    pub sold: NaiveDate,
    /// Column (d), net of fees.
    pub proceeds: Decimal,
    /// Column (e), fees capitalized in PLUS any wash-sale basis carried
    /// forward from an earlier disallowed loss.
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
        self.reportable += row.gain + row.disallowed;
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
/// the year): tax basis depends on every prior transaction, and wash-sale
/// windows and basis carry-forwards reach across year boundaries.
pub fn tax_report(txs: &[Tx], year: i32) -> Result<TaxReport, PortfolioError> {
    let disposals = tax_disposals(txs)?;

    let mut report = TaxReport {
        year,
        short: TermTotals::default(),
        long: TermTotals::default(),
        rows: Vec::new(),
    };
    for d in disposals {
        if d.sold.year() != year {
            continue;
        }
        // Long-term means held MORE than one year: sold strictly after the
        // first anniversary of the (tacked, split-preserving) acquisition
        // date — matches IRS Pub 550's example (bought Feb 5, sold the
        // following Feb 6 → long-term).
        let long_term = d.sold > d.holding_from + Months::new(12);
        let row = Form8949Row {
            symbol: d.symbol,
            qty: d.qty,
            acquired: d.holding_from,
            sold: d.sold,
            proceeds: d.proceeds,
            basis: d.basis,
            gain: d.proceeds - d.basis,
            disallowed: d.disallowed,
            code: if d.disallowed > Decimal::ZERO {
                "W"
            } else {
                ""
            },
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

/// An open TAX lot: like the portfolio engine's FIFO lot, but its basis
/// carries wash-sale adjustments and its holding anchor carries tacking.
#[derive(Debug, Clone)]
struct TaxLot {
    /// Originating buy transaction — links the lot to its replacement
    /// capacity entry.
    buy_id: i64,
    qty: Decimal,
    /// Per-share TAX basis (capitalized fees + carried-forward wash loss).
    unit_basis: Decimal,
    /// NY date the holding-period clock started (tacked for replacements).
    holding_from: NaiveDate,
}

/// Per-buy replacement capacity: each purchased share can replace at most
/// one wash-sold share across the whole log, and burns when sold itself.
struct BuyCap {
    id: i64,
    symbol: String,
    ts: DateTime<Utc>,
    date: NaiveDate,
    cap: Decimal,
}

/// A wash adjustment owed to a buy that hasn't been replayed yet (the
/// replacement was purchased within 30 days AFTER the loss sale).
struct PendingAdj {
    qty: Decimal,
    per_share_add: Decimal,
    holding_from: NaiveDate,
}

/// One closed tax-lot chunk, wash-adjusted.
#[derive(Debug)]
struct TaxDisposal {
    symbol: String,
    qty: Decimal,
    holding_from: NaiveDate,
    sold: NaiveDate,
    proceeds: Decimal,
    basis: Decimal,
    disallowed: Decimal,
}

/// Replay the log into wash-adjusted disposals (the validation mirrors
/// [`crate::portfolio::compute_basis`], so an inconsistent log fails the
/// same way here as everywhere else).
fn tax_disposals(txs: &[Tx]) -> Result<Vec<TaxDisposal>, PortfolioError> {
    // Replacement capacity is precollected over ALL buys: a loss can be
    // washed by a purchase up to 30 days in the future.
    let mut caps: Vec<BuyCap> = txs
        .iter()
        .filter(|t| t.kind == TxKind::Buy)
        .map(|t| BuyCap {
            id: t.id,
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

    let mut positions: BTreeMap<String, Vec<TaxLot>> = BTreeMap::new();
    let mut pending: HashMap<i64, Vec<PendingAdj>> = HashMap::new();
    let mut out: Vec<TaxDisposal> = Vec::new();

    for tx in txs {
        let symbol = || -> Result<String, PortfolioError> {
            tx.symbol
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_uppercase)
                .ok_or(PortfolioError::MissingSymbol {
                    id: tx.id,
                    kind: tx.kind.as_str(),
                })
        };

        match tx.kind {
            TxKind::Buy => {
                let symbol = symbol()?;
                if tx.qty <= Decimal::ZERO {
                    return Err(PortfolioError::NonPositive {
                        id: tx.id,
                        field: "qty",
                    });
                }
                let unit = (tx.qty * tx.price + tx.fees + tx.taxes) / tx.qty;
                let lots = positions.entry(symbol).or_default();
                // A buy that was matched as a FUTURE replacement enters the
                // book pre-adjusted: basis bumped, holding period tacked.
                let mut remaining = tx.qty;
                for adj in pending.remove(&tx.id).unwrap_or_default() {
                    let take = remaining.min(adj.qty);
                    if take > Decimal::ZERO {
                        lots.push(TaxLot {
                            buy_id: tx.id,
                            qty: take,
                            unit_basis: unit + adj.per_share_add,
                            holding_from: adj.holding_from,
                        });
                        remaining -= take;
                    }
                }
                if remaining > Decimal::ZERO {
                    lots.push(TaxLot {
                        buy_id: tx.id,
                        qty: remaining,
                        unit_basis: unit,
                        holding_from: ny_date(tx.ts),
                    });
                }
            }
            TxKind::Sell => {
                let symbol = symbol()?;
                if tx.qty <= Decimal::ZERO {
                    return Err(PortfolioError::NonPositive {
                        id: tx.id,
                        field: "qty",
                    });
                }
                let lots = positions.entry(symbol.clone()).or_default();
                let have: Decimal = lots.iter().map(|l| l.qty).sum();
                if tx.qty > have {
                    return Err(PortfolioError::Oversell {
                        id: tx.id,
                        symbol,
                        have,
                        want: tx.qty,
                    });
                }
                sell(tx, symbol, lots, &mut caps, &mut pending, &mut out);
            }
            TxKind::Split => {
                let symbol = symbol()?;
                if tx.qty <= Decimal::ZERO {
                    return Err(PortfolioError::BadSplitRatio {
                        id: tx.id,
                        ratio: tx.qty,
                    });
                }
                for lot in positions.entry(symbol.clone()).or_default() {
                    lot.qty *= tx.qty;
                    lot.unit_basis /= tx.qty;
                }
                // Capacities of shares bought BEFORE the split scale with
                // them; later buys are already in post-split units.
                for cap in caps
                    .iter_mut()
                    .filter(|c| c.symbol == symbol && c.ts <= tx.ts)
                {
                    cap.cap *= tx.qty;
                }
            }
            TxKind::Dividend => {
                symbol()?; // same validation as the portfolio engine
            }
            TxKind::Deposit | TxKind::Withdrawal => {}
        }
    }
    Ok(out)
}

/// Process one sell: consume FIFO tax lots, allocate net proceeds pro rata
/// (last chunk takes the exact remainder), burn the WHOLE sell's source
/// capacity before any matching, then wash-match each losing chunk and
/// carry the disallowed amount into its replacement shares.
fn sell(
    tx: &Tx,
    symbol: String,
    lots: &mut Vec<TaxLot>,
    caps: &mut [BuyCap],
    pending: &mut HashMap<i64, Vec<PendingAdj>>,
    out: &mut Vec<TaxDisposal>,
) {
    let proceeds = tx.qty * tx.price - tx.fees - tx.taxes;
    let sold_date = ny_date(tx.ts);

    struct Chunk {
        buy_id: i64,
        qty: Decimal,
        basis: Decimal,
        holding_from: NaiveDate,
    }
    let mut chunks = Vec::new();
    let mut remaining = tx.qty;
    while remaining > Decimal::ZERO {
        let lot = lots.first_mut().expect("oversell checked by caller");
        let take = remaining.min(lot.qty);
        chunks.push(Chunk {
            buy_id: lot.buy_id,
            qty: take,
            basis: take * lot.unit_basis,
            holding_from: lot.holding_from,
        });
        lot.qty -= take;
        remaining -= take;
        if lot.qty == Decimal::ZERO {
            lots.remove(0);
        }
    }

    // Burn source capacity for the whole sell first: sold shares can't be
    // their own replacement, and sibling chunks can't replace each other.
    for c in &chunks {
        if let Some(cap) = caps.iter_mut().find(|b| b.id == c.buy_id) {
            cap.cap -= c.qty.min(cap.cap);
        }
    }

    let mut allocated = Decimal::ZERO;
    let last = chunks.len() - 1;
    for (k, c) in chunks.iter().enumerate() {
        let chunk_proceeds = if k == last {
            proceeds - allocated
        } else {
            (proceeds * c.qty / tx.qty).round_dp(6)
        };
        allocated += chunk_proceeds;

        let mut disallowed = Decimal::ZERO;
        let loss = c.basis - chunk_proceeds;
        if loss > Decimal::ZERO {
            // Replacement shares: buys within ±30 NY days with capacity,
            // chronological (caps are in tx order).
            let lo = sold_date - Days::new(30);
            let hi = sold_date + Days::new(30);
            let mut matches: Vec<(i64, DateTime<Utc>, NaiveDate, Decimal)> = Vec::new();
            let mut replaced = Decimal::ZERO;
            for b in caps
                .iter_mut()
                .filter(|b| b.symbol == symbol && b.date >= lo && b.date <= hi)
            {
                if replaced >= c.qty {
                    break;
                }
                let take = (c.qty - replaced).min(b.cap);
                if take > Decimal::ZERO {
                    b.cap -= take;
                    replaced += take;
                    matches.push((b.id, b.ts, b.date, take));
                }
            }
            if replaced > Decimal::ZERO {
                disallowed = if replaced >= c.qty {
                    loss
                } else {
                    (loss * replaced / c.qty).round_dp(6)
                };
                // Carry the disallowed loss into the replacement shares,
                // allocated per matched buy with a remainder correction so
                // the carried basis sums exactly to the reported column g.
                // Holding-period tack: the replacement clock starts the
                // surrendered shares' holding period earlier.
                let held = sold_date.signed_duration_since(c.holding_from);
                let mut adj_alloc = Decimal::ZERO;
                let last_match = matches.len() - 1;
                for (j, (buy_id, buy_ts, buy_date, r)) in matches.iter().enumerate() {
                    let amount = if j == last_match {
                        disallowed - adj_alloc
                    } else {
                        (disallowed * r / replaced).round_dp(6)
                    };
                    adj_alloc += amount;
                    let per_share_add = amount / r;
                    let anchor = *buy_date - held;
                    if *buy_ts <= tx.ts {
                        // Replacement already held: adjust its lots now.
                        adjust_held_lots(lots, *buy_id, *r, per_share_add, anchor);
                    } else {
                        // Replacement bought after the sale: adjust the
                        // lot when that buy is replayed.
                        pending.entry(*buy_id).or_default().push(PendingAdj {
                            qty: *r,
                            per_share_add,
                            holding_from: anchor,
                        });
                    }
                }
            }
        }

        out.push(TaxDisposal {
            symbol: symbol.clone(),
            qty: c.qty,
            holding_from: c.holding_from,
            sold: sold_date,
            proceeds: chunk_proceeds,
            basis: c.basis,
            disallowed,
        });
    }
}

/// Designate `qty` still-held shares of `buy_id` as replacement shares:
/// bump their per-share basis and tack the holding anchor, splitting a lot
/// when only part of it is designated.
fn adjust_held_lots(
    lots: &mut Vec<TaxLot>,
    buy_id: i64,
    mut qty: Decimal,
    per_share_add: Decimal,
    anchor: NaiveDate,
) {
    let mut i = 0;
    while i < lots.len() && qty > Decimal::ZERO {
        if lots[i].buy_id != buy_id {
            i += 1;
            continue;
        }
        let take = qty.min(lots[i].qty);
        if take == lots[i].qty {
            lots[i].unit_basis += per_share_add;
            lots[i].holding_from = anchor;
        } else {
            lots[i].qty -= take;
            let adjusted = TaxLot {
                buy_id,
                qty: take,
                unit_basis: lots[i].unit_basis + per_share_add,
                holding_from: anchor,
            };
            lots.insert(i + 1, adjusted);
            i += 1; // don't revisit the lot we just inserted
        }
        qty -= take;
        i += 1;
    }
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
    fn split_scales_replacement_capacity_too() {
        // Buy 10, 4:1 split, dump all 40 at a loss with no rebuy: the burn
        // must cancel the full post-split capacity — no phantom wash.
        let txs = vec![
            tx(1, TxKind::Buy, "NVDA", "10", "400", "2026-01-05T15:00:00Z"),
            tx(2, TxKind::Split, "NVDA", "4", "0", "2026-01-10T15:00:00Z"),
            tx(3, TxKind::Sell, "NVDA", "40", "80", "2026-01-20T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        assert_eq!(report.rows[0].disallowed, Decimal::ZERO);
        assert_eq!(report.short.reportable, dec("-800"));
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
        // Schedule D: the loss is fully disallowed (deferred, not gone).
        assert_eq!(report.short.reportable, dec("0"));
    }

    #[test]
    fn deferred_loss_reemerges_when_the_replacement_lot_is_sold() {
        let txs = vec![
            tx(1, TxKind::Buy, "TSLA", "10", "100", "2026-01-05T15:00:00Z"),
            tx(2, TxKind::Sell, "TSLA", "10", "90", "2026-02-02T15:00:00Z"),
            tx(3, TxKind::Buy, "TSLA", "10", "95", "2026-02-10T15:00:00Z"),
            // Final exit, far outside any window, at the rebuy price.
            tx(4, TxKind::Sell, "TSLA", "10", "95", "2026-12-01T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        // Row 1: $100 loss, fully washed.
        assert_eq!(report.rows[0].disallowed, dec("100"));
        // Row 2: sold at the rebuy price, yet the carried basis (950 + 100
        // = 1050) surfaces the deferred $100 loss.
        assert_eq!(report.rows[1].basis, dec("1050"));
        assert_eq!(report.rows[1].gain, dec("-100"));
        assert_eq!(report.rows[1].disallowed, Decimal::ZERO);
        // Year total equals the economic truth: bought 1950, sold 1850.
        assert_eq!(
            report.short.reportable + report.long.reportable,
            dec("-100")
        );
    }

    #[test]
    fn holding_period_tacks_onto_replacement_shares() {
        // Held ~11 months before the wash sale; the replacement is held
        // only 6 weeks but the tacked clock makes its sale long-term.
        let txs = vec![
            tx(1, TxKind::Buy, "MSFT", "5", "400", "2024-01-10T15:00:00Z"),
            tx(2, TxKind::Sell, "MSFT", "5", "350", "2024-12-20T15:00:00Z"),
            tx(3, TxKind::Buy, "MSFT", "5", "340", "2024-12-30T15:00:00Z"),
            tx(4, TxKind::Sell, "MSFT", "5", "500", "2025-02-10T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2025).unwrap();
        let row = &report.rows[0];
        // Tacked anchor: Dec 30 minus the 345 days held = Jan 20 2024.
        assert_eq!(row.acquired, NaiveDate::from_ymd_opt(2024, 1, 20).unwrap());
        assert!(row.long_term, "tacked holding period crosses one year");
        // Basis carries the $250 disallowed loss: 1700 + 250 = 1950.
        assert_eq!(row.basis, dec("1950"));
        assert_eq!(row.gain, dec("550"));
    }

    #[test]
    fn replacement_bought_before_the_sale_is_adjusted_in_place() {
        let txs = vec![
            tx(1, TxKind::Buy, "AMD", "10", "100", "2026-01-05T15:00:00Z"),
            tx(2, TxKind::Buy, "AMD", "10", "95", "2026-03-02T15:00:00Z"),
            // FIFO sells lot 1 at a loss; lot 2 (bought 8 days earlier) is
            // the replacement and absorbs the disallowed $100.
            tx(3, TxKind::Sell, "AMD", "10", "90", "2026-03-10T15:00:00Z"),
            tx(4, TxKind::Sell, "AMD", "10", "105", "2026-06-01T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        assert_eq!(report.rows[0].disallowed, dec("100"));
        // Lot 2's basis: 950 + 100 carried = 1050; sold for 1050 → flat.
        assert_eq!(report.rows[1].basis, dec("1050"));
        assert_eq!(report.rows[1].gain, dec("0"));
        // Economic truth across the year: bought 1950, sold 1950.
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
        // in the December tax year, and the carried basis surfaces it in
        // the January lot's eventual sale.
        let txs = vec![
            tx(1, TxKind::Buy, "MSFT", "5", "400", "2025-06-02T15:00:00Z"),
            tx(2, TxKind::Sell, "MSFT", "5", "350", "2025-12-20T15:00:00Z"),
            tx(3, TxKind::Buy, "MSFT", "5", "340", "2026-01-10T15:00:00Z"),
            tx(4, TxKind::Sell, "MSFT", "5", "340", "2026-06-01T15:00:00Z"),
        ];
        let report_2025 = tax_report(&txs, 2025).unwrap();
        assert_eq!(report_2025.rows[0].disallowed, dec("250"));
        assert_eq!(report_2025.short.reportable, dec("0"));
        // 2026: the deferred $250 re-emerges through the carried basis.
        let report_2026 = tax_report(&txs, 2026).unwrap();
        assert_eq!(report_2026.rows[0].basis, dec("1950")); // 1700 + 250
        assert_eq!(
            report_2026.short.reportable + report_2026.long.reportable,
            dec("-250")
        );
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
    fn full_exit_across_two_lots_in_one_sell_is_not_a_wash() {
        // Two FIFO lots liquidated by a single sell with no rebuy: the
        // sibling lot's buy is inside the ±30-day window but its shares
        // were sold in the same transaction — they are not replacements.
        let txs = vec![
            tx(1, TxKind::Buy, "TSLA", "50", "200", "2026-01-02T15:00:00Z"),
            tx(2, TxKind::Buy, "TSLA", "50", "190", "2026-01-06T15:00:00Z"),
            tx(
                3,
                TxKind::Sell,
                "TSLA",
                "100",
                "150",
                "2026-01-15T15:00:00Z",
            ),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].disallowed, Decimal::ZERO);
        assert_eq!(report.rows[1].disallowed, Decimal::ZERO);
        // Both losses recognized in full: −2500 (lot 1) − 2000 (lot 2).
        assert_eq!(report.short.reportable, dec("-4500"));
    }

    #[test]
    fn partially_selling_a_recent_buy_washes_against_the_remainder() {
        // Buy 20, sell 10 at a loss nine days later: the 10 still-held
        // shares of the same purchase are replacement shares (mechanical
        // §1091 — broker 1099-B practice), and they absorb the basis.
        let txs = vec![
            tx(1, TxKind::Buy, "VOO", "20", "500", "2026-04-01T15:00:00Z"),
            tx(2, TxKind::Sell, "VOO", "10", "450", "2026-04-10T15:00:00Z"),
            tx(3, TxKind::Sell, "VOO", "10", "450", "2026-08-03T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2026).unwrap();
        assert_eq!(report.rows[0].gain, dec("-500"));
        assert_eq!(report.rows[0].disallowed, dec("500"));
        assert_eq!(report.rows[0].code, "W");
        // The remainder carries the deferred loss: basis 5000 + 500.
        assert_eq!(report.rows[1].basis, dec("5500"));
        assert_eq!(report.rows[1].gain, dec("-1000"));
        // Economic truth: bought 10000, sold 9000 → −1000 reportable.
        assert_eq!(report.short.reportable, dec("-1000"));
    }

    #[test]
    fn chunk_proceeds_sum_exactly_across_lots() {
        // 3:4 split of net proceeds (7×250 − 1 fee) cannot drop a cent.
        let txs = vec![
            tx(1, TxKind::Buy, "AAPL", "3", "100", "2025-03-10T15:00:00Z"),
            tx(2, TxKind::Buy, "AAPL", "10", "200", "2025-09-01T15:00:00Z"),
            Tx {
                fees: dec("1"),
                ..tx(3, TxKind::Sell, "AAPL", "7", "250", "2026-02-02T15:00:00Z")
            },
        ];
        let report = tax_report(&txs, 2026).unwrap();
        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].proceeds, dec("749.571429"));
        assert_eq!(
            report.rows[0].proceeds + report.rows[1].proceeds,
            dec("1749")
        );
        assert_eq!(report.rows[0].basis, dec("300"));
        assert_eq!(report.rows[1].basis, dec("800"));
    }

    #[test]
    fn leap_day_acquisition_clamps_to_feb_28_anniversary() {
        // chrono clamps 2024-02-29 + 12 months to 2025-02-28: a sale that
        // day is still short-term, the next day is long-term — and nothing
        // panics on the nonexistent 2025-02-29.
        let txs = vec![
            tx(1, TxKind::Buy, "SPY", "2", "500", "2024-02-29T15:00:00Z"),
            tx(2, TxKind::Sell, "SPY", "1", "520", "2025-02-28T15:00:00Z"),
            tx(3, TxKind::Sell, "SPY", "1", "520", "2025-03-03T15:00:00Z"),
        ];
        let report = tax_report(&txs, 2025).unwrap();
        assert!(!report.rows[0].long_term);
        assert!(report.rows[1].long_term);
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

    #[test]
    fn oversell_and_missing_symbol_fail_like_the_portfolio_engine() {
        let txs = vec![
            tx(1, TxKind::Buy, "AAPL", "5", "100", "2026-01-05T15:00:00Z"),
            tx(2, TxKind::Sell, "AAPL", "6", "100", "2026-02-02T15:00:00Z"),
        ];
        assert_eq!(
            tax_report(&txs, 2026).unwrap_err(),
            PortfolioError::Oversell {
                id: 2,
                symbol: "AAPL".into(),
                have: dec("5"),
                want: dec("6"),
            }
        );
        let txs = vec![Tx {
            symbol: None,
            ..tx(1, TxKind::Buy, "X", "5", "100", "2026-01-05T15:00:00Z")
        }];
        assert!(matches!(
            tax_report(&txs, 2026).unwrap_err(),
            PortfolioError::MissingSymbol { id: 1, .. }
        ));
    }
}
