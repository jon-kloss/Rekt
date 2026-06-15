//! GET /api/splits — flag stock splits the user held through but never
//! recorded. REKT's candles are split-adjusted while transactions are
//! as-traded, so a missing split corrupts BOTH the live valuation and the tax
//! basis. Needs a corporate-actions-capable data provider (Alpaca); degrades
//! honestly when one isn't configured. The result is cached per process —
//! split history barely changes — and invalidated when the tx log mutates.

use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::response::Json;
use chrono::Utc;
use rekt_core::splits::detect_missing_splits;
use rekt_core::taxes::ny_date;
use serde_json::{json, Value};

use crate::api::{internal, ApiError};
use crate::{repo, AppState};

static CACHE: LazyLock<Mutex<Option<(Instant, Value)>>> = LazyLock::new(|| Mutex::new(None));
const TTL: Duration = Duration::from_secs(6 * 3600);

/// Drop the cached result so the next check recomputes — called after any tx
/// mutation (a new/removed/imported tx can change which splits are missing).
pub fn invalidate() {
    *CACHE.lock().unwrap() = None;
}

#[derive(Debug, serde::Deserialize)]
pub struct CheckQuery {
    /// Bypass the cache and re-query the provider.
    #[serde(default)]
    pub refresh: bool,
}

/// GET /api/splits — `{ supported, checked, missing: [{symbol, ex_date, ratio,
/// shares_held}] }`, or `{ supported: false, reason }` when no provider.
pub async fn check(
    State(state): State<AppState>,
    Query(q): Query<CheckQuery>,
) -> Result<Json<Value>, ApiError> {
    if !q.refresh {
        if let Some((at, cached)) = CACHE.lock().unwrap().as_ref() {
            if at.elapsed() < TTL {
                return Ok(Json(cached.clone()));
            }
        }
    }
    let body = compute(&state).await?;
    *CACHE.lock().unwrap() = Some((Instant::now(), body.clone()));
    Ok(Json(body))
}

async fn compute(state: &AppState) -> Result<Value, ApiError> {
    let Some(bars) = &state.bars else {
        return Ok(json!({
            "supported": false,
            "reason": "split detection needs Alpaca's corporate-actions data — set ALPACA_PAPER_KEY / ALPACA_PAPER_SECRET",
            "missing": [],
        }));
    };
    // Live transactions only — paper fills aren't real holdings.
    let txs = repo::fetch_mode_txs(&state.db, "live")
        .await
        .map_err(internal)?;
    let Some(first) = txs.first() else {
        return Ok(json!({ "supported": true, "checked": 0, "missing": [] }));
    };
    let symbols: Vec<String> = txs
        .iter()
        .filter_map(|t| t.symbol.as_deref().map(|s| s.trim().to_uppercase()))
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // txs is (ts, id)-sorted, so `first` is the earliest acquisition.
    let start = ny_date(first.ts);
    let today = ny_date(Utc::now());

    let events = match bars.splits(&symbols, start, today).await {
        Ok(events) => events,
        // A non-corporate-actions provider — degrade honestly, don't 500.
        Err(rekt_data::DataError::Unsupported(_)) => {
            return Ok(json!({
                "supported": false,
                "reason": "the active data provider has no corporate-actions API",
                "missing": [],
            }));
        }
        Err(e) => return Err(internal(e)),
    };
    let missing = detect_missing_splits(&txs, &events);
    tracing::debug!(
        checked = symbols.len(),
        missing = missing.len(),
        "GET /api/splits"
    );
    Ok(json!({ "supported": true, "checked": symbols.len(), "missing": missing }))
}
