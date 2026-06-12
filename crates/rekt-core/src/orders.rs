//! Order domain types and guardrails — pure, testable, broker-agnostic.
//!
//! Safety rails live here (PLAN.md §4): they are checked unconditionally on
//! the submission path, separate from any strategy/UI logic, and the
//! `trading_paused` flag is consulted by the server before anything else.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::portfolio::PortfolioBasis;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit,
}

impl OrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderType::Market => "market",
            OrderType::Limit => "limit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    Day,
    Gtc,
}

impl TimeInForce {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeInForce::Day => "day",
            TimeInForce::Gtc => "gtc",
        }
    }
}

/// What the user asked for — validated by guardrails before submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderTicket {
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    pub qty: Decimal,
    pub limit_price: Option<Decimal>,
    #[serde(default = "default_tif")]
    pub tif: TimeInForce,
}

fn default_tif() -> TimeInForce {
    TimeInForce::Day
}

/// Order lifecycle states, including the in-flight mutation states that
/// prevent fill-vs-cancel races (docs/RESEARCH.md §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Persisted locally, not yet acknowledged sent to the broker.
    PendingSubmit,
    Submitted,
    Accepted,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Expired,
    PendingCancel,
    Replaced,
    /// Local terminal state: submission failed and the broker confirms it
    /// never saw the order.
    Failed,
}

impl OrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderStatus::PendingSubmit => "pending_submit",
            OrderStatus::Submitted => "submitted",
            OrderStatus::Accepted => "accepted",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::Canceled => "canceled",
            OrderStatus::Rejected => "rejected",
            OrderStatus::Expired => "expired",
            OrderStatus::PendingCancel => "pending_cancel",
            OrderStatus::Replaced => "replaced",
            OrderStatus::Failed => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrderStatus::Filled
                | OrderStatus::Canceled
                | OrderStatus::Rejected
                | OrderStatus::Expired
                | OrderStatus::Replaced
                | OrderStatus::Failed
        )
    }
}

impl std::str::FromStr for OrderStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "pending_submit" => OrderStatus::PendingSubmit,
            "submitted" => OrderStatus::Submitted,
            "accepted" => OrderStatus::Accepted,
            "partially_filled" => OrderStatus::PartiallyFilled,
            "filled" => OrderStatus::Filled,
            "canceled" => OrderStatus::Canceled,
            "rejected" => OrderStatus::Rejected,
            "expired" => OrderStatus::Expired,
            "pending_cancel" => OrderStatus::PendingCancel,
            "replaced" => OrderStatus::Replaced,
            "failed" => OrderStatus::Failed,
            other => return Err(format!("unknown order status: {other}")),
        })
    }
}

/// Guardrail configuration. Defaults are deliberately conservative; all
/// overridable via environment (see rekt-server).
#[derive(Debug, Clone)]
pub struct Guardrails {
    /// Maximum notional value of a single order.
    pub max_order_notional: Decimal,
    /// Maximum resulting position as % of (cash + market value).
    pub max_position_pct: Decimal,
    /// Maximum orders submitted per calendar day (America/New_York).
    pub max_orders_per_day: u32,
    /// Circuit breaker (Freqtrade `StoplossGuard` pattern, PLAN.md §4):
    /// block new BUYS once today's realized loss exceeds this — sells stay
    /// allowed so a tripped breaker never locks you into a falling position.
    /// `None` disables.
    pub max_daily_loss: Option<Decimal>,
}

