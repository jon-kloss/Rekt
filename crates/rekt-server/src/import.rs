//! Broker CSV presets: translate Fidelity, Schwab, and Interactive Brokers
//! activity exports into the generic transaction shape, for track-only
//! accounts held outside Alpaca.
//!
//! Philosophy: mapped rows are all-or-nothing validated like the generic
//! import; rows a broker export legitimately contains but that aren't
//! portfolio transactions (interest, journal entries, disclaimers) are
//! SKIPPED and reported back — never silently dropped without a trace.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;

use crate::api::TxInput;

/// A preset parse: importable rows (tagged with their 1-based line number
/// in the original file, so validation errors name the line the user is
/// staring at) + human-readable skip reasons.
#[derive(Debug)]
pub struct PresetParse {
    pub rows: Vec<(usize, TxInput)>,
    pub skipped: Vec<String>,
}

pub fn parse_preset(format: &str, body: &str) -> Result<PresetParse, String> {
    tracing::debug!(format, bytes = body.len(), "parsing broker CSV preset");
    let parse = match format {
        "fidelity" => parse_broker(body, &FIDELITY),
        "schwab" => parse_broker(body, &SCHWAB),
        "robinhood" => parse_broker(body, &ROBINHOOD),
        "ibkr" => parse_ibkr(body),
        other => Err(format!(
            "unknown CSV format {other:?} (use generic, fidelity, schwab, robinhood or ibkr)"
        )),
    }?;
    tracing::debug!(
        format,
        rows = parse.rows.len(),
        skipped = parse.skipped.len(),
        "preset parsed"
    );
    Ok(parse)
}

/// What one broker's export looks like: how to find columns and how to map
/// an action string to a transaction kind.
struct BrokerPreset {
    name: &'static str,
    /// Column headers (lowercased, "($)" suffixes stripped) in priority
    /// order per field.
    date: &'static [&'static str],
    action: &'static [&'static str],
    symbol: &'static [&'static str],
    qty: &'static [&'static str],
    price: &'static [&'static str],
    fees: &'static [&'static str],
    commission: &'static [&'static str],
    amount: &'static [&'static str],
    /// Map an action to a kind, or None for "not a transaction" (skipped).
    /// Receives the row's SIGNED amount so a broker that uses one action code
    /// for both directions (e.g. Robinhood's `ACH`) can route by sign.
    classify: fn(&str, Option<Decimal>) -> Option<&'static str>,
}

static FIDELITY: BrokerPreset = BrokerPreset {
    name: "fidelity",
    date: &["run date", "date"],
    action: &["action"],
    symbol: &["symbol"],
    qty: &["quantity"],
    price: &["price"],
    fees: &["fees"],
    commission: &["commission"],
    amount: &["amount"],
    classify: |action, _amount| {
        let a = action.to_uppercase();
        // Fidelity exports a reinvested dividend as TWO rows: "DIVIDEND
        // RECEIVED" (cash in) + "REINVESTMENT" (buy, cash out). Importing
        // both is intentional and cash-neutral — do NOT dedupe them.
        if a.contains("BOUGHT") || a.contains("REINVESTMENT") {
            Some("buy")
        } else if a.contains("SOLD") {
            Some("sell")
        } else if a.contains("DIVIDEND") {
            Some("dividend")
        } else if a.contains("TRANSFER RECEIVED")
            || a.contains("DEPOSIT")
            || a.contains("CONTRIBUTION")
        {
            Some("deposit")
        } else if a.contains("TRANSFER PAID")
            || a.contains("WITHDRAWAL")
            || a.contains("DISTRIBUTION")
        {
            Some("withdrawal")
        } else {
            None
        }
    },
};

static SCHWAB: BrokerPreset = BrokerPreset {
    name: "schwab",
    date: &["date"],
    action: &["action"],
    symbol: &["symbol"],
    qty: &["quantity"],
    price: &["price"],
    fees: &["fees & comm", "fees"],
    commission: &[],
    amount: &["amount"],
    classify: |action, _amount| {
        let a = action.to_uppercase();
        // "Reinvest Dividend" is the cash credit; "Reinvest Shares" is the
        // matching purchase — both rows appear, so both map.
        if a.starts_with("BUY") || a == "REINVEST SHARES" {
            Some("buy")
        } else if a.starts_with("SELL") {
            Some("sell")
        } else if a.contains("DIVIDEND") {
            Some("dividend")
        } else if a.contains("WIRE RECEIVED")
            || a.contains("MONEYLINK TRANSFER")
            || a.contains("DEPOSIT")
        {
            Some("deposit")
        } else if a.contains("WIRE SENT") || a.contains("WITHDRAWAL") {
            Some("withdrawal")
        } else {
            None
        }
    },
};

static ROBINHOOD: BrokerPreset = BrokerPreset {
    name: "robinhood",
    // Robinhood's activity export: Activity Date, Process Date, Settle Date,
    // Instrument, Description, Trans Code, Quantity, Price, Amount.
    date: &["activity date", "process date"],
    action: &["trans code"],
    symbol: &["instrument"],
    qty: &["quantity"],
    price: &["price"],
    // No standalone fee columns — RH bakes regulatory fees into the amount.
    fees: &[],
    commission: &[],
    amount: &["amount"],
    classify: |action, amount| {
        match action.trim().to_uppercase().as_str() {
            "BUY" => Some("buy"),
            "SELL" => Some("sell"),
            "CDIV" => Some("dividend"), // cash dividend
            // Capital transfers — route by the amount's sign (credit + = money
            // in, debit − = money out). `ACH`/`RTP` are bank transfers; `ITRF`
            // is an account-to-account transfer (e.g. Brokerage → Roth IRA),
            // which for THIS account is a real withdrawal/deposit of capital.
            // Missing/zero amount → deposit (a `-0.00` is sign-negative but not
            // a debit, so it must not flip to a withdrawal).
            "ACH" | "RTP" | "ITRF" => Some(match amount {
                Some(a) if a.is_sign_negative() && !a.is_zero() => "withdrawal",
                _ => "deposit",
            }),
            // Everything else is reported as a skip, not silently dropped.
            // Deliberately NOT mapped as capital: interest (INT), Gold deposit
            // boosts (GDBP), credit-card cashback (XENT_CC), plan credits/
            // adjustments (GMPC/IADJ), and Gold fees (GOLD) are income/expense,
            // not contributed capital — importing them as deposits/withdrawals
            // would distort the `deposited` basis and the TWR/IRR built on it.
            // Also skipped: options (BTO/STO/BTC/STC/OEXP), tax withholding
            // (DTAX), and corporate actions (SPR/MRGS/SOFF/SDIV/BCXL/SPL) — RH
            // reports a share delta, not a ratio, so splits are deferred.
            _ => None,
        }
    },
};

