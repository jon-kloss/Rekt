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

/// Reconcile a CSV import against the existing live log: replay everything and,
/// when a CANDIDATE sell can't be backed by held shares at its point in time,
/// DROP that row and report it rather than aborting the whole import. Real
/// brokerage exports routinely contain sells of positions opened before the
/// export window, or whose share counts were changed by splits/mergers/
/// spin-offs this tracker doesn't model — those sells are un-backable through
/// no fault of the user. This matches the importer's standing policy: skip and
/// report, never silently drop, and never fabricate an opening lot.
///
/// Genuine bad rows (missing symbol, non-positive qty) still hard-fail with
/// their line, exactly as before. Returns the candidate indices to KEEP plus
/// human-readable drop reasons to fold into the skip report.
async fn reconcile_import(
    state: &AppState,
    candidates: &[repo::NewTx],
    lines: &[usize],
) -> Result<(Vec<usize>, Vec<String>), ApiError> {
    use rekt_core::portfolio::{PortfolioError, Tx};
    let existing = repo::fetch_mode_txs(&state.db, "live")
        .await
        .map_err(internal)?;
    // If the existing log is already inconsistent, fall back to the
    // repair-friendly rule (keep everything) — same as validate_with.
    if compute_basis(&existing).is_err() {
        return Ok(((0..candidates.len()).collect(), Vec::new()));
    }

    let mut kept: Vec<usize> = (0..candidates.len()).collect();
    let mut dropped = Vec::new();
    loop {
        let mut txs = existing.clone();
        for &i in &kept {
            let c = &candidates[i];
            txs.push(Tx {
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
        let Err(e) = compute_basis(&txs) else {
            break;
        };
        let id = e.tx_id();
        // An EXISTING tx broke (not a candidate) — can't repair by dropping
        // imports; let validate_with's repair-friendly path handle it.
        if id < CANDIDATE_ID_BASE {
            break;
        }
        let idx = (id - CANDIDATE_ID_BASE) as usize;
        let line = lines.get(idx).copied().unwrap_or(0);
        match &e {
            PortfolioError::Oversell {
                symbol, have, want, ..
            } => {
                dropped.push(format!(
                    "line {line}: sell {want} {symbol} skipped — only {have} held in this import. \
                     {symbol} may still show shares here that you actually sold, and this sale's \
                     proceeds and realized P&L are NOT recorded. Cause: the opening lot predates \
                     this export, or a split/merger this importer doesn't model changed the share \
                     count. Fix by adding an opening lot (or adjusting {symbol}) manually."
                ));
                kept.retain(|&k| k != idx);
            }
            // A genuinely malformed row — surface it loudly, as before.
            other => {
                let msg = other.to_string();
                let detail = msg.split_once(": ").map(|(_, m)| m).unwrap_or(&msg);
                return Err(err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("line {line}: {detail}"),
                ));
            }
        }
    }
    Ok((kept, dropped))
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
    crate::splits::invalidate();
    live::refresh_symbols(&state).await.map_err(internal)?;
    // A newly-held symbol needs candle history for its chart + the equity
    // curve / signals; pull it now instead of waiting for the 30-min tick.
    if new_tx.symbol.is_some() {
        crate::history::spawn_backfill(&state);
    }
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
    crate::splits::invalidate();
    live::refresh_symbols(&state).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct ImportQuery {
    #[serde(default)]
    pub format: Option<String>,
    /// When true, parse + validate the full ledger replay but insert nothing,
    /// returning what WOULD be imported. Powers the UI's preview-then-confirm.
    #[serde(default)]
    pub dry_run: Option<bool>,
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
    crate::demo_guard(&state)?;
    let format = query.format.as_deref().unwrap_or("generic");
    tracing::debug!(format, bytes = body.len(), "POST /api/import/csv");
    // Every input carries its 1-based line number in the ORIGINAL file so
    // both parse and validation errors point at the same line the user
    // sees in their editor (header = line 1 for generic; presets track
    // their own offsets past preamble lines).
    let (inputs, mut skipped) = if format == "generic" {
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
    // Bound the reconcile replay (O(n²) worst case on all-oversell input).
    const MAX_IMPORT_ROWS: usize = 10_000;
    if new_txs.len() > MAX_IMPORT_ROWS {
        return Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "import has {} mapped rows; the limit is {MAX_IMPORT_ROWS} — split the file",
                new_txs.len()
            ),
        ));
    }

    // Replay order is (ts, id) everywhere and ids are assigned at INSERT time,
    // so insert order decides same-timestamp tie-breaks. Brokerage exports are
    // newest-first and don't guarantee buy-before-sell within a day (Robinhood
    // normalizes every row to 4pm NY, collapsing a same-day round-trip to one
    // timestamp). Sort so that for any timestamp, acquisitions/credits land
    // before disposals — otherwise a same-day sell could spuriously oversell a
    // same-day buy and be dropped. Stable on the original file order otherwise.
    {
        use rekt_core::portfolio::TxKind;
        let rank = |k: TxKind| matches!(k, TxKind::Sell | TxKind::Withdrawal) as u8;
        let mut order: Vec<usize> = (0..new_txs.len()).collect();
        order.sort_by(|&a, &b| {
            new_txs[a]
                .ts
                .cmp(&new_txs[b].ts)
                .then(rank(new_txs[a].kind).cmp(&rank(new_txs[b].kind)))
                .then(a.cmp(&b))
        });
        new_txs = order.iter().map(|&i| new_txs[i].clone()).collect();
        new_tx_lines = order.iter().map(|&i| new_tx_lines[i]).collect();
    }

    let _guard = state.mutations.lock().await;
    // Replay-validate; drop (and report) candidate sells that can't be backed
    // by held shares — pre-window or corporate-action-orphaned positions — so a
    // real brokerage history imports instead of aborting on the first one.
    let (keep, dropped) = reconcile_import(&state, &new_txs, &new_tx_lines).await?;
    if keep.len() != new_txs.len() {
        new_txs = keep.iter().map(|&i| new_txs[i].clone()).collect();
        // NOTE: new_tx_lines is intentionally NOT re-filtered here — it has
        // already served reconcile_import and nothing past this point reads it.
        // If a future validation step needs line numbers, filter it in parallel.
    }
    skipped.extend(dropped);
    if new_txs.is_empty() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no importable rows remain after reconciliation — every mapped trade was an \
             un-backable sell (positions opened before this export's window)"
                .to_string(),
        ));
    }

    // Corporate actions (splits/mergers/spin-offs) are skipped, but unlike an
    // un-backable sell they can leave a position's share count silently WRONG
    // without ever tripping an oversell (e.g. a reverse split that isn't
    // applied). Surface a reconcile-risk caveat so the user knows to verify.
    const CORP_ACTION_CODES: &[&str] = &["SPR", "MRGS", "SOFF", "SDIV", "BCXL", "SPL"];
    let corp_actions = skipped
        .iter()
        .filter(|s| {
            CORP_ACTION_CODES
                .iter()
                .any(|c| s.contains(&format!("\"{c}\"")))
        })
        .count();
    if corp_actions > 0 {
        skipped.push(format!(
            "NOTE: {corp_actions} corporate-action row(s) (splits / mergers / spin-offs) were \
             skipped — affected positions' imported share counts may not match your broker. \
             Verify those positions and adjust the quantity manually if needed."
        ));
    }

    // Preview: everything is parsed and the full ledger replay validated, but
    // nothing is written. Return a sample so the UI can show what will land.
    if query.dry_run.unwrap_or(false) {
        let sample: Vec<serde_json::Value> = new_txs
            .iter()
            .take(20)
            .map(|tx| {
                serde_json::json!({
                    "kind": tx.kind.as_str(),
                    "symbol": tx.symbol,
                    "qty": tx.qty,
                    "price": tx.price,
                    "ts": tx.ts,
                })
            })
            .collect();
        return Ok(Json(serde_json::json!({
            "dry_run": true,
            "would_import": new_txs.len(),
            "skipped": skipped,
            "sample": sample,
        })));
    }

    repo::insert_txs(&state.db, &new_txs)
        .await
        .map_err(internal)?;
    state.live.bump_tx_revision();
    crate::splits::invalidate();
    live::refresh_symbols(&state).await.map_err(internal)?;
    // Pull candle history for any newly-imported symbols now (chart, equity
    // curve, signals) rather than waiting for the next scheduler tick.
    if new_txs.iter().any(|t| t.symbol.is_some()) {
        crate::history::spawn_backfill(&state);
    }
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
        crate::history::spawn_backfill(&state);
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

