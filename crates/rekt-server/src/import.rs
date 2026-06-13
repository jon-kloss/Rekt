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
    match format {
        "fidelity" => parse_broker(body, &FIDELITY),
        "schwab" => parse_broker(body, &SCHWAB),
        "ibkr" => parse_ibkr(body),
        other => Err(format!(
            "unknown CSV format {other:?} (use generic, fidelity, schwab or ibkr)"
        )),
    }
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
    classify: fn(&str) -> Option<&'static str>,
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
    classify: |action| {
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
    classify: |action| {
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
    let first = raw.split([' ', ',']).find(|s| !s.is_empty()).unwrap_or("");
    let date = NaiveDate::parse_from_str(first, "%m/%d/%Y")
        .or_else(|_| NaiveDate::parse_from_str(first, "%Y-%m-%d"))
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
        let Some(kind) = (preset.classify)(action) else {
            skipped.push(format!(
                "line {line}: \"{action}\" (not a portfolio transaction)"
            ));
            continue;
        };
        let ts = parse_broker_date(date_raw).map_err(|m| format!("line {line}: {m}"))?;
        let money = |idx: Option<usize>, what: &str| -> Result<Option<Decimal>, String> {
            parse_money(field(idx)).map_err(|m| format!("line {line}: {what}: {m}"))
        };

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

/// Leading ticker of an IBKR dividend description, e.g.
/// "AAPL(US0378331005) Cash Dividend USD 0.24 per Share" → "AAPL".
fn ticker_prefix(desc: &str) -> Option<String> {
    let ticker: String = desc
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect();
    (!ticker.is_empty()).then_some(ticker)
}

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
/// Dividends, Deposits & Withdrawals — into the generic shape. Everything
/// else (Open Positions, Net Asset Value, Account Information, …) is
/// structural, not a transaction, and ignored. USD and stocks/ETFs only;
/// options, forex, and non-USD rows are skipped and reported. Withholding
/// tax is reported too (dividends import GROSS).
fn parse_ibkr(body: &str) -> Result<PresetParse, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(body.as_bytes());

    // Collect rows with their original file line (quoted Date/Time fields
    // embed commas, so the csv reader — not a naive line split — owns
    // tokenizing). Drop the leading BOM on the very first section name.
    let mut records: Vec<(u64, Vec<String>)> = Vec::new();
    for result in reader.records() {
        let record = result.map_err(|e| format!("bad IBKR row: {e}"))?;
        let line = record.position().map(|p| p.line()).unwrap_or(0);
        let fields: Vec<String> = record
            .iter()
            .map(|s| s.trim().trim_start_matches('\u{feff}').to_string())
            .collect();
        if fields.len() >= 2 {
            records.push((line, fields));
        }
    }

    // Each section's Header row names its columns (offset by the leading
    // section + row-type fields, so header column c is data field c + 2).
    let mut headers: HashMap<String, Vec<String>> = HashMap::new();
    for (_, f) in &records {
        if f[1].eq_ignore_ascii_case("header") {
            headers.insert(f[0].clone(), f[2..].to_vec());
        }
    }
    if !["Trades", "Dividends", "Deposits & Withdrawals"]
        .iter()
        .any(|s| headers.contains_key(*s))
    {
        return Err(
            "no IBKR Trades/Dividends/Deposits & Withdrawals sections found \
             — is this an Interactive Brokers Activity Statement?"
                .to_string(),
        );
    }

    let value = |section: &str, f: &[String], name: &str| -> Option<String> {
        let cols = headers.get(section)?;
        let idx = cols.iter().position(|c| c.eq_ignore_ascii_case(name))?;
        f.get(idx + 2).map(|s| s.trim().to_string())
    };

    // Trades come as order-level rows AND per-execution / per-lot detail
    // rows; importing more than one discriminator double-counts. Prefer
    // "Order" rows; fall back to "Trade" when a statement has no order
    // grouping. "ClosedLot" detail is never a separate transaction.
    let has_order = records.iter().any(|(_, f)| {
        f[0] == "Trades"
            && f[1] == "Data"
            && value("Trades", f, "DataDiscriminator").as_deref() == Some("Order")
    });
    let preferred_disc = if has_order { "Order" } else { "Trade" };

    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    for (line, f) in &records {
        if f[1] != "Data" {
            continue; // Header / SubTotal / Total / Notes
        }
        let money = |name: &str,
                     what: &str,
                     section: &str,
                     f: &[String]|
         -> Result<Option<Decimal>, String> {
            parse_money(value(section, f, name).as_deref().unwrap_or(""))
                .map_err(|m| format!("line {line}: {what}: {m}"))
        };
        match f[0].as_str() {
            "Trades" => {
                let disc = value("Trades", f, "DataDiscriminator").unwrap_or_default();
                if !disc.eq_ignore_ascii_case(preferred_disc) {
                    continue; // duplicate execution / lot detail
                }
                let symbol = value("Trades", f, "Symbol").unwrap_or_default();
                let asset = value("Trades", f, "Asset Category").unwrap_or_default();
                if !asset.eq_ignore_ascii_case("Stocks") {
                    skipped.push(format!(
                        "line {line}: {symbol} ({asset}) — only stocks & ETFs import"
                    ));
                    continue;
                }
                let currency = value("Trades", f, "Currency").unwrap_or_default();
                if !currency.eq_ignore_ascii_case("USD") {
                    skipped.push(format!(
                        "line {line}: {symbol} priced in {currency} — only USD imports"
                    ));
                    continue;
                }
                let Some(qty) = money("Quantity", "quantity", "Trades", f)? else {
                    continue; // zero/blank quantity is not a trade
                };
                let date = value("Trades", f, "Date/Time").unwrap_or_default();
                let ts = parse_broker_date(&date).map_err(|m| format!("line {line}: {m}"))?;
                let fees = money("Comm/Fee", "commission", "Trades", f)?
                    .unwrap_or_default()
                    .abs();
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
                        price: money("T. Price", "price", "Trades", f)?,
                        fees: Some(fees),
                        taxes: None,
                        ts: Some(ts),
                        note: Some("ibkr: trade".to_string()),
                    },
                ));
            }
            "Dividends" => {
                let currency = value("Dividends", f, "Currency").unwrap_or_default();
                if is_total_row(&currency) {
                    continue;
                }
                if !currency.eq_ignore_ascii_case("USD") {
                    skipped.push(format!(
                        "line {line}: dividend in {currency} — only USD imports"
                    ));
                    continue;
                }
                let desc = value("Dividends", f, "Description").unwrap_or_default();
                let Some(amount) = money("Amount", "amount", "Dividends", f)? else {
                    continue;
                };
                let Some(symbol) = ticker_prefix(&desc) else {
                    skipped.push(format!("line {line}: dividend with no ticker in {desc:?}"));
                    continue;
                };
                let date = value("Dividends", f, "Date").unwrap_or_default();
                let ts = parse_broker_date(&date).map_err(|m| format!("line {line}: {m}"))?;
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
                        note: Some("ibkr: dividend".to_string()),
                    },
                ));
            }
            "Deposits & Withdrawals" => {
                let currency = value("Deposits & Withdrawals", f, "Currency").unwrap_or_default();
                if is_total_row(&currency) {
                    continue;
                }
                if !currency.eq_ignore_ascii_case("USD") {
                    skipped.push(format!(
                        "line {line}: cash movement in {currency} — only USD imports"
                    ));
                    continue;
                }
                let Some(amount) = money("Amount", "amount", "Deposits & Withdrawals", f)? else {
                    continue;
                };
                let date = value("Deposits & Withdrawals", f, "Settle Date").unwrap_or_default();
                let ts = parse_broker_date(&date).map_err(|m| format!("line {line}: {m}"))?;
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
                        note: Some("ibkr: cash".to_string()),
                    },
                ));
            }
            "Withholding Tax" => {
                // Dividends import gross; withholding is cash the user can
                // reconcile separately. Reported, never silently dropped.
                let currency = value("Withholding Tax", f, "Currency").unwrap_or_default();
                if is_total_row(&currency) {
                    continue;
                }
                let desc = value("Withholding Tax", f, "Description").unwrap_or_default();
                skipped.push(format!(
                    "line {line}: withholding tax {desc:?} (dividends import gross)"
                ));
            }
            _ => {} // structural section — not a transaction
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
