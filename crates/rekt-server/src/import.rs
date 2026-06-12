//! Broker CSV presets (PLAN.md Phase 4): translate Fidelity and Schwab
//! activity exports into the generic transaction shape, for track-only
//! accounts held outside Alpaca.
//!
//! Philosophy: mapped rows are all-or-nothing validated like the generic
//! import; rows a broker export legitimately contains but that aren't
//! portfolio transactions (interest, journal entries, disclaimers) are
//! SKIPPED and reported back — never silently dropped without a trace.

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;

use crate::api::TxInput;

/// A preset parse: importable rows + human-readable skip reasons.
#[derive(Debug)]
pub struct PresetParse {
    pub rows: Vec<TxInput>,
    pub skipped: Vec<String>,
}

pub fn parse_preset(format: &str, body: &str) -> Result<PresetParse, String> {
    match format {
        "fidelity" => parse_broker(body, &FIDELITY),
        "schwab" => parse_broker(body, &SCHWAB),
        other => Err(format!(
            "unknown CSV format {other:?} (use generic, fidelity or schwab)"
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
    Ok(Some(if negative { -value } else { value }))
}

/// "06/12/2026" (optionally "… as of 06/15/2026") → 4pm New York, as UTC.
fn parse_broker_date(raw: &str) -> Result<DateTime<Utc>, String> {
    let first = raw.split_whitespace().next().unwrap_or("");
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
    let col_date = find(preset.date).ok_or("missing date column")?;
    let col_action = find(preset.action).ok_or("missing action column")?;
    let col_symbol = find(preset.symbol);
    let col_qty = find(preset.qty);
    let col_price = find(preset.price);
    let col_fees = find(preset.fees);
    let col_commission = find(preset.commission);
    let col_amount = find(preset.amount);

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

        rows.push(TxInput {
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
        });
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
        assert_eq!(parse_money("").unwrap(), None);
        assert!(parse_money("abc").is_err());
        // 4pm ET June (EDT, UTC-4) → 20:00Z.
        let ts = parse_broker_date("06/12/2026").unwrap();
        assert_eq!(ts.to_rfc3339(), "2026-06-12T20:00:00+00:00");
        assert!(parse_broker_date("12 Jun 2026").is_err());
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

        let buy = &parse.rows[0];
        assert_eq!(buy.kind, "buy");
        assert_eq!(buy.symbol.as_deref(), Some("AAPL"));
        assert_eq!(buy.qty.unwrap().to_string(), "10");
        assert_eq!(buy.price.unwrap().to_string(), "150.25");
        assert_eq!(buy.fees.unwrap().to_string(), "5.00"); // commission + fees

        let dividend = &parse.rows[1];
        assert_eq!(dividend.kind, "dividend");
        assert_eq!(dividend.price.unwrap().to_string(), "25.50");

        let deposit = &parse.rows[2];
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

        let sell = &parse.rows[0];
        assert_eq!(sell.kind, "sell");
        assert_eq!(sell.qty.unwrap().to_string(), "5"); // abs of -5
        assert_eq!(sell.price.unwrap().to_string(), "520.10");

        assert_eq!(parse.rows[1].kind, "dividend");
        assert_eq!(parse.rows[2].kind, "buy");
        assert_eq!(parse.rows[2].qty.unwrap().to_string(), "0.06");

        let transfer = &parse.rows[3];
        assert_eq!(transfer.kind, "deposit");
        assert_eq!(transfer.price.unwrap().to_string(), "1000.00");
    }

    #[test]
    fn unknown_format_and_wrong_file_are_rejected() {
        assert!(parse_preset("ibkr", "x").is_err());
        let err = parse_preset("fidelity", "just,some,csv\n1,2,3\n").unwrap_err();
        assert!(err.contains("header row"), "{err}");
    }
}