// ---- named watchlists (themed universes; each screened separately) --------

#[derive(Debug, Deserialize)]
pub struct ListNameInput {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct BulkSymbolsInput {
    /// Free-form: comma / whitespace / newline separated, so a user can paste
    /// a big list at once.
    pub symbols: String,
}

/// GET /api/watchlists — named lists with member counts.
pub async fn watchlists_list(
    State(state): State<AppState>,
) -> Result<Json<Vec<repo::WatchlistRow>>, ApiError> {
    Ok(Json(
        repo::list_watchlists(&state.db).await.map_err(internal)?,
    ))
}

/// POST /api/watchlists {name} — create a named list (409 if the name exists).
pub async fn watchlist_create(
    State(state): State<AppState>,
    Json(input): Json<ListNameInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let name = input.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "name must be 1–40 characters",
        ));
    }
    // Serialize list mutations (FK enforcement is off; the lock prevents a
    // concurrent delete from racing a check-then-act and orphaning rows).
    let _guard = state.mutations.lock().await;
    let lists = repo::list_watchlists(&state.db).await.map_err(internal)?;
    if lists.iter().any(|l| l.name.eq_ignore_ascii_case(&name)) {
        return Err(err(
            StatusCode::CONFLICT,
            format!("a list named {name:?} already exists"),
        ));
    }
    let id = repo::create_watchlist(&state.db, &name)
        .await
        .map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "name": name })),
    ))
}

