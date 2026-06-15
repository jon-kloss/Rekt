//! Public-demo seeding. The demo is a shared, unauthenticated instance whose
//! mutable state self-heals: on boot (when empty) and on a periodic timer we
//! restore a baked snapshot of a realistic portfolio + pre-generated AI
//! analyses. Candles and snapshots are NOT seeded — they refetch live from the
//! market feed, so the seed stays small and the charts/gauges reflect real
//! recent data. Only ever active when `AppState.demo` is set (REKT_DEMO=1).

use std::time::Duration;

use axum::{extract::State, response::Json};
use serde_json::{json, Value};

use crate::api::{self, internal, ApiError};
use crate::AppState;

/// Tables the reseed CLEARS (children before parents) before re-applying the
/// seed SQL. Deliberately excluded:
/// - `instruments` — reference data referenced by candles/orders/fills (which
///   we keep), so clearing it would violate those FKs. The seed re-inserts it
///   idempotently (INSERT OR IGNORE), keeping ids stable across reseeds.
/// - `settings` — holds `candle_floor:*` backfill bookkeeping that must track
///   the live-refetched (un-seeded) candles, not the seed.
const CLEAR_TABLES: &[&str] = &[
    "recommendations",
    "analyses",
    "alerts",
    "watchlist_members",
    "watchlists",
    "transactions",
];

/// The baked snapshot: a realistic portfolio + real, pre-generated AI analyses
/// (so the AI tabs are populated without any live, cost-bearing call).
const SEED_SQL: &str = include_str!("../assets/demo_seed.sql");

/// Seed a fresh instance (no transactions yet). No-op once seeded.
pub async fn seed_if_empty(state: &AppState) -> anyhow::Result<()> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transactions")
        .fetch_one(&state.db)
        .await?;
    if n == 0 {
        apply_seed(state).await?;
        tracing::info!("demo: seeded a fresh instance");
    }
    Ok(())
}

/// Restore the baked seed: clear the owned tables and re-apply. Leaves candles
/// and snapshots intact (live market data, expensive to refetch).
pub async fn reseed(state: &AppState) -> anyhow::Result<()> {
    apply_seed(state).await?;
    tracing::info!("demo: reseeded to the baked snapshot");
    Ok(())
}

async fn apply_seed(state: &AppState) -> anyhow::Result<()> {
    // Serialize against any concurrent mutation (validation replays the log).
    let _guard = state.mutations.lock().await;
    // One atomic script: clear the owned tables (children first), then apply the
    // seed's INSERTs. Wrapped in a transaction so a partial failure rolls back.
    let deletes: String = CLEAR_TABLES
        .iter()
        .map(|t| format!("DELETE FROM {t};\n"))
        .collect();
    let script = format!("BEGIN IMMEDIATE;\n{deletes}{SEED_SQL}\nCOMMIT;\n");
    if let Err(e) = sqlx::raw_sql(&script).execute(&state.db).await {
        // Don't leak an open transaction onto the pooled connection.
        let _ = sqlx::raw_sql("ROLLBACK;").execute(&state.db).await;
        return Err(e.into());
    }
    // Reflect the reset in the revision-gated live caches + the symbol set.
    state.live.bump_tx_revision();
    state.live.bump_alerts_revision();
    state.live.bump_watchlist_revision();
    crate::live::refresh_symbols(state).await?;
    Ok(())
}

/// Background task (demo only): reseed every REKT_DEMO_RESET_HOURS (default 6h)
/// so a visitor's changes self-heal.
pub fn spawn_reseed_task(state: AppState) {
    let hours: u64 = std::env::var("REKT_DEMO_RESET_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|h| *h > 0)
        .unwrap_or(6);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(hours * 3600));
        ticker.tick().await; // the immediate first tick — we just seeded on boot
        loop {
            ticker.tick().await;
            if let Err(e) = reseed(&state).await {
                tracing::error!(error = %e, "demo reseed failed");
            }
        }
    });
}

/// POST /api/demo/reset — visitor-triggered reseed. Demo-only (404 otherwise).
pub async fn reset(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    if !state.demo {
        return Err(api::err(
            axum::http::StatusCode::NOT_FOUND,
            "not a demo instance",
        ));
    }
    reseed(&state).await.map_err(internal)?;
    Ok(Json(json!({ "reset": true })))
}