impl Default for Guardrails {
    fn default() -> Self {
        Self {
            max_order_notional: Decimal::from(10_000),
            max_position_pct: Decimal::from(25),
            max_orders_per_day: 20,
            max_daily_loss: Some(Decimal::from(1_000)),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum GuardrailViolation {
    #[error("trading is paused — resume it explicitly before submitting orders")]
    TradingPaused,
    #[error("qty must be positive")]
    NonPositiveQty,
    #[error("limit orders require a positive limit price")]
    MissingLimitPrice,
    #[error("market orders cannot carry a limit price")]
    UnexpectedLimitPrice,
    #[error("no price available for {symbol}; cannot evaluate order notional")]
    NoPrice { symbol: String },
    #[error("order notional ${notional} exceeds the ${max} per-order cap")]
    NotionalCap { notional: Decimal, max: Decimal },
    #[error("selling {want} {symbol} but only {have} held (long-only)")]
    LongOnly {
        symbol: String,
        have: Decimal,
        want: Decimal,
    },
    #[error("resulting {symbol} position would be {pct}% of equity (cap {max}%)")]
    PositionCap {
        symbol: String,
        pct: Decimal,
        max: Decimal,
    },
    #[error("daily order cap reached ({max} orders today)")]
    DailyCap { max: u32 },
    #[error(
        "circuit breaker: today's realized loss ${loss} exceeds the ${max} cap — \
         trading blocked until tomorrow (or raise REKT_MAX_DAILY_LOSS)"
    )]
    DailyLossBreaker { loss: Decimal, max: Decimal },
}

/// Estimate the per-share price an order will execute around: the limit
/// price for limit orders, the reference (last/quote) price for market.
pub fn reference_price(ticket: &OrderTicket, last_price: Option<Decimal>) -> Option<Decimal> {
    match ticket.order_type {
        OrderType::Limit => ticket.limit_price,
        OrderType::Market => last_price,
    }
}

/// Everything the guardrails need to judge a ticket.
pub struct TicketContext<'a> {
    pub basis: &'a PortfolioBasis,
    /// Last known price for the ticket's symbol (market-order reference).
    pub last_price: Option<Decimal>,
    /// Account equity (position-cap denominator).
    pub equity: Decimal,
    pub orders_today: u32,
    /// Realized P&L since the start of the trading day (breaker input).
    pub realized_today: Decimal,
    pub trading_paused: bool,
}

