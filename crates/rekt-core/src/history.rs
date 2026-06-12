//! Historical performance: equity series replay, time-weighted return,
//! money-weighted return (IRR), and the cash-flow-matched benchmark.
//!
//! Pure functions over the transaction log + cached daily closes — the
//! whole history is recomputable, never stored as truth (PLAN.md §7).

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::portfolio::{apply_tx, PortfolioBasis, Tx, TxKind};

/// Daily closes per symbol (date → close).
pub type Closes = BTreeMap<NaiveDate, Decimal>;

#[derive(Debug, Clone, Serialize)]
pub struct DayPoint {
    pub date: NaiveDate,
    pub equity: Decimal,
    pub cash: Decimal,
    /// Cumulative net deposits up to this day (capital in).
    pub net_deposited: Decimal,
    /// Cash-flow-matched benchmark value ("what if every deposit bought
    /// SPY instead").
    pub benchmark: Option<Decimal>,
    /// Net external flow on this day (+deposit, -withdrawal).
    pub flow: Decimal,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryMetrics {
    /// Time-weighted return over the series, percent.
    pub twr_pct: Option<Decimal>,
    /// Money-weighted (IRR) annualized, percent.
    pub irr_pct: Option<Decimal>,
    /// Benchmark TWR over the same window, percent.
    pub benchmark_twr_pct: Option<Decimal>,
}

/// Last close at or before `date`.
fn close_at(closes: &Closes, date: NaiveDate) -> Option<Decimal> {
    closes.range(..=date).next_back().map(|(_, c)| *c)
}

/// Replay the log into a daily equity series from the first transaction to
/// `end`, valuing positions at the last known close (falling back to avg
/// cost before any candle exists — honest, and only affects the earliest
/// days of a brand-new symbol).
pub fn equity_series(
    txs: &[Tx],
    closes_by_symbol: &HashMap<String, Closes>,
    benchmark_closes: Option<&Closes>,
    end: NaiveDate,
) -> (Vec<DayPoint>, HistoryMetrics) {
    let Some(first) = txs.first() else {
        return (
            Vec::new(),
            HistoryMetrics {
                twr_pct: None,
                irr_pct: None,
                benchmark_twr_pct: None,
            },
        );
    };
    let start = first.ts.date_naive();

    let mut points = Vec::new();
    let mut tx_idx = 0;
    let mut basis = PortfolioBasis::default();
    let mut bench_shares = Decimal::ZERO;
    // External flows that arrived before the benchmark had a usable close
    // (e.g. a deposit on a holiday before the first SPY candle). They buy
    // in at the first close that appears instead of being dropped.
    let mut bench_pending = Decimal::ZERO;

    let mut date = start;
    'days: while date <= end {
        // Advance the replay through this day's transactions, incrementally
        // (replaying the whole prefix per day would be quadratic).
        let mut day_flow = Decimal::ZERO;
        while tx_idx < txs.len() && txs[tx_idx].ts.date_naive() <= date {
            let tx = &txs[tx_idx];
            match tx.kind {
                TxKind::Deposit => day_flow += tx.price,
                TxKind::Withdrawal => day_flow -= tx.price,
                _ => {}
            }
            // An inconsistent log can't be valued; stop the series here
            // rather than charting garbage.
            if apply_tx(&mut basis, tx).is_err() {
                break 'days;
            }
            tx_idx += 1;
        }
        // Value the book at today's closes.
        let mut equity = basis.cash;
        for (symbol, position) in &basis.positions {
            if position.qty == Decimal::ZERO {
                continue;
            }
            let price = closes_by_symbol
                .get(symbol)
                .and_then(|closes| close_at(closes, date))
                .or_else(|| position.avg_cost());
            if let Some(price) = price {
                equity += price * position.qty;
            }
        }

        // Benchmark: route the same external flows into the benchmark asset
        // at that day's close. Days before the first close emit None (not a
        // fake $0) and their flows stay pending until a close exists.
        let benchmark = benchmark_closes.and_then(|spy| {
            bench_pending += day_flow;
            let close = close_at(spy, date).filter(|c| *c > Decimal::ZERO)?;
            if bench_pending != Decimal::ZERO {
                bench_shares += bench_pending / close;
                bench_pending = Decimal::ZERO;
                if bench_shares < Decimal::ZERO {
                    bench_shares = Decimal::ZERO; // over-withdrawal clamps to flat
                }
            }
            Some(bench_shares * close)
        });

        points.push(DayPoint {
            date,
            equity,
            cash: basis.cash,
            net_deposited: basis.deposited - basis.withdrawn,
            benchmark,
            flow: day_flow,
        });

        date = match date.succ_opt() {
            Some(next) => next,
            None => break,
        };
    }

