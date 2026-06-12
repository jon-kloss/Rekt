//! Alerts-to-action (PLAN.md §4): price/drawdown alerts that, when
//! triggered, surface an optional pre-staged order ticket for one-click
//! HUMAN confirmation. Nothing here executes orders — confirmed tickets go
//! through the normal /api/orders path with every guardrail applied.

use std::sync::OnceLock;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use rekt_core::alerts::AlertCondition;
use rekt_core::orders::{OrderType, Side, TimeInForce};
use rekt_core::signals::max_drawdown_pct;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::api::{err, internal, ApiError};
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
}

/// POST /api/alerts — create an alert (optionally with a pre-staged ticket).
/// Draft tickets are shape-checked NOW so a bad draft fails at creation,
/// not when the alert fires at 3am.
pub async fn create_alert(
    State(state): State<AppState>,
    Json(input): Json<AlertInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let symbol = input.symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "symbol is required"));
    }
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
            if draft.qty <= Decimal::ZERO {
                return Err(err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "draft order qty must be positive",
                ));
            }
            match draft.order_type {
                OrderType::Limit if draft.limit_price.unwrap_or_default() <= Decimal::ZERO => {
                    return Err(err(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "draft limit orders require a positive limit price",
                    ));
                }
                OrderType::Market if draft.limit_price.is_some() => {
                    return Err(err(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "draft market orders cannot carry a limit price",
                    ));
                }
                _ => {}
            }
            Some(serde_json::to_string(draft).map_err(internal)?)
        }
        None => None,
    };

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
    state.live.bump_alerts_revision();
    crate::live::refresh_symbols(&state)
        .await
        .map_err(internal)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

/// GET /api/alerts — every alert, triggered first.
pub async fn list_alerts(
    State(state): State<AppState>,
) -> Result<Json<Vec<repo::AlertRecord>>, ApiError> {
    Ok(Json(repo::list_alerts(&state.db).await.map_err(internal)?))
}

pub async fn delete_alert(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if !repo::delete_alert(&state.db, id).await.map_err(internal)? {
        return Err(err(StatusCode::NOT_FOUND, format!("no alert {id}")));
    }
    state.live.bump_alerts_revision();
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
    state.live.bump_alerts_revision();
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/alerts/{id}/rearm — back to active, clearing the trigger.
pub async fn rearm_alert(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if !repo::rearm_alert(&state.db, id).await.map_err(internal)? {
        return Err(err(StatusCode::NOT_FOUND, format!("no alert {id}")));
    }
    state.live.bump_alerts_revision();
    Ok(StatusCode::NO_CONTENT)
}

/// Evaluate every active alert against current data; fires triggers and
/// notifications. Runs on the scheduler — cheap when nothing is active.
pub async fn evaluate_alerts(state: &AppState) -> anyhow::Result<()> {
    let alerts = repo::active_alerts(&state.db).await?;
    if alerts.is_empty() {
        return Ok(());
    }
    let prices = state.live.price_views().await;
    let mut drawdowns: std::collections::HashMap<String, Option<Decimal>> =
        std::collections::HashMap::new();

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
            match drawdowns.entry(alert.symbol.clone()) {
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