/// "Price ($)" → "price"; "Fees & Comm" → "fees & comm".
fn canonical_header(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('\u{feff}') // BOM on the first header
        .to_lowercase()
        .replace("($)", "")
        .trim()
        .to_string()
}

/// "$1,234.56" → 1234.56; "(123.45)" and "-$5" → negative; "" → None.
fn parse_money(raw: &str) -> Result<Option<Decimal>, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let negative = s.starts_with('(') && s.ends_with(')');
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    if cleaned.is_empty() || cleaned == "-" {
        return Err(format!("unparseable number {raw:?}"));
    }
    let value: Decimal = cleaned
        .parse()
        .map_err(|_| format!("unparseable number {raw:?}"))?;
    // Parens force negative even when a minus also survived the filter —
    // "(-$5.00)" must come out -5, not flip back to +5.
    Ok(Some(if negative { -value.abs() } else { value }))
}

/// "06/12/2026" (Fidelity/Schwab, optionally "… as of 06/15/2026") or
/// "2026-06-10, 10:30:00" (IBKR Date/Time) → 4pm New York, as UTC. The
/// first space- or comma-delimited token is the date.
fn parse_broker_date(raw: &str) -> Result<DateTime<Utc>, String> {
    // The date is the first token; the time (after a space, comma, or the
    // IBKR Flex semicolon) is dropped — all rows normalize to the 4pm close.
    let first = raw
        .split([' ', ',', ';'])
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let date = NaiveDate::parse_from_str(first, "%m/%d/%Y")
        .or_else(|_| NaiveDate::parse_from_str(first, "%Y-%m-%d"))
        .or_else(|_| NaiveDate::parse_from_str(first, "%Y%m%d")) // IBKR Flex Query
        .map_err(|_| format!("unparseable date {raw:?}"))?;
    let close = NaiveTime::from_hms_opt(16, 0, 0).expect("valid time");
    let ny = New_York
        .from_local_datetime(&date.and_time(close))
        .single()
        .ok_or_else(|| format!("ambiguous local time for {raw:?}"))?;
    Ok(ny.with_timezone(&Utc))
}

fn parse_broker(body: &str, preset: &BrokerPreset) -> Result<PresetParse, String> {
    // Broker exports wrap the table in preamble/disclaimer lines; find the
    // real header row (the first line containing the action column name).
    let header_idx = body
        .lines()
        .position(|line| {
            let lower = line.to_lowercase();
            preset.action.iter().any(|a| lower.contains(a))
                && preset.date.iter().any(|d| lower.contains(d))
        })
        .ok_or_else(|| {
            format!(
                "no {} header row found — is this really a {} activity export?",
                preset.name, preset.name
            )
        })?;
    let table: String = body.lines().skip(header_idx).collect::<Vec<_>>().join("\n");

    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .flexible(true) // disclaimer/total lines have fewer fields
        .from_reader(table.as_bytes());

    let headers: Vec<String> = reader
        .headers()
        .map_err(|e| format!("bad header row: {e}"))?
        .iter()
        .map(canonical_header)
        .collect();
    let find = |names: &[&str]| -> Option<usize> {
        names
            .iter()
            .find_map(|n| headers.iter().position(|h| h == n))
    };
    // Value-bearing columns are REQUIRED: a mangled export missing the
    // Amount column must fail loudly, not import zero-value transactions.
    let col_date = find(preset.date).ok_or("missing date column")?;
    let col_action = find(preset.action).ok_or("missing action column")?;
    let col_symbol = Some(find(preset.symbol).ok_or("missing symbol column")?);
    let col_qty = Some(find(preset.qty).ok_or("missing quantity column")?);
    let col_price = Some(find(preset.price).ok_or("missing price column")?);
    let col_amount = Some(find(preset.amount).ok_or("missing amount column")?);
    // Fees/commission stay optional: Schwab has no commission column.
    let col_fees = find(preset.fees);
    let col_commission = find(preset.commission);

    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    for (i, record) in reader.records().enumerate() {
        let line = header_idx + i + 2; // 1-based line in the original file
        let record = record.map_err(|e| format!("line {line}: {e}"))?;
        let field = |idx: Option<usize>| idx.and_then(|i| record.get(i)).unwrap_or("").trim();

        let action = field(Some(col_action));
        let date_raw = field(Some(col_date));
        if action.is_empty() {
            continue; // blank separator, totals row or disclaimer line
        }
        let money = |idx: Option<usize>, what: &str| -> Result<Option<Decimal>, String> {
            parse_money(field(idx)).map_err(|m| format!("line {line}: {what}: {m}"))
        };

        // Classify on a LENIENT amount (parse errors → None): classify sees the
        // sign so a one-code-both-directions broker (Robinhood ACH) can route
        // by it, but a non-transaction row whose amount cell is junk ("--",
        // "N/A") must still be skippable, not hard-fail the whole file. Rows we
        // keep re-parse the amount strictly below, so a real transaction with a
        // malformed amount still fails loudly.
        let amount_for_class = parse_money(field(col_amount)).ok().flatten();
        let Some(kind) = (preset.classify)(action, amount_for_class) else {
            skipped.push(format!(
                "line {line}: \"{action}\" (not a portfolio transaction)"
            ));
            continue;
        };
        let ts = parse_broker_date(date_raw).map_err(|m| format!("line {line}: {m}"))?;

        let qty = money(col_qty, "quantity")?.map(|d| d.abs()); // sells export negative qty
        let price = money(col_price, "price")?;
        let amount = money(col_amount, "amount")?.map(|d| d.abs());
        let fees = Some(
            money(col_fees, "fees")?.unwrap_or_default()
                + money(col_commission, "commission")?.unwrap_or_default(),
        );
        let symbol = Some(field(col_symbol).to_string()).filter(|s| !s.is_empty());

        rows.push((
            line,
            TxInput {
                kind: kind.to_string(),
                symbol,
                qty,
                // dividends/deposits/withdrawals carry their cash in `price`.
                price: match kind {
                    "buy" | "sell" => price,
                    _ => amount,
                },
                fees,
                taxes: None,
                ts: Some(ts),
                note: Some(format!("{}: {}", preset.name, action)),
            },
        ));
    }
    Ok(PresetParse { rows, skipped })
}

