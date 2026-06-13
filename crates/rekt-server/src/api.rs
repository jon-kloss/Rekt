//! REST handlers: transactions CRUD, CSV import, portfolio view.
//!
//! Concurrency: all mutations take `state.mutations` for their whole
//! validate→insert span. Validation replays the entire log, so two
//! interleaved mutations could otherwise both pass against the same
//! snapshot and write a combination the engine would reject (TOCTOU).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use chrono::{DateTime, Utc};
use rekt_core::portfolio::{compute_basis, TxKind};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{import, live, repo, AppState};

pub type ApiError = (StatusCode, Json<serde_json::Value>);

pub fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

pub fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "internal error");
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// One symbol rule for every entry point (watchlist, alerts, …): trimmed,
/// uppercased, alphanumeric-or-dot. A typo'd symbol would otherwise create
/// an instruments row and a stream subscription that never resolves.
pub fn validate_symbol(raw: &str) -> Result<String, ApiError> {
    let symbol = raw.trim().to_uppercase();
    if symbol.is_empty()
        || !symbol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.')
    {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "symbol must be alphanumeric (dots allowed, e.g. BRK.B)",
        ));
    }
    Ok(symbol)
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
///
/// `lines` (parallel to `candidates`) lets CSV imports report the failing
/// candidate by its line in the ORIGINAL file, matching the other two
/// import error paths.
async fn validate_with(
    state: &AppState,
    candidates: &[repo::NewTx],
    lines: Option<&[usize]>,
) -> Result<(), ApiError> {
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
            let idx = (e.tx_id() - CANDIDATE_ID_BASE) as usize;
            let detail = msg.split_once(": ").map(|(_, m)| m).unwrap_or(&msg);
            match lines.and_then(|l| l.get(idx)) {
                Some(line) => format!("line {line}: {detail}"),
                None if candidates.len() == 1 => detail.to_string(),
                None => format!("row {}: {detail}", idx + 1),
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
    tracing::debug!(kind = %input.kind, symbol = ?input.symbol, "POST /api/transactions");
    let new_tx = input
        .into_new_tx(repo::TxSource::Manual)
        .map_err(|m| err(StatusCode::UNPROCESSABLE_ENTITY, m))?;

    let _guard = state.mutations.lock().await;
    validate_with(&state, std::slice::from_ref(&new_tx), None).await?;
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

#[derive(Debug, Deserialize)]
pub struct ImportQuery {
    #[serde(default)]
    pub format: Option<String>,
}

/// CSV import. `?format=generic` (default) takes the native header
/// `kind,symbol,qty,price,fees,taxes,ts,note`; `?format=fidelity|schwab|ibkr`
/// translates those brokers' activity exports (non-transaction rows like
/// interest and journal entries are skipped and reported back).
///
/// All-or-nothing for the mapped rows: validation rejects the whole file
/// on one bad row, and the inserts run in a single SQL transaction so a
/// database failure can't leave a partial import either.
pub async fn import_csv(
    State(state): State<AppState>,
    Query(query): Query<ImportQuery>,
    body: String,
) -> Result<Json<serde_json::Value>, ApiError> {
    let format = query.format.as_deref().unwrap_or("generic");
    tracing::debug!(format, bytes = body.len(), "POST /api/import/csv");
    // Every input carries its 1-based line number in the ORIGINAL file so
    // both parse and validation errors point at the same line the user
    // sees in their editor (header = line 1 for generic; presets track
    // their own offsets past preamble lines).
    let (inputs, skipped) = if format == "generic" {
        let mut reader = csv::ReaderBuilder::new()
            .trim(csv::Trim::All)
            .from_reader(body.as_bytes());
        let mut inputs = Vec::new();
        for (i, row) in reader.deserialize::<TxInput>().enumerate() {
            let line = i + 2;
            let input = row.map_err(|e| {
                err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("line {line}: {e}"),
                )
            })?;
            inputs.push((line, input));
        }
        (inputs, Vec::new())
    } else {
        let parse = import::parse_preset(format, &body)
            .map_err(|m| err(StatusCode::UNPROCESSABLE_ENTITY, m))?;
        (parse.rows, parse.skipped)
    };

    let mut new_txs = Vec::new();
    let mut new_tx_lines = Vec::new();
    for (line, input) in inputs {
        let tx = input.into_new_tx(repo::TxSource::Csv).map_err(|m| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("line {line}: {m}"),
            )
        })?;
        new_txs.push(tx);
        new_tx_lines.push(line);
    }
    if new_txs.is_empty() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            if skipped.is_empty() {
                "no rows in CSV".to_string()
            } else {
                format!("no importable rows — all {} skipped", skipped.len())
            },
        ));
    }

    let _guard = state.mutations.lock().await;
    validate_with(&state, &new_txs, Some(&new_tx_lines)).await?;
    repo::insert_txs(&state.db, &new_txs)
        .await
        .map_err(internal)?;
    state.live.bump_tx_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    tracing::debug!(
        format,
        imported = new_txs.len(),
        skipped = skipped.len(),
        "csv import complete"
    );
    Ok(Json(
        serde_json::json!({ "imported": new_txs.len(), "skipped": skipped }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct WatchlistInput {
    pub symbol: String,
}

/// POST /api/watchlist — track a symbol without holding it.
pub async fn watchlist_add(
    State(state): State<AppState>,
    Json(input): Json<WatchlistInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let symbol = validate_symbol(&input.symbol)?;
    let added = repo::watchlist_add(&state.db, &symbol)
        .await
        .map_err(internal)?;
    state.live.bump_watchlist_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    // Pull candles for a NEW symbol soon (signals); idempotent + resumable,
    // and serialized against the scheduler by the backfill lock.
    if added {
        let bg = state.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::history::backfill_candles(&bg).await {
                tracing::warn!(error = %e, "watchlist candle backfill failed");
            }
        });
    }
    Ok((
        if added {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(serde_json::json!({ "symbol": symbol })),
    ))
}

/// DELETE /api/watchlist/{symbol}
pub async fn watchlist_remove(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
) -> Result<StatusCode, ApiError> {
    let removed = repo::watchlist_remove(&state.db, &symbol)
        .await
        .map_err(internal)?;
    if !removed {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("{} is not on the watchlist", symbol.to_uppercase()),
        ));
    }
    state.live.bump_watchlist_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/watchlist — symbols only; live rows come via the ws snapshot.
pub async fn watchlist_list(State(state): State<AppState>) -> Result<Json<Vec<String>>, ApiError> {
    Ok(Json(
        repo::watchlist_symbols(&state.db).await.map_err(internal)?,
    ))
}

/// One-shot portfolio snapshot (same payload as the websocket pushes).
/// Seeds REST quotes for any unpriced held symbols so the dashboard isn't
/// blank before (or without) the live stream.
pub async fn portfolio(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    // trace (not debug): the dashboard polls this on a short interval.
    tracing::trace!("GET /api/portfolio");
    live::refresh_symbols(&state).await.map_err(internal)?;
    let symbols: Vec<String> = state.symbols.borrow().clone();
    live::seed_missing_quotes(&state, &symbols).await;
    let snapshot = live::portfolio_snapshot(&state).await.map_err(internal)?;
    Ok(Json(snapshot))
}
