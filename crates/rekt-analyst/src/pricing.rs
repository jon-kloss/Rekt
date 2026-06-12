//! Cost metering: turn token usage into dollars so every analysis row can
//! carry an honest price tag and a daily budget can gate runs.
//!
//! Prices are per million tokens (cached 2026-06; update alongside model
//! bumps). Cache reads bill at 0.1x input, cache writes (5m TTL) at 1.25x.

use rust_decimal::Decimal;

use crate::UsageTotals;

/// (input $/MTok, output $/MTok) for the models REKT uses.
fn rates(model: &str) -> (Decimal, Decimal) {
    if model.starts_with("claude-haiku-4-5") {
        (Decimal::new(100, 2), Decimal::new(500, 2)) // $1.00 / $5.00
    } else if model.starts_with("claude-sonnet-4-6") {
        (Decimal::new(300, 2), Decimal::new(1500, 2)) // $3.00 / $15.00
    } else {
        // claude-opus-4-8 and a conservative default for anything unknown.
        (Decimal::new(500, 2), Decimal::new(2500, 2)) // $5.00 / $25.00
    }
}

/// Worst-case OUTPUT cost of one response (`max_tokens` × output rate) —
/// budget gates use it as headroom so an in-flight run can't sail
/// arbitrarily far past the ceiling. (Input cost is unbounded by nature —
/// web search results — and stays post-hoc.)
pub fn max_output_cost(model: &str, max_tokens: u32) -> Decimal {
    let (_, output_rate) = rates(model);
    (Decimal::from(max_tokens) * output_rate / Decimal::from(1_000_000u64)).round_dp(6)
}

pub fn cost_usd(model: &str, totals: &UsageTotals) -> Decimal {
    let (input_rate, output_rate) = rates(model);
    let million = Decimal::from(1_000_000u64);
    let cache_read_rate = input_rate * Decimal::new(1, 1); // 0.1x
    let cache_write_rate = input_rate * Decimal::new(125, 2); // 1.25x

    let cost = Decimal::from(totals.input_tokens) * input_rate
        + Decimal::from(totals.cache_read_tokens) * cache_read_rate
        + Decimal::from(totals.cache_write_tokens) * cache_write_rate
        + Decimal::from(totals.output_tokens) * output_rate;
    (cost / million).round_dp(6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_costs_more_than_haiku_and_cache_reads_are_cheap() {
        let totals = UsageTotals {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            requests: 1,
        };
        // Opus 4.8: $5 input + $2.50 output.
        assert_eq!(cost_usd("claude-opus-4-8", &totals), "7.5".parse().unwrap());
        // Haiku: $1 + $0.50.
        assert_eq!(
            cost_usd("claude-haiku-4-5", &totals),
            "1.5".parse().unwrap()
        );

        // The same million tokens served from cache cost a tenth.
        let cached = UsageTotals {
            input_tokens: 0,
            cache_read_tokens: 1_000_000,
            cache_write_tokens: 0,
            output_tokens: 0,
            requests: 1,
        };
        assert_eq!(cost_usd("claude-opus-4-8", &cached), "0.5".parse().unwrap());

        // Cache writes carry the 1.25x premium.
        let written = UsageTotals {
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 1_000_000,
            output_tokens: 0,
            requests: 1,
        };
        assert_eq!(
            cost_usd("claude-opus-4-8", &written),
            "6.25".parse().unwrap()
        );
    }
}
