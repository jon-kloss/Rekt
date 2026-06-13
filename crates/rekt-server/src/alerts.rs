//! Alerts-to-action (PLAN.md §4): price/drawdown alerts that, when
//! triggered, surface an optional pre-staged order ticket for one-click
//! HUMAN confirmation. Nothing here executes orders — confirmed tickets go
//! through the normal /api/orders path with every guardrail applied.

use std::collections::HashMap;
use std::sync::OnceLock;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use rekt_core::alerts::AlertCondition;
use rekt_core::orders::{check_shape, OrderType, Side, TimeInForce};
use rekt_core::signals::max_drawdown_pct;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::api::{err, internal, validate_symbol, ApiError};
use crate::live::SIGNAL_WINDOW_BARS;
use crate::{repo, AppState};

/// The pre-staged ticket carried by an alert. Mirrors `trading::TicketInput`
/// minus the symbol (the alert's instrument is the symbol) so a stored
/// draft can never silently point at a different name than the alert.
#[derive(Debug, Serialize, Deserialize)]
pub struct DraftOrder {
    pub side: Side,
    pub order_type: OrderType,
    pub qty: Decimal,
    #[serde(default)]
    pub limit_price: Option<Decimal>,
    #[serde(default)]
    pub tif: Option<TimeInForce>,
}

#[derive(Debug, Deserialize)]
pub struct AlertInput {
    pub symbol: String,
    pub condition: String,
    pub threshold: Decimal,
    #[serde(default)]
    pub draft_order: Option<DraftOrder>,
    #[serde(default)]
    pub note: Option<String>,
    /// Arm even if the condition is ALREADY satisfied (it would then fire
    /// on the next evaluation). Off by default: "alert me at ≤170" while
    /// the price sits at 150 is almost always a mistake, not intent.
    #[serde(default)]
    pub allow_immediate: bool,
}

/// What the condition's input currently reads: last cached price for price
/// conditions, rolling drawdown over cached candles for drawdown. `None`
/// when no data exists yet.
async fn current_observed(
    state: &AppState,
    condition: AlertCondition,
    symbol: &str,
) -> Option<Decimal> {
    if condition.needs_price() {
        state.live.price_views().await.get(symbol).map(|p| p.price)
    } else {
        let closes = repo::recent_closes(&state.db, symbol, SIGNAL_WINDOW_BARS)
            .await
            .ok()?;
        max_drawdown_pct(&closes)
    }
}

/// POST /api/alerts — create an alert (optionally with a pre-staged ticket).
/// Draft tickets are SHAPE-checked now (via the same `check_shape` rules
/// real orders use) so a malformed draft fails at creation, not when the
/// alert fires at 3am. The contextual guardrails (notional cap, breaker,
/// long-only) deliberately run at confirm time instead — they depend on
/// account state at the moment the user places the order, not now.
/// An already-satisfied condition is rejected so a fresh alert can't fire
/// 15 seconds after you arm it.
pub async fn create_alert(
    State(state): State<AppState>,
    Json(input): Json<AlertInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    tracing::debug!(symbol = %input.symbol, condition = %input.condition, "POST /api/alerts");
    let symbol = validate_symbol(&input.symbol)?;
    let condition: AlertCondition = input
        .condition
        .parse()
        .map_err(|m: String| err(StatusCode::UNPROCESSABLE_ENTITY, m))?;
    if input.threshold <= Decimal::ZERO {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "threshold must be positive",
        ));
    }
    let draft_json = match &input.draft_order {
        Some(draft) => {
            check_shape(draft.order_type, draft.qty, draft.limit_price).map_err(|v| {
                err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("draft order: {v}"),
                )
            })?;
            Some(serde_json::to_string(draft).map_err(internal)?)
        }
        None => None,
    };

    // Arm-time check: an alert means "tell me when this BECOMES true". If
    // it's already true (including against a stale after-hours price), the
    // next evaluator tick would fire it instantly — reject unless the user
    // explicitly opted in. With no data yet the check can't run; be honest
    // about that in the response instead of implying it passed.
    let observed = current_observed(&state, condition, &symbol).await;
    if !input.allow_immediate {
        if let Some(observed) = observed {
            if rekt_core::alerts::check(condition, input.threshold, observed).is_some() {
                return Err(err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "condition is already true ({} — currently {observed}); this would fire \
                         immediately. Pick a threshold the market hasn't met, or pass \
                         allow_immediate to arm anyway",
                        condition.describe(&symbol, input.threshold)
                    ),
                ));
            }
        }
    }

    let id = repo::insert_alert(
        &state.db,
        &symbol,
        condition.as_str(),
        input.threshold,
        draft_json.as_deref(),
        input.note.as_deref().unwrap_or(""),
    )
    .await
    .map_err(internal)?;

    // Subscribe the symbol so the condition has data to evaluate against.
    alert_set_changed(&state).await?;
    let mut response = serde_json::json!({ "id": id });
    if observed.is_none() && !input.allow_immediate {
        response["note"] = serde_json::Value::String(format!(
            "no market data for {symbol} yet — if the condition is already true when data \
             arrives, the alert will fire then"
        ));
    }
    Ok((StatusCode::CREATED, Json(response)))
}

/// GET /api/alerts — every alert, triggered first.
pub async fn list_alerts(
    State(state): State<AppState>,
) -> Result<Json<Vec<repo::AlertRecord>>, ApiError> {
    Ok(Json(repo::list_alerts(&state.db).await.map_err(internal)?))
}