    let metrics = metrics_for(&points);
    (points, metrics)
}

/// Compute TWR / IRR / benchmark TWR over any (sub)window of day points —
/// the range buttons in the UI slice the full series and re-derive metrics
/// for exactly the displayed window.
pub fn metrics_for(points: &[DayPoint]) -> HistoryMetrics {
    if points.len() < 2 {
        return HistoryMetrics {
            twr_pct: None,
            irr_pct: None,
            benchmark_twr_pct: None,
        };
    }

    // Chain daily factors with flows applied at the start of each day.
    // Sub-periods where capital sits at zero on BOTH ends (empty account)
    // contribute nothing and are skipped; a period that moves through zero
    // or negative equity has no representable return, so the whole metric
    // honestly becomes None instead of a chain with a silent hole.
    let mut twr_factor = Decimal::ONE;
    let mut twr_valid = true;
    let mut bench_factor: Option<Decimal> = points[0].benchmark.map(|_| Decimal::ONE);
    let mut bench_valid = true;
    for window in points.windows(2) {
        let (prev, day) = (&window[0], &window[1]);
        let base = prev.equity + day.flow;
        if base > Decimal::ZERO && day.equity > Decimal::ZERO {
            twr_factor *= day.equity / base;
        } else if day.equity != base {
            twr_valid = false;
        }
        if let (Some(prev_b), Some(day_b)) = (prev.benchmark, day.benchmark) {
            let base_b = prev_b + day.flow;
            if base_b > Decimal::ZERO && day_b > Decimal::ZERO {
                bench_factor = bench_factor.map(|f| f * day_b / base_b);
            } else if day_b != base_b {
                bench_valid = false;
            }
        }
    }

    // IRR over the window: opening equity counts as an inflow. A window
    // that opens in negative equity has no honest IRR — report None.
    let first = &points[0];
    let last = points.last().expect("len >= 2");
    let opening = first.equity - first.flow; // value before day-1 flows
    let irr_pct = if opening < Decimal::ZERO {
        None
    } else {
        let mut cash_flows: Vec<(NaiveDate, f64)> = Vec::new();
        if opening > Decimal::ZERO {
            cash_flows.push((first.date, -f64::try_from(opening).unwrap_or(0.0)));
        }
        for point in points {
            if point.flow != Decimal::ZERO {
                cash_flows.push((point.date, -f64::try_from(point.flow).unwrap_or(0.0)));
            }
        }
        cash_flows.push((last.date, f64::try_from(last.equity).unwrap_or(0.0)));
        xirr(&cash_flows).map(|r| Decimal::try_from(r * 100.0).unwrap_or_default().round_dp(2))
    };

    let to_pct = |f: Decimal| ((f - Decimal::ONE) * Decimal::ONE_HUNDRED).round_dp(2);
    HistoryMetrics {
        twr_pct: twr_valid.then(|| to_pct(twr_factor)),
        irr_pct,
        benchmark_twr_pct: if bench_valid {
            bench_factor.map(to_pct)
        } else {
            None
        },
    }
}

