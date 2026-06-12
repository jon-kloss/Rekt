//! REST handlers: transactions CRUD, CSV import, portfolio view.
//!
//! Concurrency: all mutations take `state.mutations` for their whole
//! validate→insert span. Validation replays the entire log, so two
//! interleaved mutations could otherwise both pass against the same
//! snapshot and write a combination the engine would reject (TOCTOU).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use chrono::{DateTime, Utc};
use rekt_core::portfolio::{compute_basis, TxKind};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{live, repo, AppState};

pub type ApiError = (StatusCode, Json<serde_json::Value>);

pub fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

pub fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "internal error");
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Shared input shape for both the JSON API and CSV rows (the csv crate
/// maps empty fields to `None` for `Option` types).
#[derive(Debug, Deserialize)]
pub struct TxInput {
    pub kind: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub qty: Option<Decimal>,
    #[serde(default)]
    pub price: Option<Decimal>,
    #[serde(default)]
    pub fees: Option<Decimal>,
    #[serde(default)]
    pub taxes: Option<Decimal>,
    #[serde(default)]
    pub ts: Option<DateTime<Utc>>,
    #[serde(default)]
    pub note: Option<String>,
}

impl TxInput {
    fn into_new_tx(self, source: repo::TxSource) -> Result<repo::NewTx, String> {
        let kind: TxKind = self.kind.parse()?;
        let symbol = self
            .symbol
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty());
        if kind.needs_symbol() && symbol.is_none() {
            return Err(format!("{} requires a symbol", kind.as_str()));
        }
        Ok(repo::NewTx {
            kind,
            symbol,
            qty: self.qty.unwrap_or_default(),
            price: self.price.unwrap_or_default(),
            fees: self.fees.unwrap_or_default(),
            taxes: self.taxes.unwrap_or_default(),
            ts: self.ts.unwrap_or_else(Utc::now),
            note: self.note.unwrap_or_default(),
            source,
            fill_id: None,
            mode: "live", // manual/CSV entries are real holdings
        })
    }
}

/// Placeholder id base for not-yet-inserted transactions during validation.
/// Far above any real rowid, and ascending so same-timestamp candidates
/// keep their input order in the replay (ties break by id).
const CANDIDATE_ID_BASE: i64 = i64::MAX / 2;

/// Validate that the live-mode log including `candidates` still replays
/// cleanly (oversells etc.), without writing anything.
///
/// Repair-friendly: if the EXISTING log is already inconsistent (e.g. a
/// broker fill landed against deleted history), new transactions are
/// allowed through — otherwise the user could never insert the correcting
/// entry and the create path would be permanently bricked.
async fn validate_with(state: &AppState, candidates: &[repo::NewTx]) -> Result<(), ApiError> {
    let mut txs = repo::fetch_mode_txs(&state.db, "live")
        .await
        .map_err(internal)?;
    if let Err(existing_error) = compute_basis(&txs) {
        tracing::warn!(
            error = %existing_error,
            "transaction log already inconsistent — allowing mutation so it can be repaired"
        );
        return Ok(());
    }
    for (i, c) in candidates.iter().enumerate() {
        txs.push(rekt_core::portfolio::Tx {
            id: CANDIDATE_ID_BASE + i as i64,
            kind: c.kind,
            symbol: c.symbol.clone(),
            qty: c.qty,
            price: c.price,
            fees: c.fees,
            taxes: c.taxes,
            ts: c.ts,
            note: c.note.clone(),
        });
    }
    txs.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.id.cmp(&b.id)));
    compute_basis(&txs).map(|_| ()).map_err(|e| {
        // "transaction 4611686018427387904: …" helps nobody — name the
        // candidate by its input position instead.
        let msg = e.to_string();
        let msg = if e.tx_id() >= CANDIDATE_ID_BASE {
            let detail = msg.split_once(": ").map(|(_, m)| m).unwrap_or(&msg);
            if candidates.len() == 1 {
                detail.to_string()
            } else {
                let row = e.tx_id() - CANDIDATE_ID_BASE + 1;
                format!("row {row}: {detail}")
            }
        } else {
            msg
        };
        err(StatusCode::UNPROCESSABLE_ENTITY, msg)
    })
}

pub async fn create_tx(
    State(state): State<AppState>,
    Json(input): Json<TxInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let new_tx = input
        .into_new_tx(repo::TxSource::Manual)
        .map_err(|m| err(StatusCode::UNPROCESSABLE_ENTITY, m))?;

    let _guard = state.mutations.lock().await;
    validate_with(&state, std::slice::from_ref(&new_tx)).await?;
    let ids = repo::insert_txs(&state.db, std::slice::from_ref(&new_tx))
        .await
        .map_err(internal)?;
    state.live.bump_tx_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": ids[0] })),
    ))
}

pub async fn list_txs(
    State(state): State<AppState>,
) -> Result<Json<Vec<repo::TxRecord>>, ApiError> {
    let mut txs = repo::fetch_txs(&state.db, None).await.map_err(internal)?;
    txs.reverse(); // newest first for display
    Ok(Json(txs))
}

pub async fn delete_tx(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let _guard = state.mutations.lock().await;
    let deleted = repo::delete_tx(&state.db, id).await.map_err(internal)?;
    if !deleted {
        return Err(err(StatusCode::NOT_FOUND, format!("no transaction {id}")));
    }
    state.live.bump_tx_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Generic CSV import. Header: `kind,symbol,qty,price,fees,taxes,ts,note`
/// (qty/price/fees/taxes/symbol optional per kind; ts RFC 3339, defaults to
/// now). All-or-nothing: validation rejects the whole file on one bad row,
/// and the inserts run in a single SQL transaction so a database failure
/// can't leave a partial import either.
pub async fn import_csv(
    State(state): State<AppState>,
    body: String,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(body.as_bytes());
    let mut new_txs = Vec::new();
    for (line, row) in reader.deserialize::<TxInput>().enumerate() {
        let input = row.map_err(|e| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("row {}: {e}", line + 2),
            )
        })?;
        let tx = input.into_new_tx(repo::TxSource::Csv).map_err(|m| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("row {}: {m}", line + 2),
            )
        })?;
        new_txs.push(tx);
    }
    if new_txs.is_empty() {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "no rows in CSV"));
    }

    let _guard = state.mutations.lock().await;
    validate_with(&state, &new_txs).await?;
    repo::insert_txs(&state.db, &new_txs)
        .await
        .map_err(internal)?;
    state.live.bump_tx_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    Ok(Json(serde_json::json!({ "imported": new_txs.len() })))
}

/// One-shot portfolio snapshot (same payload as the websocket pushes).
/// Seeds REST quotes for any unpriced held symbols so the dashboard isn't
/// blank before (or without) the live stream.
pub async fn portfolio(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    live::refresh_symbols(&state).await.map_err(internal)?;
    let symbols: Vec<String> = state.symbols.borrow().clone();
    live::seed_missing_quotes(&state, &symbols).await;
    let snapshot = live::portfolio_snapshot(&state).await.map_err(internal)?;
    Ok(Json(snapshot))
}
