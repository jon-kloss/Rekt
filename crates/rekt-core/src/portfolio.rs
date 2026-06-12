//! The portfolio engine: transactions in, positions/cash/P&L out.
//!
//! Pure and deterministic — the entire derived state is recomputable from
//! the transaction log (PLAN.md §7). Cost basis is FIFO lots in v1; the
//! engine is structured so other strategies (average, specific-lot) can
//! slot in later.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxKind {
    Buy,
    Sell,
    Dividend,
    Split,
    Deposit,
    Withdrawal,
}

impl TxKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TxKind::Buy => "buy",
            TxKind::Sell => "sell",
            TxKind::Dividend => "dividend",
            TxKind::Split => "split",
            TxKind::Deposit => "deposit",
            TxKind::Withdrawal => "withdrawal",
        }
    }

    pub fn needs_symbol(&self) -> bool {
        matches!(
            self,
            TxKind::Buy | TxKind::Sell | TxKind::Dividend | TxKind::Split
        )
    }
}

impl std::str::FromStr for TxKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "buy" => Ok(TxKind::Buy),
            "sell" => Ok(TxKind::Sell),
            "dividend" => Ok(TxKind::Dividend),
            "split" => Ok(TxKind::Split),
            "deposit" => Ok(TxKind::Deposit),
            "withdrawal" => Ok(TxKind::Withdrawal),
            other => Err(format!("unknown transaction kind: {other}")),
        }
    }
}