/// Annualized internal rate of return for dated cash flows (negative =
/// money in, positive = money out / terminal value). Bisection: robust,
/// no derivative pathologies.
pub fn xirr(cash_flows: &[(NaiveDate, f64)]) -> Option<f64> {
    if cash_flows.len() < 2 {
        return None;
    }
    let has_negative = cash_flows.iter().any(|(_, v)| *v < 0.0);
    let has_positive = cash_flows.iter().any(|(_, v)| *v > 0.0);
    if !has_negative || !has_positive {
        return None;
    }
    let t0 = cash_flows[0].0;
    let npv = |rate: f64| -> f64 {
        cash_flows
            .iter()
            .map(|(date, value)| {
                let years = (*date - t0).num_days() as f64 / 365.0;
                value / (1.0 + rate).powf(years)
            })
            .sum()
    };

    let (mut lo, mut hi) = (-0.9999_f64, 1000.0_f64);
    let (npv_lo, npv_hi) = (npv(lo), npv(hi));
    if npv_lo.signum() == npv_hi.signum() {
        return None; // no root in a sane bracket
    }
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        let v = npv(mid);
        if v.abs() < 1e-9 {
            return Some(mid);
        }
        if v.signum() == npv_lo.signum() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some((lo + hi) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::dec;
    use chrono::{DateTime, Utc};

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn ts(s: &str) -> DateTime<Utc> {
        format!("{s}T15:00:00Z").parse().unwrap()
    }

    fn tx(id: i64, kind: TxKind, symbol: Option<&str>, qty: &str, price: &str, day: &str) -> Tx {
        Tx {
            id,
            kind,
            symbol: symbol.map(Into::into),
            qty: dec(qty),
            price: dec(price),
            fees: Decimal::ZERO,
            taxes: Decimal::ZERO,
            ts: ts(day),
            note: String::new(),
        }
    }

    fn closes(pairs: &[(&str, &str)]) -> Closes {
        pairs.iter().map(|(day, px)| (d(day), dec(px))).collect()
    }

    #[test]
    fn twr_ignores_flows_irr_feels_them() {
        // Day 1: deposit 1000, buy 10 @ 100. Price doubles by day 3.
        // Day 3: deposit another 1000 (after the gain) — TWR must not change
        // because of the deposit; IRR is dominated by the early money.
        let txs = vec![
            tx(1, TxKind::Deposit, None, "0", "1000", "2026-01-01"),
            tx(2, TxKind::Buy, Some("AAPL"), "10", "100", "2026-01-01"),
            tx(3, TxKind::Deposit, None, "0", "1000", "2026-01-03"),
        ];
        let mut by_symbol = HashMap::new();
        by_symbol.insert(
            "AAPL".to_string(),
            closes(&[
                ("2026-01-01", "100"),
                ("2026-01-02", "150"),
                ("2026-01-03", "200"),
            ]),
        );
        let (points, metrics) = equity_series(&txs, &by_symbol, None, d("2026-01-03"));

        assert_eq!(points.len(), 3);
        // Day 1: 10×100 = 1000 equity; day 3: 10×200 + 1000 cash = 3000.
        assert_eq!(points[0].equity, dec("1000"));
        assert_eq!(points[2].equity, dec("3000"));
        // TWR: day2 1500/1000, day3 (3000)/(1500+1000) = 1.2 → 1.5*1.2 = 1.8 → 80%.
        assert_eq!(metrics.twr_pct.unwrap(), dec("80.00"));
        // Annualized IRR for a 2-day doubling is astronomically large —
        // correctly out of bracket, reported as None rather than nonsense.
        assert!(metrics.irr_pct.is_none());
    }

    #[test]
    fn benchmark_matches_flows_not_trades() {
        // Deposit 1000 on day 1 (SPY at 100 → 10 shares), nothing else.
        // SPY at 110 on day 2 → benchmark 1100 regardless of what we traded.
        let txs = vec![
            tx(1, TxKind::Deposit, None, "0", "1000", "2026-01-01"),
            tx(2, TxKind::Buy, Some("MEME"), "100", "10", "2026-01-01"),
        ];
        let mut by_symbol = HashMap::new();
        by_symbol.insert(
            "MEME".to_string(),
            closes(&[("2026-01-01", "10"), ("2026-01-02", "5")]), // we got rekt
        );
        let spy = closes(&[("2026-01-01", "100"), ("2026-01-02", "110")]);
        let (points, metrics) = equity_series(&txs, &by_symbol, Some(&spy), d("2026-01-02"));

        assert_eq!(points[1].equity, dec("500")); // 100 × 5
        assert_eq!(points[1].benchmark.unwrap(), dec("1100")); // shoulda bought SPY
        assert_eq!(metrics.benchmark_twr_pct.unwrap(), dec("10.00"));
    }

    #[test]
    fn benchmark_flows_pend_until_first_close() {
        // Deposit lands on a day with NO benchmark candle yet (holiday /
        // pre-listing). The flow must wait and buy in at the first close —
        // not be dropped forever (the old one-way latch bug).
        let txs = vec![tx(1, TxKind::Deposit, None, "0", "1000", "2026-01-01")];
        let spy = closes(&[("2026-01-02", "100"), ("2026-01-03", "110")]);
        let (points, metrics) = equity_series(&txs, &HashMap::new(), Some(&spy), d("2026-01-03"));

        assert_eq!(points[0].benchmark, None); // no close yet → no fake $0
        assert_eq!(points[1].benchmark.unwrap(), dec("1000")); // bought in at 100
        assert_eq!(points[2].benchmark.unwrap(), dec("1100"));
        // Window starts before the benchmark exists → no benchmark TWR.
        assert!(metrics.benchmark_twr_pct.is_none());
        // ...but a window that starts at the first benchmark value has one.
        let windowed = metrics_for(&points[1..]);
        assert_eq!(windowed.benchmark_twr_pct.unwrap(), dec("10.00"));
    }

    fn point(day: &str, equity: &str, flow: &str) -> DayPoint {
        DayPoint {
            date: d(day),
            equity: dec(equity),
            cash: Decimal::ZERO,
            net_deposited: Decimal::ZERO,
            benchmark: None,
            flow: dec(flow),
        }
    }

    #[test]
    fn twr_declines_to_answer_through_zero_equity() {
        // Equity collapses below zero mid-window: no chain of positive
        // factors can represent that, so TWR must be None, not a spliced
        // number that silently skips the loss.
        let points = vec![
            point("2026-01-01", "100", "100"),
            point("2026-01-02", "-50", "0"),
            point("2026-01-03", "25", "0"),
        ];
        assert!(metrics_for(&points).twr_pct.is_none());

        // An empty account going flat (0 → 0, no flows) is fine to skip.
        let flat = vec![
            point("2026-01-01", "0", "0"),
            point("2026-01-02", "0", "0"),
            point("2026-01-03", "100", "100"),
        ];
        assert!(metrics_for(&flat).twr_pct.is_some());
    }

    #[test]
    fn irr_declines_negative_opening_equity() {
        let points = vec![
            point("2026-01-01", "-100", "0"),
            point("2026-06-01", "50", "0"),
        ];
        assert!(metrics_for(&points).irr_pct.is_none());
    }

    #[test]
    fn xirr_recovers_known_rate() {
        // -1000 grows to 1100 in exactly one year → 10%.
        let flows = vec![(d("2025-01-01"), -1000.0), (d("2026-01-01"), 1100.0)];
        let rate = xirr(&flows).unwrap();
        assert!((rate - 0.10).abs() < 1e-6, "got {rate}");
        // No sign change → no IRR.
        assert!(xirr(&[(d("2025-01-01"), 100.0), (d("2026-01-01"), 100.0)]).is_none());
    }

    #[test]
    fn valuation_falls_back_to_avg_cost_before_candles_exist() {
        let txs = vec![tx(1, TxKind::Buy, Some("NEW"), "10", "50", "2026-01-01")];
        let (points, _) = equity_series(&txs, &HashMap::new(), None, d("2026-01-02"));
        // No candles at all → both days valued at cost (cash is -500).
        assert_eq!(points[0].equity, dec("0")); // -500 cash + 500 position
        assert_eq!(points[1].equity, dec("0"));
    }
}
