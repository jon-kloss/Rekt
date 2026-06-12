//! Historical performance: equity series replay, time-weighted return,
//! money-weighted return (IRR), and the cash-flow-matched benchmark.
//!
//! Pure functions over the transaction log + cached daily closes — the
//! whole history is recomputable, never stored as truth (PLAN.md §7).

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::portfolio::{compute_basis, Tx, TxKind};

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
    let mut basis = compute_basis(&txs[..0]).expect("empty log is valid");
    let mut bench_shares = Decimal::ZERO;
    let mut bench_dirty = benchmark_closes.is_some();

    let mut date = start;
    while date <= end {
        // Advance the replay to include this day's transactions.
        let mut day_flow = Decimal::ZERO;
        let mut day_had_tx = false;
        while tx_idx < txs.len() && txs[tx_idx].ts.date_naive() <= date {
            let tx = &txs[tx_idx];
            match tx.kind {
                TxKind::Deposit => day_flow += tx.price,
                TxKind::Withdrawal => day_flow -= tx.price,
                _ => {}
            }
            tx_idx += 1;
            day_had_tx = true;
        }
        if day_had_tx {
            basis = match compute_basis(&txs[..tx_idx]) {
                Ok(b) => b,
                // An inconsistent prefix can't be valued; stop the series
                // here rather than charting garbage.
                Err(_) => break,
            };
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
        // at that day's close.
        let benchmark = benchmark_closes.and_then(|spy| {
            let close = close_at(spy, date)?;
            if bench_dirty && day_flow != Decimal::ZERO && close > Decimal::ZERO {
                bench_shares += day_flow / close;
                if bench_shares < Decimal::ZERO {
                    bench_shares = Decimal::ZERO; // over-withdrawal clamps to flat
                }
            }
            Some(bench_shares * close)
        });
        if benchmark.is_none() {
            bench_dirty = false;
        }

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
    let mut twr_factor = Decimal::ONE;
    let mut bench_factor: Option<Decimal> = points[0].benchmark.map(|_| Decimal::ONE);
    for window in points.windows(2) {
        let (prev, day) = (&window[0], &window[1]);
        let base = prev.equity + day.flow;
        if base > Decimal::ZERO && day.equity > Decimal::ZERO {
            twr_factor *= day.equity / base;
        }
        if let (Some(factor), Some(prev_b), Some(day_b)) =
            (bench_factor, prev.benchmark, day.benchmark)
        {
            let base_b = prev_b + day.flow;
            if base_b > Decimal::ZERO && day_b > Decimal::ZERO {
                bench_factor = Some(factor * day_b / base_b);
            }
        }
    }

    // IRR over the window: opening equity counts as an inflow.
    let first = &points[0];
    let last = points.last().expect("len >= 2");
    let mut cash_flows: Vec<(NaiveDate, f64)> = Vec::new();
    let opening = first.equity - first.flow; // value before day-1 flows
    if opening > Decimal::ZERO {
        cash_flows.push((first.date, -f64::try_from(opening).unwrap_or(0.0)));
    }
    for point in points {
        if point.flow != Decimal::ZERO {
            cash_flows.push((point.date, -f64::try_from(point.flow).unwrap_or(0.0)));
        }
    }
    cash_flows.push((last.date, f64::try_from(last.equity).unwrap_or(0.0)));
    let irr_pct =
        xirr(&cash_flows).map(|r| Decimal::try_from(r * 100.0).unwrap_or_default().round_dp(2));

    let to_pct = |f: Decimal| ((f - Decimal::ONE) * Decimal::ONE_HUNDRED).round_dp(2);
    HistoryMetrics {
        twr_pct: Some(to_pct(twr_factor)),
        irr_pct,
        benchmark_twr_pct: bench_factor.map(to_pct),
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