/// DELETE /api/watchlists/{id} — remove a list (refuses the last one).
pub async fn watchlist_delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let _guard = state.mutations.lock().await;
    let lists = repo::list_watchlists(&state.db).await.map_err(internal)?;
    if lists.len() <= 1 {
        return Err(err(
            StatusCode::CONFLICT,
            "can't delete the only watchlist — create another first",
        ));
    }
    let deleted = repo::delete_watchlist(&state.db, id)
        .await
        .map_err(internal)?;
    if !deleted {
        return Err(err(StatusCode::NOT_FOUND, format!("no watchlist {id}")));
    }
    state.live.bump_watchlist_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/watchlists/{id} — the list's member symbols.
pub async fn watchlist_member_list(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<String>>, ApiError> {
    Ok(Json(
        repo::watchlist_members(&state.db, id)
            .await
            .map_err(internal)?,
    ))
}

/// POST /api/watchlists/{id}/symbols {symbols} — bulk-add (parses a pasted
/// list); reports how many landed, plus any tokens that weren't valid symbols.
pub async fn watchlist_add_symbols(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<BulkSymbolsInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut syms: Vec<String> = Vec::new();
    let mut invalid: Vec<String> = Vec::new();
    for tok in input.symbols.split(|c: char| c == ',' || c.is_whitespace()) {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        match validate_symbol(t) {
            Ok(s) if !syms.contains(&s) => syms.push(s),
            Ok(_) => {}
            Err(_) => invalid.push(t.to_uppercase()),
        }
    }
    if syms.is_empty() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no valid symbols in the input",
        ));
    }
    let _guard = state.mutations.lock().await;
    let lists = repo::list_watchlists(&state.db).await.map_err(internal)?;
    if !lists.iter().any(|l| l.id == id) {
        return Err(err(StatusCode::NOT_FOUND, format!("no watchlist {id}")));
    }
    let added = repo::watchlist_add_bulk(&state.db, id, &syms)
        .await
        .map_err(internal)?;
    state.live.bump_watchlist_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    // Pull candles for the newly-watched names (signals/screening need them).
    crate::history::spawn_backfill(&state);
    Ok(Json(serde_json::json!({
        "added": added,
        "submitted": syms.len(),
        "invalid": invalid,
    })))
}

/// DELETE /api/watchlists/{id}/symbols/{symbol} — remove one member.
pub async fn watchlist_remove_symbol(
    State(state): State<AppState>,
    Path((id, symbol)): Path<(i64, String)>,
) -> Result<StatusCode, ApiError> {
    let _guard = state.mutations.lock().await;
    let removed = repo::watchlist_member_remove(&state.db, id, &symbol)
        .await
        .map_err(internal)?;
    if !removed {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("{} is not in list {id}", symbol.to_uppercase()),
        ));
    }
    state.live.bump_watchlist_revision();
    live::refresh_symbols(&state).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
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