/// Every alert mutation changes the ACTIVE set, which feeds the stream
/// subscription and backfill sets — refresh after each one (cheap,
/// idempotent), so e.g. deleting the last alert on a not-held symbol
/// actually unsubscribes it.
async fn alert_set_changed(state: &AppState) -> Result<(), ApiError> {
    state.live.bump_alerts_revision();
    crate::live::refresh_symbols(state).await.map_err(internal)
}

pub async fn delete_alert(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if !repo::delete_alert(&state.db, id).await.map_err(internal)? {
        return Err(err(StatusCode::NOT_FOUND, format!("no alert {id}")));
    }
    alert_set_changed(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/alerts/{id}/dismiss — acknowledge a triggered alert.
pub async fn dismiss_alert(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if !repo::dismiss_alert(&state.db, id).await.map_err(internal)? {
        return Err(err(
            StatusCode::CONFLICT,
            format!("alert {id} is not in triggered state"),
        ));
    }
    alert_set_changed(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/alerts/{id}/rearm — back to active, clearing the trigger.
/// Same arm-time rule as creation: a condition that is STILL true hasn't
/// "become true again", and re-arming it would just re-fire 15s later.
pub async fn rearm_alert(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let Some(alert) = repo::get_alert(&state.db, id).await.map_err(internal)? else {
        return Err(err(StatusCode::NOT_FOUND, format!("no alert {id}")));
    };
    if let Ok(condition) = alert.condition.parse::<AlertCondition>() {
        if let Some(observed) = current_observed(&state, condition, &alert.symbol).await {
            if rekt_core::alerts::check(condition, alert.threshold, observed).is_some() {
                return Err(err(
                    StatusCode::CONFLICT,
                    format!(
                        "condition is still true ({} — currently {observed}); re-arming would \
                         fire it again immediately. It can re-arm once the condition resets, \
                         or delete and recreate the alert with allow_immediate",
                        condition.describe(&alert.symbol, alert.threshold)
                    ),
                ));
            }
        }
    }
    if !repo::rearm_alert(&state.db, id).await.map_err(internal)? {
        return Err(err(StatusCode::NOT_FOUND, format!("no alert {id}")));
    }
    alert_set_changed(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Drawdown values carried across evaluator ticks: drawdown only changes
/// when new candles land, so the cache is invalidated by candles_revision
/// instead of being re-queried from SQLite every 15 seconds.
#[derive(Default)]
pub struct DrawdownCache {
    candles_rev: u64,
    map: HashMap<String, Option<Decimal>>,
}

/// Evaluate every active alert against current data; fires triggers and
/// notifications. Runs on the scheduler — cheap when nothing is active.
pub async fn evaluate_alerts(
    state: &AppState,
    drawdowns: &mut DrawdownCache,
) -> anyhow::Result<()> {
    let alerts = repo::active_alerts(&state.db).await?;
    if alerts.is_empty() {
        return Ok(());
    }
    let prices = state.live.price_views().await;
    let (_, candles_rev) = state.live.revisions();
    if drawdowns.candles_rev != candles_rev {
        drawdowns.map.clear();
        drawdowns.candles_rev = candles_rev;
    }

    let mut any_triggered = false;
    for alert in &alerts {
        let Ok(condition) = alert.condition.parse::<AlertCondition>() else {
            tracing::error!(alert = alert.id, condition = %alert.condition, "unknown alert condition in DB");
            continue;
        };
        let observed = if condition.needs_price() {
            prices.get(&alert.symbol).map(|p| p.price)
        } else {
            // Rolling drawdown over the same window the signal badges use.
            match drawdowns.map.entry(alert.symbol.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => *e.get(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let dd = match repo::recent_closes(&state.db, &alert.symbol, SIGNAL_WINDOW_BARS)
                        .await
                    {
                        Ok(closes) => max_drawdown_pct(&closes),
                        Err(e) => {
                            tracing::warn!(symbol = %alert.symbol, error = %e, "drawdown closes fetch failed");
                            None
                        }
                    };
                    *e.insert(dd)
                }
            }
        };
        let Some(observed) = observed else {
            continue; // no data yet — stays armed
        };
        if let Some(value) = rekt_core::alerts::check(condition, alert.threshold, observed) {
            // Status-guarded UPDATE: a concurrent pass can't double-fire.
            if repo::trigger_alert(&state.db, alert.id, value).await? {
                any_triggered = true;
                let desc = condition.describe(&alert.symbol, alert.threshold);
                tracing::info!(alert = alert.id, %desc, observed = %value, "alert triggered");
                let body = match &alert.draft_order {
                    Some(_) => {
                        format!("now {value} — a pre-staged order is ready to confirm in REKT")
                    }
                    None => format!("now {value}"),
                };
                send_notification(state, &desc, &body).await;
            }
        }
    }
    if any_triggered {
        state.live.bump_alerts_revision();
        // Triggered alerts leave the ACTIVE set — drop subscriptions for
        // symbols nothing else watches (don't fail the eval pass over it).
        if let Err(e) = crate::live::refresh_symbols(state).await {
            tracing::warn!(error = %e, "post-trigger symbol refresh failed");
        }
    }
    Ok(())
}

/// Push a notification via ntfy (https://ntfy.sh or self-hosted) when
/// configured; otherwise the dashboard banner is the notification.
async fn send_notification(state: &AppState, title: &str, body: &str) {
    let Some(url) = &state.notify_url else {
        return;
    };
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client")
    });
    let result = client
        .post(url)
        .header("Title", title)
        .header("Tags", "rotating_light")
        .body(body.to_string())
        .send()
        .await;
    match result {
        Ok(response) if !response.status().is_success() => {
            tracing::warn!(status = %response.status(), "ntfy push rejected");
        }
        Err(e) => tracing::warn!(error = %e, "ntfy push failed"),
        _ => {}
    }
}