/// Leading ticker of an IBKR dividend description: the text before the
/// ISIN/CUSIP parenthesis that always follows the symbol, e.g.
/// "AAPL(US0378331005) Cash Dividend …" → "AAPL". The split (rather than a
/// character class) preserves IBKR's space-bearing class tickers like
/// "BRK B", which must match the Trades Symbol column. No paren → not the
/// expected format → None (reported, not guessed).
fn ticker_prefix(desc: &str) -> Option<String> {
    let (ticker, _) = desc.split_once('(')?;
    let ticker = ticker.trim();
    (!ticker.is_empty()).then(|| ticker.to_string())
}

/// Sections whose rows are real cash or position events a user would
/// expect to import, but that this preset does not (yet) model — REPORTED
/// as skips (like Withholding Tax) so the gap is visible, never silently
/// swallowed. Splits in particular would desync share counts; IBKR reports
/// net-new shares rather than the ratio `TxKind::Split` wants, so mapping
/// them is deferred, but the omission is surfaced.
const REPORTED_SECTIONS: &[&str] = &[
    "Corporate Actions",
    "Interest",
    "Broker Interest Paid",
    "Broker Interest Received",
    "Payment In Lieu Of Dividends",
    "Fees",
    "Other Fees",
    "Commission Adjustments",
];

/// A currency cell that is actually a section subtotal ("Total",
/// "Total in USD") rather than a real ISO currency.
fn is_total_row(currency: &str) -> bool {
    currency.is_empty() || currency.to_ascii_uppercase().starts_with("TOTAL")
}

