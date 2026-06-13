//! Phase 6: Form 8949 / Schedule D endpoint + CSV export.
//!
//! Live-mode transactions only — paper fills are not taxable events and
//! must never leak into a tax document.

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use chrono::{Datelike, Utc};
use rekt_core::taxes::{ny_date, tax_report, TaxReport};

use crate::api::{err, internal, ApiError};
use crate::{repo, AppState};

#[derive(Debug, serde::Deserialize)]
pub struct TaxQuery {
    #[serde(default)]
    pub year: Option<i32>,
}

/// Compute the report for the requested (default: current NY) year, plus
/// the list of years that have any live transactions.
async fn build(state: &AppState, year: Option<i32>) -> Result<(TaxReport, Vec<i32>), ApiError> {
    let txs = repo::fetch_mode_txs(&state.db, "live")
        .await
        .map_err(internal)?;
    let current = ny_date(Utc::now()).year();
    let years: Vec<i32> = match txs.first().map(|t| ny_date(t.ts).year()) {
        Some(first) => (first..=current.max(first)).rev().collect(),
        None => vec![current],
    };
    let year = year.unwrap_or(current);
    let report = tax_report(&txs, year).map_err(|e| {
        err(
            StatusCode::CONFLICT,
            format!("transaction log inconsistent: {e}"),
        )
    })?;
    Ok((report, years))
}

/// GET /api/taxes?year=YYYY — Form 8949 rows + Schedule D totals.
pub async fn taxes(
    State(state): State<AppState>,
    Query(query): Query<TaxQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    tracing::debug!(year = ?query.year, "GET /api/taxes");
    let (report, years) = build(&state, query.year).await?;
    tracing::debug!(
        year = report.year,
        rows = report.rows.len(),
        "tax report built"
    );
    let mut body = serde_json::to_value(&report).map_err(internal)?;
    body.as_object_mut()
        .expect("TaxReport serializes to an object")
        .insert("years".into(), serde_json::json!(years));
    Ok(Json(body))
}

/// GET /api/taxes/csv?year=YYYY — Form 8949-shaped CSV download.
pub async fn taxes_csv(
    State(state): State<AppState>,
    Query(query): Query<TaxQuery>,
) -> Result<Response, ApiError> {
    tracing::debug!(year = ?query.year, "GET /api/taxes/csv");
    let (report, _) = build(&state, query.year).await?;
    let csv = to_8949_csv(&report);
    let filename = format!("rekt-form8949-{}.csv", report.year);
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        csv,
    )
        .into_response())
}

/// Form 8949 column layout: description (a), acquired (b), sold (c),
/// proceeds (d), basis (e), code (f), adjustment (g), gain/loss (h, with
/// the wash adjustment applied), plus term for Part I/II sorting.
///
/// Written with the `csv` crate so quoting is always correct — symbols are
/// validated at most entry points, but the CSV import path stores them
/// verbatim, so a comma in a field must not shift the columns.
fn to_8949_csv(report: &TaxReport) -> String {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record([
        "Description",
        "Date Acquired",
        "Date Sold",
        "Proceeds",
        "Cost Basis",
        "Code",
        "Adjustment",
        "Gain/Loss",
        "Term",
    ])
    .expect("in-memory csv write");
    for row in &report.rows {
        let proceeds = row.proceeds.round_dp(2);
        let basis = row.basis.round_dp(2);
        let adjustment = row.disallowed.round_dp(2);
        // Column (h) from the already-rounded columns, so every exported
        // row passes the d − e + g = h arithmetic check preparers apply.
        let gain = proceeds - basis + adjustment;
        w.write_record([
            format!("{} sh {}", row.qty, row.symbol),
            row.acquired.format("%m/%d/%Y").to_string(),
            row.sold.format("%m/%d/%Y").to_string(),
            proceeds.to_string(),
            basis.to_string(),
            row.code.to_string(),
            adjustment.to_string(),
            gain.to_string(),
            if row.long_term { "long" } else { "short" }.to_string(),
        ])
        .expect("in-memory csv write");
    }
    String::from_utf8(w.into_inner().expect("in-memory csv flush")).expect("csv output is utf-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rekt_core::taxes::{Form8949Row, TermTotals};

    #[test]
    fn hostile_fields_are_quoted_and_rows_stay_arithmetic_consistent() {
        // The CSV import path stores symbols verbatim, so the export must
        // quote a comma rather than shift every column right.
        let report = TaxReport {
            year: 2026,
            short: TermTotals::default(),
            long: TermTotals::default(),
            rows: vec![Form8949Row {
                symbol: "FOO,BAR".into(),
                qty: "1".parse().unwrap(),
                acquired: chrono::NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(),
                sold: chrono::NaiveDate::from_ymd_opt(2026, 3, 2).unwrap(),
                // Unrounded internals: rounded d=100.01, e=50.00 → the
                // exported h must be 50.01 (from the rounded columns),
                // not round(100.006 − 50.004) = 50.00.
                proceeds: "100.006".parse().unwrap(),
                basis: "50.004".parse().unwrap(),
                gain: "50.002".parse().unwrap(),
                disallowed: rust_decimal::Decimal::ZERO,
                code: "",
                long_term: false,
            }],
        };
        let csv = to_8949_csv(&report);
        assert!(csv.contains("\"1 sh FOO,BAR\""), "{csv}");
        assert!(csv.contains("100.01,50.00,,0,50.01,short"), "{csv}");
    }
}