/// Check a ticket against every rail. Returns the estimated notional on
/// success so the UI confirm dialog can show it.
pub fn check_ticket(
    ticket: &OrderTicket,
    ctx: &TicketContext,
    rails: &Guardrails,
) -> Result<Decimal, GuardrailViolation> {
    let TicketContext {
        basis,
        last_price,
        equity,
        orders_today,
        realized_today,
        trading_paused,
    } = *ctx;
    if trading_paused {
        return Err(GuardrailViolation::TradingPaused);
    }
    // Buys only: a breaker that blocked sells would forbid REDUCING risk
    // exactly when the user most needs to exit a falling position.
    if matches!(ticket.side, Side::Buy) {
        if let Some(max_loss) = rails.max_daily_loss {
            if realized_today <= -max_loss {
                return Err(GuardrailViolation::DailyLossBreaker {
                    loss: -realized_today.round_dp(2),
                    max: max_loss,
                });
            }
        }
    }
    if ticket.qty <= Decimal::ZERO {
        return Err(GuardrailViolation::NonPositiveQty);
    }
    match ticket.order_type {
        OrderType::Limit if ticket.limit_price.unwrap_or_default() <= Decimal::ZERO => {
            return Err(GuardrailViolation::MissingLimitPrice);
        }
        OrderType::Market if ticket.limit_price.is_some() => {
            return Err(GuardrailViolation::UnexpectedLimitPrice);
        }
        _ => {}
    }
    if orders_today >= rails.max_orders_per_day {
        return Err(GuardrailViolation::DailyCap {
            max: rails.max_orders_per_day,
        });
    }

    let price = reference_price(ticket, last_price).ok_or_else(|| GuardrailViolation::NoPrice {
        symbol: ticket.symbol.clone(),
    })?;
    let notional = price * ticket.qty;
    if notional > rails.max_order_notional {
        return Err(GuardrailViolation::NotionalCap {
            notional: notional.round_dp(2),
            max: rails.max_order_notional,
        });
    }

    let held = basis
        .positions
        .get(&ticket.symbol)
        .map(|p| p.qty)
        .unwrap_or_default();

    match ticket.side {
        Side::Sell => {
            if ticket.qty > held {
                return Err(GuardrailViolation::LongOnly {
                    symbol: ticket.symbol.clone(),
                    have: held,
                    want: ticket.qty,
                });
            }
        }
        Side::Buy => {
            // Position cap: resulting position value vs current equity.
            if equity > Decimal::ZERO {
                let resulting = (held + ticket.qty) * price;
                let pct = (resulting / equity) * Decimal::ONE_HUNDRED;
                if pct > rails.max_position_pct {
                    return Err(GuardrailViolation::PositionCap {
                        symbol: ticket.symbol.clone(),
                        pct: pct.round_dp(1),
                        max: rails.max_position_pct,
                    });
                }
            }
        }
    }

    Ok(notional.round_dp(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::{compute_basis, Tx, TxKind};
    use crate::test_util::dec;
    use chrono::Utc;

    fn ticket(
        symbol: &str,
        side: Side,
        qty: &str,
        kind: OrderType,
        limit: Option<&str>,
    ) -> OrderTicket {
        OrderTicket {
            symbol: symbol.into(),
            side,
            order_type: kind,
            qty: dec(qty),
            limit_price: limit.map(dec),
            tif: TimeInForce::Day,
        }
    }

    fn basis_with(symbol: &str, qty: &str, price: &str) -> PortfolioBasis {
        let txs = vec![
            Tx {
                id: 1,
                kind: TxKind::Deposit,
                symbol: None,
                qty: Decimal::ZERO,
                price: dec("100000"),
                fees: Decimal::ZERO,
                taxes: Decimal::ZERO,
                ts: Utc::now(),
                note: String::new(),
            },
            Tx {
                id: 2,
                kind: TxKind::Buy,
                symbol: Some(symbol.into()),
                qty: dec(qty),
                price: dec(price),
                fees: Decimal::ZERO,
                taxes: Decimal::ZERO,
                ts: Utc::now(),
                note: String::new(),
            },
        ];
        compute_basis(&txs).unwrap()
    }

    #[test]
    fn happy_path_returns_notional() {
        let basis = basis_with("AAPL", "10", "100");
        let t = ticket("AAPL", Side::Buy, "5", OrderType::Limit, Some("150"));
        let notional = check_ticket(
            &t,
            &TicketContext {
                basis: &basis,
                last_price: None,
                equity: dec("100000"),
                orders_today: 0,
                realized_today: Decimal::ZERO,
                trading_paused: false,
            },
            &Guardrails::default(),
        );
        assert_eq!(notional.unwrap(), dec("750.00"));
    }

    #[test]
    fn paused_blocks_everything_first() {
        let basis = PortfolioBasis::default();
        let t = ticket("AAPL", Side::Buy, "1", OrderType::Market, None);
        assert_eq!(
            check_ticket(
                &t,
                &TicketContext {
                    basis: &basis,
                    last_price: Some(dec("100")),
                    equity: dec("1000"),
                    orders_today: 0,
                    realized_today: Decimal::ZERO,
                    trading_paused: true,
                },
                &Guardrails::default(),
            ),
            Err(GuardrailViolation::TradingPaused)
        );
    }

    #[test]
    fn long_only_blocks_oversell() {
        let basis = basis_with("AAPL", "5", "100");
        let t = ticket("AAPL", Side::Sell, "6", OrderType::Market, None);
        let err = check_ticket(
            &t,
            &TicketContext {
                basis: &basis,
                last_price: Some(dec("100")),
                equity: dec("100000"),
                orders_today: 0,
                realized_today: Decimal::ZERO,
                trading_paused: false,
            },
            &Guardrails::default(),
        );
        assert!(matches!(err, Err(GuardrailViolation::LongOnly { .. })));
    }

    #[test]
    fn notional_and_position_caps() {
        let basis = basis_with("AAPL", "10", "100");
        let rails = Guardrails {
            max_order_notional: dec("1000"),
            max_position_pct: dec("25"),
            max_orders_per_day: 20,
            max_daily_loss: None,
        };
        // Notional cap: 20 * 100 = 2000 > 1000.
        let t = ticket("AAPL", Side::Buy, "20", OrderType::Limit, Some("100"));
        assert!(matches!(
            check_ticket(
                &t,
                &TicketContext {
                    basis: &basis,
                    last_price: None,
                    equity: dec("100000"),
                    orders_today: 0,
                    realized_today: Decimal::ZERO,
                    trading_paused: false,
                },
                &rails,
            ),
            Err(GuardrailViolation::NotionalCap { .. })
        ));
        // Position cap: equity 2000, resulting position 5*100+held 10*100=1500 → 75%.
        let t = ticket("AAPL", Side::Buy, "5", OrderType::Limit, Some("100"));
        assert!(matches!(
            check_ticket(
                &t,
                &TicketContext {
                    basis: &basis,
                    last_price: None,
                    equity: dec("2000"),
                    orders_today: 0,
                    realized_today: Decimal::ZERO,
                    trading_paused: false,
                },
                &rails,
            ),
            Err(GuardrailViolation::PositionCap { .. })
        ));
    }

    #[test]
    fn daily_loss_breaker_trips_on_realized_losses() {
        let basis = basis_with("AAPL", "10", "100");
        let t = ticket("AAPL", Side::Buy, "1", OrderType::Limit, Some("100"));
        let rails = Guardrails::default(); // max_daily_loss = 1000
        let err = check_ticket(
            &t,
            &TicketContext {
                basis: &basis,
                last_price: None,
                equity: dec("100000"),
                orders_today: 0,
                realized_today: dec("-1500"),
                trading_paused: false,
            },
            &rails,
        );
        assert!(matches!(
            err,
            Err(GuardrailViolation::DailyLossBreaker { .. })
        ));
        // Sells stay allowed — the breaker must never lock the user into a
        // losing position.
        let sell = ticket("AAPL", Side::Sell, "5", OrderType::Limit, Some("100"));
        assert!(check_ticket(
            &sell,
            &TicketContext {
                basis: &basis,
                last_price: None,
                equity: dec("100000"),
                orders_today: 0,
                realized_today: dec("-1500"),
                trading_paused: false,
            },
            &rails,
        )
        .is_ok());
        // A profitable day doesn't trip it.
        assert!(check_ticket(
            &t,
            &TicketContext {
                basis: &basis,
                last_price: None,
                equity: dec("100000"),
                orders_today: 0,
                realized_today: dec("500"),
                trading_paused: false,
            },
            &rails,
        )
        .is_ok());
    }

    #[test]
    fn daily_cap_and_market_price_requirements() {
        let basis = basis_with("AAPL", "10", "100");
        let t = ticket("AAPL", Side::Buy, "1", OrderType::Market, None);
        assert!(matches!(
            check_ticket(
                &t,
                &TicketContext {
                    basis: &basis,
                    last_price: Some(dec("100")),
                    equity: dec("100000"),
                    orders_today: 20,
                    realized_today: Decimal::ZERO,
                    trading_paused: false,
                },
                &Guardrails::default(),
            ),
            Err(GuardrailViolation::DailyCap { .. })
        ));
        // Market order with no reference price cannot be risk-checked.
        assert!(matches!(
            check_ticket(
                &t,
                &TicketContext {
                    basis: &basis,
                    last_price: None,
                    equity: dec("100000"),
                    orders_today: 0,
                    realized_today: Decimal::ZERO,
                    trading_paused: false,
                },
                &Guardrails::default(),
            ),
            Err(GuardrailViolation::NoPrice { .. })
        ));
    }
}