/// One row of the transaction log. Field semantics by kind:
///
/// | kind        | symbol | qty                    | price                |
/// |-------------|--------|------------------------|----------------------|
/// | buy/sell    | yes    | shares                 | per-share price      |
/// | dividend    | yes    | unused                 | total cash received  |
/// | split       | yes    | ratio (new per old)    | unused               |
/// | deposit     | no     | unused                 | total cash in        |
/// | withdrawal  | no     | unused                 | total cash out       |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tx {
    pub id: i64,
    pub kind: TxKind,
    pub symbol: Option<String>,
    pub qty: Decimal,
    pub price: Decimal,
    pub fees: Decimal,
    pub taxes: Decimal,
    pub ts: DateTime<Utc>,
    pub note: String,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PortfolioError {
    #[error("transaction {id}: {kind} requires a symbol")]
    MissingSymbol { id: i64, kind: &'static str },
    #[error("transaction {id}: selling {want} {symbol} but only {have} held")]
    Oversell {
        id: i64,
        symbol: String,
        have: Decimal,
        want: Decimal,
    },
    #[error("transaction {id}: split ratio must be positive, got {ratio}")]
    BadSplitRatio { id: i64, ratio: Decimal },
    #[error("transaction {id}: {field} must be positive")]
    NonPositive { id: i64, field: &'static str },
}

impl PortfolioError {
    /// The id of the transaction that failed validation.
    pub fn tx_id(&self) -> i64 {
        match self {
            PortfolioError::MissingSymbol { id, .. }
            | PortfolioError::Oversell { id, .. }
            | PortfolioError::BadSplitRatio { id, .. }
            | PortfolioError::NonPositive { id, .. } => *id,
        }
    }
}

/// An open FIFO tax lot.
#[derive(Debug, Clone, PartialEq)]
pub struct Lot {
    pub qty: Decimal,
    /// Per-share cost including capitalized fees/taxes.
    pub unit_cost: Decimal,
    pub acquired: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PositionBasis {
    pub qty: Decimal,
    pub cost_basis: Decimal,
    pub realized_pnl: Decimal,
    pub dividends: Decimal,
    #[serde(skip)]
    pub lots: Vec<Lot>,
}

impl PositionBasis {
    pub fn avg_cost(&self) -> Option<Decimal> {
        (self.qty > Decimal::ZERO).then(|| self.cost_basis / self.qty)
    }
}

/// Everything derivable from the transaction log alone (no market data).
#[derive(Debug, Clone, Default, Serialize)]
pub struct PortfolioBasis {
    pub cash: Decimal,
    /// Keyed by symbol. Retains flat (qty == 0) positions so their realized
    /// P&L and dividends still report.
    pub positions: BTreeMap<String, PositionBasis>,
    pub deposited: Decimal,
    pub withdrawn: Decimal,
}

/// Replay the transaction log into cash + FIFO lots + realized P&L.
///
/// `txs` must be ordered chronologically (ties broken by id) — the repo
/// layer guarantees this.
pub fn compute_basis(txs: &[Tx]) -> Result<PortfolioBasis, PortfolioError> {
    let mut book = PortfolioBasis::default();

    for tx in txs {
        let symbol = || -> Result<String, PortfolioError> {
            tx.symbol
                .as_deref()
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
                let cost = tx.qty * tx.price + tx.fees + tx.taxes;
                book.cash -= cost;
                let position = book.positions.entry(symbol).or_default();
                position.qty += tx.qty;
                position.cost_basis += cost;
                position.lots.push(Lot {
                    qty: tx.qty,
                    unit_cost: cost / tx.qty,
                    acquired: tx.ts,
                });
            }
            TxKind::Sell => {
                let symbol = symbol()?;
                if tx.qty <= Decimal::ZERO {
                    return Err(PortfolioError::NonPositive {
                        id: tx.id,
                        field: "qty",
                    });
                }
                let position = book.positions.entry(symbol.clone()).or_default();
                if tx.qty > position.qty {
                    return Err(PortfolioError::Oversell {
                        id: tx.id,
                        symbol,
                        have: position.qty,
                        want: tx.qty,
                    });
                }
                // Consume lots FIFO.
                let mut remaining = tx.qty;
                let mut basis_sold = Decimal::ZERO;
                while remaining > Decimal::ZERO {
                    let lot = position.lots.first_mut().expect("qty checked above");
                    let take = remaining.min(lot.qty);
                    basis_sold += take * lot.unit_cost;
                    lot.qty -= take;
                    remaining -= take;
                    if lot.qty == Decimal::ZERO {
                        position.lots.remove(0);
                    }
                }
                let proceeds = tx.qty * tx.price - tx.fees - tx.taxes;
                book.cash += proceeds;
                position.qty -= tx.qty;
                position.cost_basis -= basis_sold;
                position.realized_pnl += proceeds - basis_sold;
            }
            TxKind::Dividend => {
                let symbol = symbol()?;
                book.cash += tx.price;
                let position = book.positions.entry(symbol).or_default();
                position.dividends += tx.price;
            }
            TxKind::Split => {
                let symbol = symbol()?;
                if tx.qty <= Decimal::ZERO {
                    return Err(PortfolioError::BadSplitRatio {
                        id: tx.id,
                        ratio: tx.qty,
                    });
                }
                let position = book.positions.entry(symbol).or_default();
                position.qty *= tx.qty;
                for lot in &mut position.lots {
                    lot.qty *= tx.qty;
                    lot.unit_cost /= tx.qty;
                }
            }
            TxKind::Deposit => {
                book.cash += tx.price;
                book.deposited += tx.price;
            }
            TxKind::Withdrawal => {
                book.cash -= tx.price;
                book.withdrawn += tx.price;
            }
        }
    }

    Ok(book)
}

/// Latest known market prices, keyed by symbol.
#[derive(Debug, Clone, Default)]
pub struct PriceView {
    pub price: Decimal,
    pub prev_close: Option<Decimal>,
    pub ts: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PositionView {
    pub symbol: String,
    pub qty: Decimal,
    pub avg_cost: Option<Decimal>,
    pub cost_basis: Decimal,
    pub realized_pnl: Decimal,
    pub dividends: Decimal,
    pub price: Option<Decimal>,
    pub price_ts: Option<DateTime<Utc>>,
    pub market_value: Option<Decimal>,
    pub day_pnl: Option<Decimal>,
    pub unrealized_pnl: Option<Decimal>,
    pub unrealized_pnl_pct: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortfolioView {
    pub cash: Decimal,
    pub deposited: Decimal,
    pub withdrawn: Decimal,
    /// cash + market value of all priced positions.
    pub equity: Decimal,
    pub market_value: Decimal,
    pub day_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub dividends: Decimal,
    /// Held symbols we have no price for — equity excludes them, honestly.
    pub unpriced_symbols: Vec<String>,
    pub positions: Vec<PositionView>,
}

/// Combine the basis (from transactions) with current prices.
pub fn value(basis: &PortfolioBasis, prices: &HashMap<String, PriceView>) -> PortfolioView {
    let mut view = PortfolioView {
        cash: basis.cash,
        deposited: basis.deposited,
        withdrawn: basis.withdrawn,
        equity: basis.cash,
        market_value: Decimal::ZERO,
        day_pnl: Decimal::ZERO,
        unrealized_pnl: Decimal::ZERO,
        realized_pnl: Decimal::ZERO,
        dividends: Decimal::ZERO,
        unpriced_symbols: Vec::new(),
        positions: Vec::new(),
    };

    for (symbol, position) in &basis.positions {
        view.realized_pnl += position.realized_pnl;
        view.dividends += position.dividends;

        // Flat positions still contribute realized P&L above but don't
        // clutter the positions table.
        if position.qty == Decimal::ZERO {
            continue;
        }

        let quote = prices.get(symbol);
        let price = quote.map(|q| q.price);
        let market_value = price.map(|p| p * position.qty);
        let day_pnl = quote.and_then(|q| q.prev_close.map(|pc| (q.price - pc) * position.qty));
        let unrealized = market_value.map(|mv| mv - position.cost_basis);
        let unrealized_pct = unrealized.and_then(|u| {
            (position.cost_basis != Decimal::ZERO)
                .then(|| (u / position.cost_basis) * Decimal::ONE_HUNDRED)
        });

        if let Some(mv) = market_value {
            view.market_value += mv;
            view.equity += mv;
        } else {
            view.unpriced_symbols.push(symbol.clone());
        }
        if let Some(d) = day_pnl {
            view.day_pnl += d;
        }
        if let Some(u) = unrealized {
            view.unrealized_pnl += u;
        }

        view.positions.push(PositionView {
            symbol: symbol.clone(),
            qty: position.qty,
            avg_cost: position.avg_cost(),
            cost_basis: position.cost_basis,
            realized_pnl: position.realized_pnl,
            dividends: position.dividends,
            price,
            price_ts: quote.and_then(|q| q.ts),
            market_value,
            day_pnl,
            unrealized_pnl: unrealized,
            unrealized_pnl_pct: unrealized_pct,
        });
    }

    view
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn tx(id: i64, kind: TxKind, symbol: Option<&str>, qty: &str, price: &str) -> Tx {
        Tx {
            id,
            kind,
            symbol: symbol.map(Into::into),
            qty: dec(qty),
            price: dec(price),
            fees: Decimal::ZERO,
            taxes: Decimal::ZERO,
            ts: ts("2026-01-02T15:00:00Z"),
            note: String::new(),
        }
    }

    #[test]
    fn fifo_realized_pnl_across_two_lots() {
        let txs = vec![
            tx(1, TxKind::Deposit, None, "0", "10000"),
            tx(2, TxKind::Buy, Some("AAPL"), "10", "100"), // lot1: 10 @ 100
            tx(3, TxKind::Buy, Some("AAPL"), "10", "200"), // lot2: 10 @ 200
            tx(4, TxKind::Sell, Some("AAPL"), "15", "300"),
        ];
        let book = compute_basis(&txs).unwrap();
        let aapl = &book.positions["AAPL"];
        // Sold 10 from lot1 (basis 1000) + 5 from lot2 (basis 1000) = 2000;
        // proceeds 4500 → realized 2500.
        assert_eq!(aapl.realized_pnl, dec("2500"));
        assert_eq!(aapl.qty, dec("5"));
        assert_eq!(aapl.cost_basis, dec("1000")); // 5 left @ 200
                                                  // Cash: 10000 - 1000 - 2000 + 4500 = 11500.
        assert_eq!(book.cash, dec("11500"));
    }

    #[test]
    fn fees_capitalize_into_basis_and_reduce_proceeds() {
        let txs = vec![
            Tx {
                fees: dec("10"),
                ..tx(1, TxKind::Buy, Some("VOO"), "10", "100")
            },
            Tx {
                fees: dec("10"),
                ..tx(2, TxKind::Sell, Some("VOO"), "10", "110")
            },
        ];
        let book = compute_basis(&txs).unwrap();
        // Basis 1010, proceeds 1090 → realized 80.
        assert_eq!(book.positions["VOO"].realized_pnl, dec("80"));
    }

    #[test]
    fn split_preserves_cost_basis() {
        let txs = vec![
            tx(1, TxKind::Buy, Some("NVDA"), "10", "400"),
            tx(2, TxKind::Split, Some("NVDA"), "4", "0"), // 4:1
        ];
        let book = compute_basis(&txs).unwrap();
        let nvda = &book.positions["NVDA"];
        assert_eq!(nvda.qty, dec("40"));
        assert_eq!(nvda.cost_basis, dec("4000"));
        assert_eq!(nvda.avg_cost().unwrap(), dec("100"));
    }

    #[test]
    fn oversell_is_rejected_with_details() {
        let txs = vec![
            tx(1, TxKind::Buy, Some("AAPL"), "5", "100"),
            tx(2, TxKind::Sell, Some("AAPL"), "6", "100"),
        ];
        let err = compute_basis(&txs).unwrap_err();
        assert_eq!(
            err,
            PortfolioError::Oversell {
                id: 2,
                symbol: "AAPL".into(),
                have: dec("5"),
                want: dec("6"),
            }
        );
    }

    #[test]
    fn dividends_and_cash_flows() {
        let txs = vec![
            tx(1, TxKind::Deposit, None, "0", "5000"),
            tx(2, TxKind::Buy, Some("SCHD"), "10", "80"),
            tx(3, TxKind::Dividend, Some("SCHD"), "0", "25.50"),
            tx(4, TxKind::Withdrawal, None, "0", "1000"),
        ];
        let book = compute_basis(&txs).unwrap();
        assert_eq!(book.cash, dec("3225.50"));
        assert_eq!(book.positions["SCHD"].dividends, dec("25.50"));
        assert_eq!(book.deposited, dec("5000"));
        assert_eq!(book.withdrawn, dec("1000"));
    }

    #[test]
    fn valuation_with_partial_prices_is_honest() {
        let txs = vec![
            tx(1, TxKind::Buy, Some("AAPL"), "10", "100"),
            tx(2, TxKind::Buy, Some("MYST"), "10", "50"),
        ];
        let book = compute_basis(&txs).unwrap();
        let mut prices = HashMap::new();
        prices.insert(
            "AAPL".to_string(),
            PriceView {
                price: dec("110"),
                prev_close: Some(dec("105")),
                ts: None,
            },
        );

        let view = value(&book, &prices);
        assert_eq!(view.market_value, dec("1100"));
        assert_eq!(view.day_pnl, dec("50"));
        assert_eq!(view.unrealized_pnl, dec("100"));
        assert_eq!(view.unpriced_symbols, vec!["MYST".to_string()]);
        // Equity = cash (-1500) + priced market value (1100); MYST excluded.
        assert_eq!(view.equity, dec("-400"));
    }

    #[test]
    fn flat_positions_keep_realized_pnl_but_leave_the_table() {
        let txs = vec![
            tx(1, TxKind::Buy, Some("GME"), "10", "20"),
            tx(2, TxKind::Sell, Some("GME"), "10", "400"),
        ];
        let book = compute_basis(&txs).unwrap();
        let view = value(&book, &HashMap::new());
        assert!(view.positions.is_empty());
        assert_eq!(view.realized_pnl, dec("3800"));
    }
}