/// Interactive Brokers Activity Statement: a multi-section CSV where every
/// row is prefixed with its section name and a row type ("Header"/"Data"/
/// "SubTotal"/"Total"). Each section carries its own Header row, so a
/// column means different things in different sections.
///
/// We translate the three transaction-bearing sections — Trades,
/// Dividends, Deposits & Withdrawals — into the generic shape. Structural
/// sections (Open Positions, Net Asset Value, Account Information, …) are
/// ignored; cash/position sections we don't model (Corporate Actions,
/// Interest, Withholding Tax, …) are REPORTED as skips so the gap is
/// visible. USD and stocks/ETFs only; options, forex, non-USD rows, and
/// trade cancels/adjustments are skipped and reported.
fn parse_ibkr(body: &str) -> Result<PresetParse, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(body.as_bytes());

    // Stream once, resolving each Data row against the header most recently
    // seen FOR ITS SECTION. A combined multi-account statement repeats
    // section headers (possibly with different layouts), so applying them
    // in document order is the only correct mapping — a global last-wins
    // map would reindex an earlier account's rows. Each Data row becomes a
    // map keyed by lowercased column name; a row shorter than its header
    // (flexible CSV drops trailing empties) simply lacks those keys, which
    // resolve to "" — a safe skip, never a stale offset. The csv reader
    // (not a line split) owns tokenizing, since Date/Time embeds commas.
    let mut headers: HashMap<String, Vec<String>> = HashMap::new();
    let mut data: Vec<(u64, String, HashMap<String, String>)> = Vec::new();
    for result in reader.records() {
        let record = result.map_err(|e| format!("bad IBKR row: {e}"))?;
        let line = record.position().map(|p| p.line()).unwrap_or(0);
        let fields: Vec<String> = record
            .iter()
            .map(|s| s.trim().trim_start_matches('\u{feff}').to_string())
            .collect();
        if fields.len() < 2 {
            continue;
        }
        if fields[1].eq_ignore_ascii_case("header") {
            headers.insert(fields[0].clone(), fields[2..].to_vec());
        } else if fields[1].eq_ignore_ascii_case("data") {
            let Some(cols) = headers.get(&fields[0]) else {
                continue; // a Data row before its section's Header
            };
            let map = cols
                .iter()
                .enumerate()
                .filter_map(|(c, name)| {
                    fields
                        .get(c + 2)
                        .map(|v| (name.to_ascii_lowercase(), v.clone()))
                })
                .collect();
            data.push((line, fields[0].clone(), map));
        }
        // Header/SubTotal/Total/Notes contribute no transactions.
    }
    if !data.iter().any(|(_, s, _)| {
        matches!(
            s.as_str(),
            "Trades" | "Dividends" | "Deposits & Withdrawals"
        )
    }) {
        return Err(
            "no IBKR Trades/Dividends/Deposits & Withdrawals sections found \
             — is this an Interactive Brokers Activity Statement?"
                .to_string(),
        );
    }

    // Trades come at order grain AND per-execution / per-lot detail grain;
    // importing more than one double-counts. Prefer "Order" rows, falling
    // back to "Trade" when a statement isn't order-grouped (a single
    // statement is grouped one way). "ClosedLot" is always lot detail.
    let has_order = data.iter().any(|(_, s, m)| {
        s == "Trades" && m.get("datadiscriminator").map(String::as_str) == Some("Order")
    });
    let preferred_disc = if has_order { "Order" } else { "Trade" };

    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    for (line, section, m) in &data {
        let get = |name: &str| m.get(name).map(|s| s.trim()).unwrap_or("");
        let money = |name: &str, what: &str| -> Result<Option<Decimal>, String> {
            parse_money(get(name)).map_err(|msg| format!("line {line}: {what}: {msg}"))
        };
        match section.as_str() {
            "Trades" => {
                let symbol = get("symbol").to_string();
                let disc = get("datadiscriminator");
                if !disc.is_empty() && disc != preferred_disc {
                    if disc == "ClosedLot" || disc == "Order" || disc == "Trade" {
                        continue; // lot detail, or the same trade at the other grain
                    }
                    // Trade Cancel / Adjustment / Bust: a real event we
                    // don't net — surface it rather than drop it silently.
                    skipped.push(format!(
                        "line {line}: {symbol} \"{disc}\" (reconcile manually)"
                    ));
                    continue;
                }
                let asset = get("asset category");
                if !asset.eq_ignore_ascii_case("Stocks") {
                    skipped.push(format!(
                        "line {line}: {symbol} ({asset}) — only stocks & ETFs import"
                    ));
                    continue;
                }
                let currency = get("currency");
                if !currency.eq_ignore_ascii_case("USD") {
                    skipped.push(format!(
                        "line {line}: {symbol} priced in {currency} — only USD imports"
                    ));
                    continue;
                }
                let Some(qty) = money("quantity", "quantity")? else {
                    continue; // blank quantity is not a trade
                };
                if qty.is_zero() {
                    continue; // a zero-quantity adjustment row is not a trade
                }
                let ts =
                    parse_broker_date(get("date/time")).map_err(|m| format!("line {line}: {m}"))?;
                let fees = money("comm/fee", "commission")?.unwrap_or_default().abs();
                rows.push((
                    *line as usize,
                    TxInput {
                        // IBKR signs quantity: negative is a sale.
                        kind: if qty.is_sign_negative() {
                            "sell"
                        } else {
                            "buy"
                        }
                        .to_string(),
                        symbol: Some(symbol),
                        qty: Some(qty.abs()),
                        price: money("t. price", "price")?,
                        fees: Some(fees),
                        taxes: None,
                        ts: Some(ts),
                        note: Some("ibkr: trade".to_string()),
                    },
                ));
            }
            "Dividends" => {
                let currency = get("currency");
                if is_total_row(currency) {
                    continue;
                }
                if !currency.eq_ignore_ascii_case("USD") {
                    skipped.push(format!(
                        "line {line}: dividend in {currency} — only USD imports"
                    ));
                    continue;
                }
                let desc = get("description");
                let Some(amount) = money("amount", "amount")? else {
                    continue;
                };
                let Some(symbol) = ticker_prefix(desc) else {
                    skipped.push(format!("line {line}: dividend with no ticker in {desc:?}"));
                    continue;
                };
                let ts = parse_broker_date(get("date")).map_err(|m| format!("line {line}: {m}"))?;
                rows.push((
                    *line as usize,
                    TxInput {
                        kind: "dividend".to_string(),
                        symbol: Some(symbol),
                        qty: None,
                        price: Some(amount), // dividend cash; reversals stay negative
                        fees: None,
                        taxes: None,
                        ts: Some(ts),
                        note: Some(format!("ibkr: {desc}")), // CUSIP + per-share rate, for audit
                    },
                ));
            }
            "Deposits & Withdrawals" => {
                let currency = get("currency");
                if is_total_row(currency) {
                    continue;
                }
                if !currency.eq_ignore_ascii_case("USD") {
                    skipped.push(format!(
                        "line {line}: cash movement in {currency} — only USD imports"
                    ));
                    continue;
                }
                let Some(amount) = money("amount", "amount")? else {
                    continue;
                };
                let desc = get("description");
                let ts = parse_broker_date(get("settle date"))
                    .map_err(|m| format!("line {line}: {m}"))?;
                rows.push((
                    *line as usize,
                    TxInput {
                        kind: if amount.is_sign_negative() {
                            "withdrawal"
                        } else {
                            "deposit"
                        }
                        .to_string(),
                        symbol: None,
                        qty: None,
                        price: Some(amount.abs()),
                        fees: None,
                        taxes: None,
                        ts: Some(ts),
                        note: Some(format!("ibkr: {desc}")),
                    },
                ));
            }
            "Withholding Tax" => {
                // Dividends import gross; withholding is cash the user can
                // reconcile separately. Reported, never silently dropped.
                let currency = get("currency");
                if is_total_row(currency) {
                    continue;
                }
                skipped.push(format!(
                    "line {line}: withholding tax {:?} (dividends import gross)",
                    get("description")
                ));
            }
            section if REPORTED_SECTIONS.contains(&section) => {
                // A real cash/position event we don't model — surface it so
                // the gap is visible (a total/subtotal Data row is not).
                let currency = get("currency");
                if !currency.is_empty() && is_total_row(currency) {
                    continue;
                }
                skipped.push(format!(
                    "line {line}: {section} {:?} (not imported — reconcile manually)",
                    get("description")
                ));
            }
            _ => {} // structural section (Open Positions, NAV, …) — not a transaction
        }
    }
    Ok(PresetParse { rows, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_and_date_parsing() {
        assert_eq!(
            parse_money("$1,234.56").unwrap().unwrap().to_string(),
            "1234.56"
        );
        assert_eq!(
            parse_money("(123.45)").unwrap().unwrap().to_string(),
            "-123.45"
        );
        assert_eq!(parse_money("-$5").unwrap().unwrap().to_string(), "-5");
        // Parens win even with an embedded minus (contra/correction rows).
        assert_eq!(
            parse_money("(-$5.00)").unwrap().unwrap().to_string(),
            "-5.00"
        );
        assert_eq!(parse_money("").unwrap(), None);
        assert!(parse_money("abc").is_err());
        // 4pm ET June (EDT, UTC-4) → 20:00Z.
        let ts = parse_broker_date("06/12/2026").unwrap();
        assert_eq!(ts.to_rfc3339(), "2026-06-12T20:00:00+00:00");
        assert!(parse_broker_date("12 Jun 2026").is_err());
        // IBKR "Date/Time" with a comma between date and time.
        let ts = parse_broker_date("2026-06-10, 10:30:00").unwrap();
        assert_eq!(ts.to_rfc3339(), "2026-06-10T20:00:00+00:00");
        // IBKR Flex Query "YYYYMMDD;HHMMSS" and the bare date.
        assert_eq!(
            parse_broker_date("20260610;103000").unwrap().to_rfc3339(),
            "2026-06-10T20:00:00+00:00"
        );
        assert_eq!(
            parse_broker_date("20260610").unwrap().to_rfc3339(),
            "2026-06-10T20:00:00+00:00"
        );
        // Fidelity "… as of …" still takes the first token.
        assert_eq!(
            parse_broker_date("06/12/2026 as of 06/15/2026")
                .unwrap()
                .to_rfc3339(),
            "2026-06-12T20:00:00+00:00"
        );
    }

    #[test]
    fn ticker_prefix_keeps_space_bearing_class_tickers() {
        assert_eq!(
            ticker_prefix("AAPL(US0378331005) Cash Dividend USD 0.24 per Share").as_deref(),
            Some("AAPL")
        );
        // Class B shares: IBKR writes "BRK B", matching the Symbol column.
        assert_eq!(
            ticker_prefix("BRK B(US0846707026) Cash Dividend").as_deref(),
            Some("BRK B")
        );
        // No parenthesis → not the expected format → reported, not guessed.
        assert_eq!(ticker_prefix("Some narrative with no ticker"), None);
    }

    #[test]
    fn ibkr_reports_cancels_corporate_actions_and_cash_sections() {
        let csv = "\
Trades,Header,DataDiscriminator,Asset Category,Currency,Symbol,Date/Time,Quantity,T. Price,Comm/Fee\n\
Trades,Data,Order,Stocks,USD,AAPL,\"2026-06-10, 10:30:00\",10,150.25,-1\n\
Trades,Data,Trade Cancel,Stocks,USD,AAPL,\"2026-06-10, 10:30:00\",-10,150.25,1\n\
Trades,Data,Order,Stocks,USD,MSFT,\"2026-06-11, 10:30:00\",0,400.00,0\n\
Corporate Actions,Header,Asset Category,Currency,Report Date,Description,Quantity\n\
Corporate Actions,Data,Stocks,USD,2026-06-12,NVDA(US67066G1040) Split 4 for 1,30\n\
Interest,Header,Currency,Date,Description,Amount\n\
Interest,Data,USD,2026-06-30,USD Credit Interest,1.42\n\
Interest,Data,Total,,,1.42\n";
        let parse = parse_preset("ibkr", csv).unwrap();
        // Only the AAPL order imports; the zero-qty MSFT order is dropped.
        assert_eq!(parse.rows.len(), 1, "{:?}", parse.rows);
        assert_eq!(parse.rows[0].1.symbol.as_deref(), Some("AAPL"));
        // The cancel, the split, and the interest are all REPORTED.
        assert!(
            parse.skipped.iter().any(|s| s.contains("Trade Cancel")),
            "{:?}",
            parse.skipped
        );
        assert!(parse
            .skipped
            .iter()
            .any(|s| s.contains("Corporate Actions")));
        assert!(parse.skipped.iter().any(|s| s.contains("Interest")));
        // The Interest total row is not double-reported.
        assert_eq!(
            parse
                .skipped
                .iter()
                .filter(|s| s.contains("Interest"))
                .count(),
            1
        );
    }

    #[test]
    fn ibkr_multi_account_headers_apply_in_document_order() {
        // Two accounts whose Trades layouts differ (the second swaps the
        // last two columns). A global last-header-wins map would parse the
        // first account's row at the wrong offsets; per-section running
        // headers keep each row aligned to its own header.
        let csv = "\
Trades,Header,DataDiscriminator,Asset Category,Currency,Symbol,Date/Time,Quantity,T. Price,Comm/Fee\n\
Trades,Data,Order,Stocks,USD,AAPL,\"2026-06-10, 10:30:00\",10,150.25,-1\n\
Trades,Header,DataDiscriminator,Asset Category,Currency,Symbol,Date/Time,Quantity,Comm/Fee,T. Price\n\
Trades,Data,Order,Stocks,USD,MSFT,\"2026-06-11, 10:30:00\",5,-2,400.00\n";
        let parse = parse_preset("ibkr", csv).unwrap();
        assert_eq!(parse.rows.len(), 2);
        assert_eq!(parse.rows[0].1.price.unwrap().to_string(), "150.25");
        assert_eq!(parse.rows[0].1.fees.unwrap().to_string(), "1");
        // MSFT's price is in the LAST column under the swapped header.
        assert_eq!(parse.rows[1].1.price.unwrap().to_string(), "400.00");
        assert_eq!(parse.rows[1].1.fees.unwrap().to_string(), "2");
    }

    #[test]
    fn ibkr_activity_statement_maps_three_sections() {
        let csv = "\
Statement,Header,Field Name,Field Value\n\
Statement,Data,BrokerName,Interactive Brokers LLC\n\
Account Information,Header,Field Name,Field Value\n\
Account Information,Data,Account,U1234567\n\
Trades,Header,DataDiscriminator,Asset Category,Currency,Symbol,Date/Time,Quantity,T. Price,C. Price,Proceeds,Comm/Fee,Basis,Realized P/L,Code\n\
Trades,Data,Order,Stocks,USD,AAPL,\"2026-06-10, 10:30:00\",10,150.25,150.30,-1502.50,-1,1503.50,0,O\n\
Trades,Data,Order,Stocks,USD,VOO,\"2026-06-11, 14:00:00\",-5,520.10,520.00,2600.47,-1.03,-2550,49.44,C\n\
Trades,Data,ClosedLot,Stocks,USD,VOO,\"2026-06-11, 14:00:00\",-5,520.10,,,,2550,,C\n\
Trades,Data,Order,Equity and Index Options,USD,AAPL 240920C,\"2026-06-10, 10:30:00\",1,2.50,,-250,-1,251,0,O\n\
Trades,Data,Order,Stocks,EUR,ASML,\"2026-06-10, 09:00:00\",2,900.00,,-1800,-2,1802,0,O\n\
Trades,SubTotal,,Stocks,USD,AAPL,,10,,,,,,,\n\
Dividends,Header,Currency,Date,Description,Amount\n\
Dividends,Data,USD,2026-06-09,SCHD(US8085247976) Cash Dividend USD 0.27 per Share (Ordinary Dividend),25.50\n\
Dividends,Data,Total,,,25.50\n\
Withholding Tax,Header,Currency,Date,Description,Amount,Code\n\
Withholding Tax,Data,USD,2026-06-09,SCHD(US8085247976) Cash Dividend - US Tax,-3.83,\n\
Deposits & Withdrawals,Header,Currency,Settle Date,Description,Amount\n\
Deposits & Withdrawals,Data,USD,2026-06-08,Electronic Fund Transfer,5000\n\
Deposits & Withdrawals,Data,USD,2026-06-12,Disbursement,-1000\n\
Deposits & Withdrawals,Data,Total,,,4000\n";
        let parse = parse_preset("ibkr", csv).unwrap();
        assert_eq!(parse.rows.len(), 5, "skipped: {:?}", parse.skipped);

        let (_, buy) = &parse.rows[0];
        assert_eq!(buy.kind, "buy");
        assert_eq!(buy.symbol.as_deref(), Some("AAPL"));
        assert_eq!(buy.qty.unwrap().to_string(), "10");
        assert_eq!(buy.price.unwrap().to_string(), "150.25");
        assert_eq!(buy.fees.unwrap().to_string(), "1"); // abs(Comm/Fee)

        let (_, sell) = &parse.rows[1];
        assert_eq!(sell.kind, "sell");
        assert_eq!(sell.qty.unwrap().to_string(), "5"); // abs of -5
        assert_eq!(sell.fees.unwrap().to_string(), "1.03");

        let (_, dividend) = &parse.rows[2];
        assert_eq!(dividend.kind, "dividend");
        assert_eq!(dividend.symbol.as_deref(), Some("SCHD")); // ticker prefix
        assert_eq!(dividend.price.unwrap().to_string(), "25.50");

        assert_eq!(parse.rows[3].1.kind, "deposit");
        assert_eq!(parse.rows[3].1.price.unwrap().to_string(), "5000");
        assert!(parse.rows[3].1.symbol.is_none());
        assert_eq!(parse.rows[4].1.kind, "withdrawal");
        assert_eq!(parse.rows[4].1.price.unwrap().to_string(), "1000"); // abs

        // The ClosedLot detail and the SubTotal/Total rows are silently
        // structural; the option, the EUR trade, and the withholding tax
        // are reported skips.
        assert_eq!(parse.skipped.len(), 3, "{:?}", parse.skipped);
        assert!(parse.skipped.iter().any(|s| s.contains("Options")));
        assert!(parse.skipped.iter().any(|s| s.contains("EUR")));
        assert!(parse.skipped.iter().any(|s| s.contains("withholding")));
    }

    #[test]
    fn ibkr_prefers_order_rows_over_trade_detail() {
        // A statement grouped by orders also lists per-execution Trade
        // rows; importing both would double-count the position.
        let csv = "\
Trades,Header,DataDiscriminator,Asset Category,Currency,Symbol,Date/Time,Quantity,T. Price,Comm/Fee\n\
Trades,Data,Order,Stocks,USD,AAPL,\"2026-06-10, 10:30:00\",10,150.25,-1\n\
Trades,Data,Trade,Stocks,USD,AAPL,\"2026-06-10, 10:30:01\",6,150.25,-0.6\n\
Trades,Data,Trade,Stocks,USD,AAPL,\"2026-06-10, 10:30:02\",4,150.25,-0.4\n";
        let parse = parse_preset("ibkr", csv).unwrap();
        assert_eq!(parse.rows.len(), 1);
        assert_eq!(parse.rows[0].1.qty.unwrap().to_string(), "10");
    }

    #[test]
    fn fidelity_export_maps_and_skips() {
        let csv = "\
Brokerage\n\
\n\
Run Date,Action,Symbol,Description,Type,Quantity,Price ($),Commission ($),Fees ($),Accrued Interest ($),Amount ($),Settlement Date\n\
06/10/2026, YOU BOUGHT             PROSHARES TR (AAPL) (Cash), AAPL, PROSHARES TR, Cash, 10, 150.25, 4.95, 0.05, , -1507.50, 06/12/2026\n\
06/09/2026, DIVIDEND RECEIVED (SCHD) (Cash), SCHD, SCHWAB US DIVIDEND, Cash, , , , , , 25.50, \n\
06/08/2026, Electronic Funds Transfer Received (Cash), , , Cash, , , , , , 5000.00, \n\
06/07/2026, INTEREST EARNED (Cash), , , Cash, , , , , , 0.42, \n\
\n\
\"The data and information in this spreadsheet is provided to you...\"\n";
        let parse = parse_preset("fidelity", csv).unwrap();
        assert_eq!(parse.rows.len(), 3);
        assert_eq!(parse.skipped.len(), 1);
        assert!(parse.skipped[0].contains("INTEREST EARNED"));

        let (buy_line, buy) = &parse.rows[0];
        assert_eq!(*buy_line, 4); // 1-based line in the original file
        assert_eq!(buy.kind, "buy");
        assert_eq!(buy.symbol.as_deref(), Some("AAPL"));
        assert_eq!(buy.qty.unwrap().to_string(), "10");
        assert_eq!(buy.price.unwrap().to_string(), "150.25");
        assert_eq!(buy.fees.unwrap().to_string(), "5.00"); // commission + fees

        let (_, dividend) = &parse.rows[1];
        assert_eq!(dividend.kind, "dividend");
        assert_eq!(dividend.price.unwrap().to_string(), "25.50");

        let (_, deposit) = &parse.rows[2];
        assert_eq!(deposit.kind, "deposit");
        assert_eq!(deposit.price.unwrap().to_string(), "5000.00");
        assert!(deposit.symbol.is_none());
    }

    #[test]
    fn schwab_export_maps_sells_and_transfers() {
        let csv = "\
\"Transactions for account ...XXX as of 06/12/2026\"\n\
\"Date\",\"Action\",\"Symbol\",\"Description\",\"Quantity\",\"Price\",\"Fees & Comm\",\"Amount\"\n\
\"06/11/2026\",\"Sell\",\"VOO\",\"VANGUARD S&P 500\",\"-5\",\"$520.10\",\"$0.03\",\"$2600.47\"\n\
\"06/10/2026\",\"Reinvest Dividend\",\"VOO\",\"VANGUARD S&P 500\",\"\",\"\",\"\",\"$31.20\"\n\
\"06/10/2026\",\"Reinvest Shares\",\"VOO\",\"VANGUARD S&P 500\",\"0.06\",\"$520.00\",\"\",\"-$31.20\"\n\
\"06/09/2026\",\"MoneyLink Transfer\",\"\",\"Tfr FROM CHECKING\",\"\",\"\",\"\",\"$1,000.00\"\n\
\"06/08/2026\",\"Journal\",\"\",\"Journaled Shares\",\"\",\"\",\"\",\"\"\n\
\"Transactions Total\",\"\",\"\",\"\",\"\",\"\",\"\",\"$3,600.47\"\n";
        let parse = parse_preset("schwab", csv).unwrap();
        assert_eq!(parse.rows.len(), 4, "skipped: {:?}", parse.skipped);
        assert_eq!(parse.skipped.len(), 1); // Journal (totals row has no action)

        let (_, sell) = &parse.rows[0];
        assert_eq!(sell.kind, "sell");
        assert_eq!(sell.qty.unwrap().to_string(), "5"); // abs of -5
        assert_eq!(sell.price.unwrap().to_string(), "520.10");

        assert_eq!(parse.rows[1].1.kind, "dividend");
        assert_eq!(parse.rows[2].1.kind, "buy");
        assert_eq!(parse.rows[2].1.qty.unwrap().to_string(), "0.06");

        let (_, transfer) = &parse.rows[3];
        assert_eq!(transfer.kind, "deposit");
        assert_eq!(transfer.price.unwrap().to_string(), "1000.00");
    }

    #[test]
    fn robinhood_maps_trades_dividends_and_routes_ach_by_sign() {
        let csv = "\
\"Activity Date\",\"Process Date\",\"Settle Date\",\"Instrument\",\"Description\",\"Trans Code\",\"Quantity\",\"Price\",\"Amount\"\n\
\"6/10/2026\",\"6/10/2026\",\"6/12/2026\",\"AAPL\",\"Apple\",\"Buy\",\"10\",\"$150.25\",\"($1502.50)\"\n\
\"6/11/2026\",\"6/11/2026\",\"6/13/2026\",\"VOO\",\"Vanguard\",\"Sell\",\"5\",\"$520.10\",\"$2600.47\"\n\
\"6/09/2026\",\"6/09/2026\",\"6/09/2026\",\"SCHD\",\"Schwab Div\",\"CDIV\",\"\",\"\",\"$25.50\"\n\
\"6/08/2026\",\"6/08/2026\",\"6/08/2026\",\"\",\"ACH Deposit\",\"ACH\",\"\",\"\",\"$5000.00\"\n\
\"6/12/2026\",\"6/12/2026\",\"6/12/2026\",\"\",\"ACH Withdrawal\",\"ACH\",\"\",\"\",\"($1000.00)\"\n\
\"6/07/2026\",\"6/07/2026\",\"6/07/2026\",\"AAPL\",\"Call option\",\"BTO\",\"1\",\"$2.50\",\"($250.00)\"\n\
\"6/06/2026\",\"6/06/2026\",\"6/06/2026\",\"\",\"Interest\",\"INT\",\"\",\"\",\"$0.42\"\n";
        let parse = parse_preset("robinhood", csv).unwrap();
        assert_eq!(parse.rows.len(), 5, "skipped: {:?}", parse.skipped);

        let (_, buy) = &parse.rows[0];
        assert_eq!(buy.kind, "buy");
        assert_eq!(buy.symbol.as_deref(), Some("AAPL"));
        assert_eq!(buy.qty.unwrap().to_string(), "10");
        assert_eq!(buy.price.unwrap().to_string(), "150.25");

        assert_eq!(parse.rows[1].1.kind, "sell");
        assert_eq!(parse.rows[1].1.qty.unwrap().to_string(), "5");

        let (_, dividend) = &parse.rows[2];
        assert_eq!(dividend.kind, "dividend");
        assert_eq!(dividend.symbol.as_deref(), Some("SCHD"));
        assert_eq!(dividend.price.unwrap().to_string(), "25.50"); // cash from Amount

        // Same ACH code, routed by the amount's sign.
        assert_eq!(parse.rows[3].1.kind, "deposit");
        assert_eq!(parse.rows[3].1.price.unwrap().to_string(), "5000.00");
        assert_eq!(parse.rows[4].1.kind, "withdrawal");
        assert_eq!(parse.rows[4].1.price.unwrap().to_string(), "1000.00"); // abs

        // The option (BTO) and interest (INT) are reported, never silent.
        assert_eq!(parse.skipped.len(), 2, "{:?}", parse.skipped);
        assert!(parse.skipped.iter().any(|s| s.contains("BTO")));
        assert!(parse.skipped.iter().any(|s| s.contains("INT")));
    }

    #[test]
    fn robinhood_routes_rtp_and_itrf_transfers_by_sign() {
        // RTP (instant bank transfer) and ITRF (account-to-account transfer)
        // are real capital flows, routed by sign like ACH. Income/fee codes
        // (GDBP boost, GOLD fee) stay reported, NOT mapped as capital.
        let csv = "\
\"Activity Date\",\"Process Date\",\"Settle Date\",\"Instrument\",\"Description\",\"Trans Code\",\"Quantity\",\"Price\",\"Amount\"\n\
\"11/12/2024\",\"11/12/2024\",\"11/12/2024\",\"\",\"Instant bank transfer\",\"RTP\",\"\",\"\",\"$15,000.00\"\n\
\"7/17/2025\",\"7/17/2025\",\"7/17/2025\",\"\",\"Transfer from Brokerage to Roth IRA\",\"ITRF\",\"\",\"\",\"($1,000.00)\"\n\
\"6/01/2025\",\"6/01/2025\",\"6/01/2025\",\"\",\"Gold Deposit Boost Payment\",\"GDBP\",\"\",\"\",\"$5.46\"\n\
\"6/16/2025\",\"6/16/2025\",\"6/16/2025\",\"\",\"Gold Subscription Fee\",\"GOLD\",\"\",\"\",\"($5.00)\"\n";
        let parse = parse_preset("robinhood", csv).unwrap();
        assert_eq!(parse.rows.len(), 2, "skipped: {:?}", parse.skipped);

        assert_eq!(parse.rows[0].1.kind, "deposit"); // RTP +
        assert_eq!(parse.rows[0].1.price.unwrap().to_string(), "15000.00");
        assert_eq!(parse.rows[1].1.kind, "withdrawal"); // ITRF −
        assert_eq!(parse.rows[1].1.price.unwrap().to_string(), "1000.00");

        // Promo credit and fee are income/expense — reported, never capital.
        assert_eq!(parse.skipped.len(), 2, "{:?}", parse.skipped);
        assert!(parse.skipped.iter().any(|s| s.contains("GDBP")));
        assert!(parse.skipped.iter().any(|s| s.contains("GOLD")));
    }

    #[test]
    fn skippable_row_with_junk_amount_does_not_fail_the_file() {
        // An unmapped action whose Amount cell is unparseable ("--") must be
        // reported as a skip, not abort the whole import (regression guard:
        // classify runs on a lenient amount, before the strict value parse).
        let csv = "\
\"Activity Date\",\"Process Date\",\"Settle Date\",\"Instrument\",\"Description\",\"Trans Code\",\"Quantity\",\"Price\",\"Amount\"\n\
\"6/06/2026\",\"6/06/2026\",\"6/06/2026\",\"\",\"Gold fee\",\"GOLD\",\"\",\"\",\"--\"\n\
\"6/10/2026\",\"6/10/2026\",\"6/12/2026\",\"AAPL\",\"Apple\",\"Buy\",\"10\",\"$150.25\",\"($1502.50)\"\n";
        let parse = parse_preset("robinhood", csv).unwrap();
        assert_eq!(parse.rows.len(), 1);
        assert_eq!(parse.rows[0].1.kind, "buy");
        assert_eq!(parse.skipped.len(), 1);
        assert!(parse.skipped[0].contains("GOLD"));
    }

    #[test]
    fn zero_value_ach_is_a_deposit_not_a_withdrawal() {
        // `($0.00)` parses to -0.00 (sign-negative) — it must NOT route to
        // withdrawal; only a true debit does.
        let csv = "\
\"Activity Date\",\"Process Date\",\"Settle Date\",\"Instrument\",\"Description\",\"Trans Code\",\"Quantity\",\"Price\",\"Amount\"\n\
\"6/08/2026\",\"6/08/2026\",\"6/08/2026\",\"\",\"Zero transfer\",\"ACH\",\"\",\"\",\"($0.00)\"\n";
        let parse = parse_preset("robinhood", csv).unwrap();
        assert_eq!(parse.rows.len(), 1);
        assert_eq!(parse.rows[0].1.kind, "deposit");
    }

    #[test]
    fn unknown_format_and_wrong_file_are_rejected() {
        assert!(parse_preset("ibkr", "x").is_err());
        let err = parse_preset("fidelity", "just,some,csv\n1,2,3\n").unwrap_err();
        assert!(err.contains("header row"), "{err}");
        // A recognizable header missing a value column must fail loudly,
        // not import zero-value transactions.
        let err = parse_preset(
            "schwab",
            "\"Date\",\"Action\",\"Symbol\",\"Description\",\"Quantity\",\"Price\"\n",
        )
        .unwrap_err();
        assert!(err.contains("missing amount column"), "{err}");
    }
}
