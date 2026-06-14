//! Market-idea screener: runs the deterministic core
//! ([`rekt_core::screener`]) over a watchlist's members, ranked, using the
//! per-equity-type aggressiveness. No AI here — these candidates are what the
//! analyst is later asked to turn into theses.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use rekt_core::screener::{screen, Aggressiveness};
use rekt_core::signals::summarize;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{err, internal, ApiError};
use crate::live::SIGNAL_WINDOW_BARS;
use crate::{repo, AppState};

const STOCK_KEY: &str = "screener.aggressiveness.stock";
const ETF_KEY: &str = "screener.aggressiveness.etf";
/// A symbol needs at least this many daily closes for signals to be meaningful.
const MIN_BARS: usize = 30;

/// The stored aggressiveness for one equity type (defaults to Balanced).
pub async fn aggressiveness_for(pool: &sqlx::SqlitePool, kind: &str) -> Aggressiveness {
    let key = if kind == "etf" { ETF_KEY } else { STOCK_KEY };
    let stored = repo::get_setting(pool, key)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    Aggressiveness::parse_or_balanced(&stored)
}

/// GET /api/screener/settings — the per-type aggressiveness.
pub async fn get_settings(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "stock": aggressiveness_for(&state.db, "stock").await.as_str(),
        "etf": aggressiveness_for(&state.db, "etf").await.as_str(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct SettingsInput {
    pub stock: String,
    pub etf: String,
}

/// PUT /api/screener/settings {stock, etf} — set the per-type aggressiveness.
pub async fn put_settings(
    State(state): State<AppState>,
    Json(input): Json<SettingsInput>,
) -> Result<Json<Value>, ApiError> {
    // Normalize through the enum so only valid levels are ever stored.
    let stock = Aggressiveness::parse_or_balanced(&input.stock);
    let etf = Aggressiveness::parse_or_balanced(&input.etf);
    let _guard = state.mutations.lock().await;
    repo::set_setting(&state.db, STOCK_KEY, stock.as_str())
        .await
        .map_err(internal)?;
    repo::set_setting(&state.db, ETF_KEY, etf.as_str())
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({ "stock": stock.as_str(), "etf": etf.as_str() }),
    ))
}

/// One ranked candidate plus the context the UI/AI need.
pub struct Scored {
    pub symbol: String,
    pub kind: String,
    pub aggr: Aggressiveness,
    pub candidate: rekt_core::screener::Candidate,
}

/// Screen a list's members into ranked candidates (strongest first), shared by
/// the endpoint and the AI-thesis flow. Returns (scored, scanned, awaiting_data).
pub async fn scored_candidates(
    state: &AppState,
    list_id: i64,
) -> anyhow::Result<(Vec<Scored>, usize, usize)> {
    let members = repo::watchlist_members_detail(&state.db, list_id).await?;
    let stock_aggr = aggressiveness_for(&state.db, "stock").await;
    let etf_aggr = aggressiveness_for(&state.db, "etf").await;
    let mut scored = Vec::new();
    let mut scanned = 0usize;
    let mut awaiting_data = 0usize;
    for (symbol, kind) in members {
        let closes = repo::recent_closes(&state.db, &symbol, SIGNAL_WINDOW_BARS).await?;
        if closes.len() < MIN_BARS {
            awaiting_data += 1;
            continue;
        }
        scanned += 1;
        let aggr = if kind == "etf" { etf_aggr } else { stock_aggr };
        if let Some(candidate) = screen(&summarize(&closes), aggr) {
            scored.push(Scored {
                symbol,
                kind,
                aggr,
                candidate,
            });
        }
    }
    scored.sort_by(|a, b| b.candidate.score.cmp(&a.candidate.score));
    Ok((scored, scanned, awaiting_data))
}

/// GET /api/screener/{list_id} — ranked buy/sell candidates from a list's
/// members. Deterministic (signal-derived); the AI layer narrates these later.
pub async fn screen_list(
    State(state): State<AppState>,
    Path(list_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let lists = repo::list_watchlists(&state.db).await.map_err(internal)?;
    if !lists.iter().any(|l| l.id == list_id) {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no watchlist {list_id}"),
        ));
    }
    let (scored, scanned, awaiting_data) =
        scored_candidates(&state, list_id).await.map_err(internal)?;
    let ideas: Vec<Value> = scored
        .into_iter()
        .map(|s| {
            json!({
                "symbol": s.symbol,
                "kind": s.kind,
                "action": s.candidate.action,
                "score": s.candidate.score,
                "reasons": s.candidate.reasons,
                "aggressiveness": s.aggr.as_str(),
            })
        })
        .collect();
    Ok(Json(json!({
        "list_id": list_id,
        "scanned": scanned,
        "awaiting_data": awaiting_data,
        "ideas": ideas,
    })))
}

/// Format the screened candidates as a prompt block the analyst narrates.
pub fn candidates_prompt_block(list_name: &str, scored: &[Scored]) -> String {
    let mut out = format!(
        "A deterministic signal screen of the user's \"{list_name}\" watchlist surfaced these \
         ranked candidates. The action and the signals are FACTS from the screen — ground each \
         thesis in them; do NOT add tickers not listed and do NOT drop any:\n"
    );
    for s in scored {
        out.push_str(&format!(
            "- {} {} (score {}): {}\n",
            s.candidate.action.to_uppercase(),
            s.symbol,
            s.candidate.score,
            s.candidate.reasons.join("; ")
        ));
    }
    out
}

/// POST /api/screener/{list_id}/analyze — kick off an AI pass that writes a
/// thesis per screened candidate (persisted as tracked recommendations). Does
/// nothing (and bills nothing) if the screen is empty.
pub async fn analyze_list(
    State(state): State<AppState>,
    Path(list_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let lists = repo::list_watchlists(&state.db).await.map_err(internal)?;
    if !lists.iter().any(|l| l.id == list_id) {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no watchlist {list_id}"),
        ));
    }
    let (scored, _, _) = scored_candidates(&state, list_id).await.map_err(internal)?;
    if scored.is_empty() {
        return Ok(Json(json!({ "started": false, "ideas": 0 })));
    }
    let id = crate::analyst::start_run(&state, "market_ideas", Some(list_id.to_string())).await?;
    Ok(Json(
        json!({ "started": true, "id": id, "ideas": scored.len() }),
    ))
}
