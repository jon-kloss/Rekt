//! REKT server binary: axum API + SQLite + live pipeline + embedded UI.
//!
//! Configuration via environment:
//! - `REKT_DB`           sqlite path (default `rekt.db`)
//! - `REKT_LISTEN`       listen address (default `127.0.0.1:7777`)
//! - `FINNHUB_API_KEY`   enables quotes + the live trade stream; without it
//!   the API answers 503 with an explanation instead of pretending.

mod api;
mod live;
mod repo;

use std::sync::Arc;

use axum::{
    extract::{Path, State, WebSocketUpgrade},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use rekt_data::{finnhub::Finnhub, DataError, MarketData};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use tokio::sync::{broadcast, watch};

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub market: Option<Arc<dyn MarketData>>,
    pub finnhub_token: Option<String>,
    pub live: Arc<live::Live>,
    pub snapshots: broadcast::Sender<String>,
    pub symbols: Arc<watch::Sender<Vec<String>>>,
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/quote/{symbol}", get(quote))
        .route("/api/portfolio", get(api::portfolio))
        .route("/api/transactions", get(api::list_txs).post(api::create_tx))
        .route("/api/transactions/{id}", delete(api::delete_tx))
        .route("/api/import/csv", post(api::import_csv))
        .route("/api/ws", get(ws_upgrade))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn ws_upgrade(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| live::client_ws(socket, state))
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

    let finnhub_token = std::env::var("FINNHUB_API_KEY")
        .ok()
        .filter(|t| !t.is_empty());
    let market: Option<Arc<dyn MarketData>> = match &finnhub_token {
        Some(token) => Some(Arc::new(Finnhub::new(token.clone()))),
        None => {
            tracing::warn!("FINNHUB_API_KEY not set — quotes and live feed disabled");
            None
        }
    };

    let state = live::start(move |handles| AppState {
        db,
        market,
        finnhub_token,
        live: handles.live,
        snapshots: handles.snapshots,
        symbols: Arc::new(handles.symbols),
    });

    // Subscribe the stream to currently held symbols from the start.
    live::refresh_symbols(&state).await?;

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(addr = %listen, "REKT listening — how rekt are you today?");
    axum::serve(listener, app(state))
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
        let (snapshots, _) = broadcast::channel(4);
        let (symbols, _) = watch::channel(Vec::new());
        AppState {
            db: pool,
            market: None,
            finnhub_token: None,
            live: Arc::new(live::Live::default()),
            snapshots,
            symbols: Arc::new(symbols),
        }
    }

    async fn request(
        state: AppState,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let builder = axum::http::Request::builder().method(method).uri(uri);
        let request = match body {
            Some(json) => builder
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap(),
            None => builder.body(axum::body::Body::empty()).unwrap(),
        };
        let response = app(state).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    #[tokio::test]
    async fn health_reports_ok_with_migrated_db() {
        let (status, json) = request(test_state().await, "GET", "/api/health", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["status"], "ok");
        assert_eq!(json["market_data"], "unconfigured");
    }

    #[tokio::test]
    async fn quote_without_provider_is_503_with_explanation() {
        let (status, json) = request(test_state().await, "GET", "/api/quote/aapl", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(json["error"].as_str().unwrap().contains("FINNHUB_API_KEY"));
    }

    #[tokio::test]
    async fn transaction_lifecycle_and_portfolio_math() {
        let state = test_state().await;

        // Deposit, then buy 10 AAPL @ 100 with $1 fee.
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/transactions",
            Some(serde_json::json!({ "kind": "deposit", "price": "5000" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/transactions",
            Some(serde_json::json!({
                "kind": "buy", "symbol": "aapl", "qty": "10", "price": "100", "fees": "1"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, json) = request(state.clone(), "GET", "/api/portfolio", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["portfolio"]["cash"], "3999");
        assert_eq!(json["portfolio"]["positions"][0]["symbol"], "AAPL");
        assert_eq!(json["portfolio"]["positions"][0]["qty"], "10");
        assert_eq!(json["portfolio"]["positions"][0]["avg_cost"], "100.10");
        // No provider → position is unpriced, and the payload says so.
        assert_eq!(json["portfolio"]["unpriced_symbols"][0], "AAPL");
        assert_eq!(json["live_feed"], false);
    }

    #[tokio::test]
    async fn oversell_is_rejected_with_422() {
        let state = test_state().await;
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/transactions",
            Some(serde_json::json!({ "kind": "buy", "symbol": "VOO", "qty": "5", "price": "400" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, json) = request(
            state.clone(),
            "POST",
            "/api/transactions",
            Some(
                serde_json::json!({ "kind": "sell", "symbol": "VOO", "qty": "6", "price": "400" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(json["error"].as_str().unwrap().contains("only 5 held"));
    }

    #[tokio::test]
    async fn delete_recomputes_and_404s_on_missing() {
        let state = test_state().await;
        let (_, created) = request(
            state.clone(),
            "POST",
            "/api/transactions",
            Some(serde_json::json!({ "kind": "deposit", "price": "100" })),
        )
        .await;
        let id = created["id"].as_i64().unwrap();

        let (status, _) = request(
            state.clone(),
            "DELETE",
            &format!("/api/transactions/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = request(
            state.clone(),
            "DELETE",
            &format!("/api/transactions/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn csv_import_is_all_or_nothing() {
        let state = test_state().await;
        // Second row oversells — nothing must be imported.
        let bad_csv = "kind,symbol,qty,price,fees,taxes,ts,note\n\
                       buy,AAPL,5,100,,,,\n\
                       sell,AAPL,6,110,,,,";
        let request_body = axum::http::Request::builder()
            .method("POST")
            .uri("/api/import/csv")
            .header("content-type", "text/csv")
            .body(axum::body::Body::from(bad_csv))
            .unwrap();
        let response = app(state.clone()).oneshot(request_body).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let (_, txs) = request(state.clone(), "GET", "/api/transactions", None).await;
        assert_eq!(txs.as_array().unwrap().len(), 0);

        // A clean file imports fully.
        let good_csv = "kind,symbol,qty,price,fees,taxes,ts,note\n\
                        deposit,,,2000,,,,seed\n\
                        buy,AAPL,5,100,1,,2026-01-05T15:00:00Z,first buy";
        let request_body = axum::http::Request::builder()
            .method("POST")
            .uri("/api/import/csv")
            .header("content-type", "text/csv")
            .body(axum::body::Body::from(good_csv))
            .unwrap();
        let response = app(state.clone()).oneshot(request_body).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let (_, txs) = request(state, "GET", "/api/transactions", None).await;
        assert_eq!(txs.as_array().unwrap().len(), 2);
    }
}
