//! REKT server binary: axum API + SQLite + embedded UI shell.
//!
//! Configuration via environment:
//! - `REKT_DB`           sqlite path (default `rekt.db`)
//! - `REKT_LISTEN`       listen address (default `127.0.0.1:7777`)
//! - `FINNHUB_API_KEY`   enables live quotes; without it the API answers
//!   503 with an explanation instead of pretending.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use rekt_data::{finnhub::Finnhub, DataError, MarketData};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    market: Option<Arc<dyn MarketData>>,
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/quote/{symbol}", get(quote))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = sqlx::query("SELECT 1").execute(&state.db).await.is_ok();
    let market = match &state.market {
        Some(provider) => provider.name(),
        None => "unconfigured",
    };
    Json(serde_json::json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "db": db_ok,
        "market_data": market,
    }))
}

async fn quote(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
) -> Result<Json<rekt_core::Quote>, (StatusCode, Json<serde_json::Value>)> {
    let err = |status: StatusCode, msg: String| (status, Json(serde_json::json!({ "error": msg })));

    let Some(provider) = &state.market else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no market data provider configured — set FINNHUB_API_KEY".into(),
        ));
    };

    let symbol = symbol.to_uppercase();
    match provider.quote(&symbol).await {
        Ok(quote) => Ok(Json(quote)),
        Err(DataError::SymbolNotFound(s)) => {
            Err(err(StatusCode::NOT_FOUND, format!("unknown symbol: {s}")))
        }
        Err(DataError::RateLimited) => Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "provider rate limit hit, try again shortly".into(),
        )),
        Err(DataError::Upstream(msg)) => Err(err(StatusCode::BAD_GATEWAY, msg)),
    }
}

async fn open_db(path: &str) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new().connect_with(options).await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;
    Ok(pool)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let db_path = std::env::var("REKT_DB").unwrap_or_else(|_| "rekt.db".into());
    let listen = std::env::var("REKT_LISTEN").unwrap_or_else(|_| "127.0.0.1:7777".into());

    let db = open_db(&db_path).await?;
    tracing::info!(db = %db_path, "database ready, migrations applied");

    let market: Option<Arc<dyn MarketData>> = match std::env::var("FINNHUB_API_KEY") {
        Ok(token) if !token.is_empty() => Some(Arc::new(Finnhub::new(token))),
        _ => {
            tracing::warn!("FINNHUB_API_KEY not set — /api/quote will answer 503");
            None
        }
    };

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(addr = %listen, "REKT listening — how rekt are you today?");
    axum::serve(listener, app(AppState { db, market }))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
        AppState {
            db: pool,
            market: None,
        }
    }

    #[tokio::test]
    async fn health_reports_ok_with_migrated_db() {
        let response = app(test_state().await)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["market_data"], "unconfigured");
    }

    #[tokio::test]
    async fn quote_without_provider_is_503_with_explanation() {
        let response = app(test_state().await)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/quote/aapl")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("FINNHUB_API_KEY"));
    }

    #[tokio::test]
    async fn index_serves_embedded_shell() {
        let response = app(test_state().await)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
