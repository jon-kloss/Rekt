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
use chrono_tz::America::New_York;
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
    let current = Utc::now().with_timezone(&New_York).year();
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
    let (report, years) = build(&state, query.year).await?;
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
fn to_8949_csv(report: &TaxReport) -> String {
    let mut out = String::from(
        "Description,Date Acquired,Date Sold,Proceeds,Cost Basis,Code,Adjustment,Gain/Loss,Term\n",
    );
    for row in &report.rows {
        // Symbols are validated alphanumeric-or-dot and qty is a Decimal,
        // so no field needs CSV quoting.
        out.push_str(&format!(
            "{} sh {},{},{},{},{},{},{},{},{}\n",
            row.qty,
            row.symbol,
            row.acquired.format("%m/%d/%Y"),
            row.sold.format("%m/%d/%Y"),
            row.proceeds.round_dp(2),
            row.basis.round_dp(2),
            row.code,
            row.disallowed.round_dp(2),
            (row.gain + row.disallowed).round_dp(2),
            if row.long_term { "long" } else { "short" },
        ));
    }
    out
}
