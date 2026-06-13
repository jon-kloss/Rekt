//! REKT server binary: axum API + SQLite + live pipeline + embedded UI.
//!
//! Configuration via environment:
//! - `REKT_DB`           sqlite path (default `rekt.db`)
//! - `REKT_LISTEN`       listen address (default `127.0.0.1:7777`)
//! - `FINNHUB_API_KEY`   enables quotes + the live trade stream; without it
//!   the API answers 503 with an explanation instead of pretending.

mod alerts;
mod analyst;
mod api;
mod history;
mod import;
mod live;
mod portfolios;
mod repo;
mod taxes;
mod trading;

use std::sync::atomic::AtomicBool;
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
use tokio::sync::{broadcast, watch, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub market: Option<Arc<dyn MarketData>>,
    /// Daily-bars provider (Alpaca data API) for candles/backfill.
    pub bars: Option<Arc<dyn MarketData>>,
    pub finnhub_token: Option<String>,
    pub broker: Option<Arc<dyn rekt_broker::Broker>>,
    pub trading: Arc<trading::TradingState>,
    pub guardrails: Arc<rekt_core::orders::Guardrails>,
    pub live: Arc<live::Live>,
    pub snapshots: broadcast::Sender<String>,
    pub symbols: Arc<watch::Sender<Vec<String>>>,
    /// Serializes transaction mutations: validation replays the whole log,
    /// so validate→insert must be atomic with respect to other mutations.
    pub mutations: Arc<Mutex<()>>,
    /// ntfy push endpoint for triggered alerts (REKT_NTFY_URL / _TOPIC).
    pub notify_url: Option<String>,
    /// Serializes candle backfill runs: the scheduler and on-demand spawns
    /// (watchlist add) must not fetch the same symbol's history twice or
    /// contend for SQLite's single writer.
    pub backfill_lock: Arc<Mutex<()>>,
    /// Claude API transport (ANTHROPIC_API_KEY) — None disables the analyst
    /// honestly. NOTE: the analyst layer never touches `broker` (PLAN.md §5).
    pub analyst: Option<Arc<dyn rekt_analyst::Transport>>,
    /// True when the analyst transport is the Claude Code CLI (`claude -p`):
    /// every kind runs tool-lessly with injected context, since the CLI is
    /// launched with no tools and drives no in-process tool loop.
    pub analyst_cli: bool,
    /// Daily AI spend ceiling (REKT_AI_DAILY_BUDGET, USD).
    pub ai_budget: rust_decimal::Decimal,
    /// Single-flight latch: one analysis at a time.
    pub analyst_running: Arc<AtomicBool>,
    /// The active portfolio's name (for display in health + the snapshot).
    pub active_portfolio: String,
    /// Directory holding the portfolio registry + per-portfolio DB files.
    pub data_dir: std::path::PathBuf,
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
        .route("/api/history", get(history::history))
        .route("/api/candles", get(history::candles))
        .route("/api/taxes", get(taxes::taxes))
        .route("/api/taxes/csv", get(taxes::taxes_csv))
        .route(
            "/api/watchlist",
            get(api::watchlist_list).post(api::watchlist_add),
        )
        .route("/api/watchlist/{symbol}", delete(api::watchlist_remove))
        .route(
            "/api/alerts",
            get(alerts::list_alerts).post(alerts::create_alert),
        )
        .route("/api/alerts/{id}", delete(alerts::delete_alert))
        .route("/api/alerts/{id}/dismiss", post(alerts::dismiss_alert))
        .route("/api/alerts/{id}/rearm", post(alerts::rearm_alert))
        .route("/api/analyst", get(analyst::summary))
        .route("/api/analyst/briefing", post(analyst::run_briefing))
        .route("/api/analyst/review", post(analyst::run_review))
        .route("/api/analyst/ask", post(analyst::ask))
        .route("/api/analyses/{id}", get(analyst::get_analysis))
        .route(
            "/api/recommendations/{id}/accept",
            post(analyst::accept_recommendation),
        )
        .route(
            "/api/recommendations/{id}/dismiss",
            post(analyst::dismiss_recommendation),
        )
        .route(
            "/api/orders",
            get(trading::list_orders).post(trading::submit_order),
        )
        .route("/api/orders/{id}", delete(trading::cancel_order))
        .route("/api/orders/cancel_all", post(trading::cancel_all))
        .route("/api/trading/pause", post(trading::set_paused))
        .route("/api/broker/account", get(trading::account))
        .route(
            "/api/portfolios",
            get(portfolios::list).post(portfolios::create),
        )
        .route("/api/portfolios/switch", post(portfolios::switch))
        .route("/api/portfolios/{name}", delete(portfolios::delete))
        .route("/api/ws", get(ws_upgrade))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn ws_upgrade(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| live::client_ws(socket, state))
}

/// Seconds since the process anchored its start clock. Uses a process-wide
/// `OnceLock` so it survives without threading a field through `AppState`;
/// `main` touches it at boot so real uptime starts there (a test that only
/// hits `/health` anchors on first call, which is fine).
fn uptime_seconds() -> u64 {
    static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    STARTED
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs()
}

/// Liveness + readiness probe. `status` is the liveness signal (`degraded`
/// when the DB is unreachable); `components` is an honest readiness map so
/// an operator can confirm exactly which optional features their env wired
/// up — no secrets, just present/absent.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = sqlx::query("SELECT 1").execute(&state.db).await.is_ok();
    Json(serde_json::json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime_seconds(),
        "db": db_ok,
        "active_portfolio": state.active_portfolio,
        "components": {
            "market_data": match &state.market {
                Some(provider) => provider.name(),
                None => "unconfigured",
            },
            "daily_bars": state.bars.is_some(),
            "trading_paper": state.broker.is_some(),
            "ai_analyst": state.analyst.is_some(),
            "alert_push": state.notify_url.is_some(),
        },
    }))
}

async fn quote(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
) -> Result<Json<rekt_core::Quote>, api::ApiError> {
    let Some(provider) = &state.market else {
        return Err(api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no market data provider configured — set FINNHUB_API_KEY",
        ));
    };

    let symbol = symbol.to_uppercase();
    match provider.quote(&symbol).await {
        Ok(quote) => Ok(Json(quote)),
        Err(DataError::SymbolNotFound(s)) => Err(api::err(
            StatusCode::NOT_FOUND,
            format!("unknown symbol: {s}"),
        )),
        Err(DataError::RateLimited) => Err(api::err(
            StatusCode::TOO_MANY_REQUESTS,
            "provider rate limit hit, try again shortly",
        )),
        Err(DataError::Unsupported(what)) => Err(api::err(
            StatusCode::NOT_IMPLEMENTED,
            format!("provider does not support {what}"),
        )),
        Err(DataError::Upstream(msg)) => Err(api::err(StatusCode::BAD_GATEWAY, msg)),
    }
}

/// Guardrails from env with conservative defaults (PLAN.md §4):
/// REKT_MAX_ORDER_NOTIONAL, REKT_MAX_POSITION_PCT, REKT_MAX_ORDERS_PER_DAY.
fn guardrails_from_env() -> rekt_core::orders::Guardrails {
    let mut rails = rekt_core::orders::Guardrails::default();
    if let Some(v) = std::env::var("REKT_MAX_ORDER_NOTIONAL")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        rails.max_order_notional = v;
    }
    if let Some(v) = std::env::var("REKT_MAX_POSITION_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        rails.max_position_pct = v;
    }
    if let Some(v) = std::env::var("REKT_MAX_ORDERS_PER_DAY")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        rails.max_orders_per_day = v;
    }
    if let Some(v) = std::env::var("REKT_MAX_DAILY_LOSS")
        .ok()
        .and_then(|v| v.parse::<rust_decimal::Decimal>().ok())
    {
        // Zero or negative disables the breaker explicitly.
        rails.max_daily_loss = (v > rust_decimal::Decimal::ZERO).then_some(v);
    }
    rails
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
    uptime_seconds(); // anchor the uptime clock at process start
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "REKT starting");

    let listen = std::env::var("REKT_LISTEN").unwrap_or_else(|_| "127.0.0.1:7777".into());

    // Resolve the active portfolio: one SQLite file per portfolio, selected by
    // a registry outside any single DB. A switch re-execs this process, so this
    // boot path serves whichever portfolio is active.
    let data_dir = portfolios::data_dir();
    let active = portfolios::load(&data_dir).active_entry().clone();
    let active_portfolio = active.name.clone();
    let db_path = portfolios::db_path_for(&data_dir, &active).map_err(|m| anyhow::anyhow!(m))?;
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db_path_str = db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 db path: {}", db_path.display()))?;
    let db = open_db(db_path_str).await?;
    tracing::info!(portfolio = %active_portfolio, db = %db_path.display(), "database ready, migrations applied");

    let finnhub_token = std::env::var("FINNHUB_API_KEY")
        .ok()
        .filter(|t| !t.is_empty());

    // Phase 2 ships PAPER ONLY (PLAN.md §4): live mode arrives behind an
    // explicit toggle after a paper soak, with separate key config.
    // Per-portfolio paper keys (ALPACA_PAPER_KEY_<NAME>) override the global
    // pair, so each portfolio can have its own isolated paper-trading account.
    let alpaca_keys = portfolios::resolve_alpaca_keys(&active_portfolio).or_else(|| {
        match (
            std::env::var("ALPACA_PAPER_KEY")
                .ok()
                .filter(|k| !k.is_empty()),
            std::env::var("ALPACA_PAPER_SECRET")
                .ok()
                .filter(|k| !k.is_empty()),
        ) {
            (Some(key), Some(secret)) => Some((key, secret)),
            _ => None,
        }
    });
    if alpaca_keys.is_none() {
        tracing::warn!("ALPACA_PAPER_KEY/SECRET not set — trading disabled");
    }

    // Quotes: Finnhub primary, Alpaca data API as the guaranteed fallback
    // (PLAN.md §3 — the trading account exists anyway).
    let alpaca_data = alpaca_keys
        .clone()
        .map(|(key, secret)| Arc::new(rekt_data::alpaca_data::AlpacaData::new(key, secret)));
    let market: Option<Arc<dyn MarketData>> = match (&finnhub_token, &alpaca_data) {
        (Some(token), _) => Some(Arc::new(Finnhub::new(token.clone()))),
        (None, Some(alpaca)) => {
            tracing::info!("no FINNHUB_API_KEY — using Alpaca data API for quotes");
            Some(alpaca.clone() as Arc<dyn MarketData>)
        }
        (None, None) => {
            tracing::warn!("no market data keys — quotes disabled");
            None
        }
    };
    // Daily bars are Alpaca-only (Finnhub's free tier dropped candles).
    let bars: Option<Arc<dyn MarketData>> = alpaca_data.map(|alpaca| alpaca as Arc<dyn MarketData>);
    let broker: Option<Arc<dyn rekt_broker::Broker>> = alpaca_keys.clone().map(|(key, secret)| {
        Arc::new(rekt_broker::alpaca::Alpaca::paper(key, secret)) as Arc<dyn rekt_broker::Broker>
    });

    let guardrails = Arc::new(guardrails_from_env());

    // Alert push: full URL wins, else a topic on the public ntfy.sh.
    let notify_url = match std::env::var("REKT_NTFY_URL")
        .ok()
        .filter(|u| !u.is_empty())
    {
        Some(url) => {
            tracing::info!("alert push notifications enabled (ntfy)");
            Some(url)
        }
        None => std::env::var("REKT_NTFY_TOPIC")
            .ok()
            .filter(|t| !t.is_empty())
            .map(|t| {
                tracing::warn!(
                    "REKT_NTFY_TOPIC publishes to the PUBLIC ntfy.sh server — anyone who \
                     guesses the topic name can read your trade alerts. Use a long random \
                     topic, or self-host ntfy and set REKT_NTFY_URL"
                );
                format!("https://ntfy.sh/{t}")
            }),
    };

    // AI analyst (PLAN.md §5): two backends, both advisory only.
    //   default (or REKT_ANALYST_BACKEND=cli) → drive the local Claude Code
    //     CLI (`claude -p`), reusing its auth; no ANTHROPIC_API_KEY needed.
    //     Runs tool-less with injected context (no orders — empty allowlist).
    //   REKT_ANALYST_BACKEND=http (or =api)   → the HTTP API client
    //     (ANTHROPIC_API_KEY), which also produces structured recs.
    // The CLI is the default because REKT is self-hosted alongside Claude
    // Code; opt into the API explicitly when you'd rather spend a key.
    let analyst_cli = match std::env::var("REKT_ANALYST_BACKEND") {
        Ok(v) if v.trim().eq_ignore_ascii_case("http") || v.trim().eq_ignore_ascii_case("api") => {
            false
        }
        _ => true, // default: local Claude Code CLI
    };
    let analyst: Option<Arc<dyn rekt_analyst::Transport>> = if analyst_cli {
        // Probe the binary so a host without Claude Code degrades honestly
        // (disabled) rather than advertising a backend that fails every run.
        let cli = rekt_analyst::CliTransport::new();
        if cli.is_available().await {
            tracing::info!(
                "AI analyst enabled via Claude Code CLI (claude -p; advisory only, no tools)"
            );
            Some(Arc::new(cli) as Arc<dyn rekt_analyst::Transport>)
        } else {
            tracing::warn!(
                "REKT_ANALYST_BACKEND=cli (the default) but `claude` is not runnable on PATH \
                 — AI analyst disabled. Install Claude Code, or set REKT_ANALYST_BACKEND=http \
                 with ANTHROPIC_API_KEY."
            );
            None
        }
    } else {
        let t = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .map(|key| {
                tracing::info!("AI analyst enabled (advisory only — it can never execute orders)");
                Arc::new(rekt_analyst::HttpTransport::new(key)) as Arc<dyn rekt_analyst::Transport>
            });
        if t.is_none() {
            tracing::warn!(
                "REKT_ANALYST_BACKEND=http but no ANTHROPIC_API_KEY — AI analyst disabled"
            );
        }
        t
    };
    let ai_budget = std::env::var("REKT_AI_DAILY_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| rust_decimal::Decimal::new(250, 2)); // $2.50/day

    let state = live::start(move |handles| AppState {
        db,
        market,
        bars,
        finnhub_token,
        broker,
        trading: Arc::new(trading::TradingState::default()),
        guardrails,
        live: handles.live,
        snapshots: handles.snapshots,
        symbols: Arc::new(handles.symbols),
        mutations: Arc::new(Mutex::new(())),
        notify_url,
        backfill_lock: Arc::new(Mutex::new(())),
        analyst,
        analyst_cli,
        ai_budget,
        analyst_running: Arc::new(AtomicBool::new(false)),
        active_portfolio,
        data_dir,
    });

    // Subscribe the stream to currently held symbols from the start.
    live::refresh_symbols(&state).await?;

    // Restore persisted safety flags (pause survives restarts).
    trading::load_persisted(&state).await?;

    // Trading pipeline: trade_updates stream → event apply. Reconciliation
    // runs (a) on every stream (re)connect, and (b) on a periodic timer
    // whose first tick fires immediately — a permanently-blocked websocket
    // can never lock trading out.
    if let Some((key, secret)) = alpaca_keys {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(256);
        tokio::spawn(rekt_broker::stream::run_trade_updates(
            rekt_broker::alpaca::PAPER_API.to_string(),
            key,
            secret,
            events_tx,
        ));
        let event_state = state.clone();
        tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                if let Err(e) = trading::apply_event(&event_state, event).await {
                    tracing::error!(error = %e, "broker event apply failed");
                }
            }
        });
        let recon_state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                tick.tick().await; // first tick is immediate
                tracing::trace!("scheduler tick: reconciliation");
                if let Err(e) = trading::reconcile(&recon_state).await {
                    tracing::error!(error = %e, "periodic reconciliation failed");
                }
            }
        });
        tracing::info!(mode = "paper", "broker configured — reconciling");
    }

    // History pipeline: candle backfill + EOD snapshots (needs the bars
    // provider). First tick is immediate, then every 30 minutes.
    if state.bars.is_some() {
        let hist_state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1800));
            loop {
                tick.tick().await;
                tracing::trace!("scheduler tick: candle backfill + EOD snapshot");
                if let Err(e) = history::backfill_candles(&hist_state).await {
                    tracing::error!(error = %e, "candle backfill failed");
                }
                if let Err(e) = history::maybe_snapshot_eod(&hist_state).await {
                    tracing::error!(error = %e, "EOD snapshot failed");
                }
            }
        });
    }

    // Scheduled analyst runs (REKT_AI_AUTO=0 disables): briefing on NY
    // weekday mornings, deep review on Saturdays — each once per period.
    let ai_auto = !matches!(
        std::env::var("REKT_AI_AUTO").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    );
    if state.analyst.is_some() && ai_auto {
        let ai_state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(900));
            loop {
                tick.tick().await;
                tracing::trace!("scheduler tick: analyst auto-run check");
                if let Err(e) = analyst::maybe_scheduled_runs(&ai_state).await {
                    tracing::error!(error = %e, "scheduled analyst run failed");
                }
            }
        });
    }

    // Alert evaluator: every 15s against the in-memory price cache (price
    // conditions) and cached candles (drawdown). Cheap when nothing is
    // active; triggers persist + notify + surface in the next snapshot.
    {
        let alert_state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            let mut drawdowns = alerts::DrawdownCache::default();
            loop {
                tick.tick().await;
                tracing::trace!("scheduler tick: alert evaluation");
                if let Err(e) = alerts::evaluate_alerts(&alert_state, &mut drawdowns).await {
                    tracing::error!(error = %e, "alert evaluation failed");
                }
            }
        });
    }

    // A portfolio switch re-execs this binary in place; the old listener fd is
    // CLOEXEC and closes on exec, but the new image can momentarily race a
    // socket still in TIME_WAIT. Retry the bind briefly so a switch never
    // crash-loops on a transient "address in use".
    let listener = {
        let mut attempt = 0u32;
        loop {
            match tokio::net::TcpListener::bind(&listen).await {
                Ok(l) => break l,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempt < 20 => {
                    attempt += 1;
                    tracing::warn!(addr = %listen, attempt, "address in use — retrying bind");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
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
            bars: None,
            finnhub_token: None,
            broker: None,
            trading: Arc::new(trading::TradingState::default()),
            guardrails: Arc::new(rekt_core::orders::Guardrails::default()),
            live: Arc::new(live::Live::default()),
            snapshots,
            symbols: Arc::new(symbols),
            mutations: Arc::new(Mutex::new(())),
            notify_url: None,
            backfill_lock: Arc::new(Mutex::new(())),
            analyst: None,
            analyst_cli: false,
            ai_budget: rust_decimal::Decimal::new(250, 2),
            analyst_running: Arc::new(AtomicBool::new(false)),
            active_portfolio: "real".into(),
            data_dir: std::path::PathBuf::from("."),
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
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert!(json["uptime_seconds"].is_u64());
        // Readiness map: the bare test_state wires no optional components.
        assert_eq!(json["components"]["market_data"], "unconfigured");
        assert_eq!(json["components"]["daily_bars"], false);
        assert_eq!(json["components"]["trading_paper"], false);
        assert_eq!(json["components"]["ai_analyst"], false);
        assert_eq!(json["components"]["alert_push"], false);
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
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        // Replay errors name the line in the ORIGINAL file (header = line 1;
        // the oversell is data row 2 = file line 3), like every other path.
        assert!(
            body["error"].as_str().unwrap().starts_with("line 3:"),
            "{body}"
        );

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

        let (_, txs) = request(state.clone(), "GET", "/api/transactions", None).await;
        assert_eq!(txs.as_array().unwrap().len(), 2);

        // CSV imports must be recorded with source='csv', not 'manual'.
        use sqlx::Row;
        let sources: Vec<String> = sqlx::query("SELECT source FROM transactions")
            .fetch_all(&state.db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get("source"))
            .collect();
        assert_eq!(sources, vec!["csv", "csv"]);
    }

    #[tokio::test]
    async fn csv_import_dry_run_validates_without_inserting() {
        let state = test_state().await;
        let csv = "kind,symbol,qty,price,fees,taxes,ts,note\n\
                   deposit,,,2000,,,,seed\n\
                   buy,AAPL,5,100,1,,2026-01-05T15:00:00Z,first buy";

        // Dry run: reports what WOULD import, writes nothing.
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/import/csv?dry_run=true")
            .header("content-type", "text/csv")
            .body(axum::body::Body::from(csv))
            .unwrap();
        let response = app(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["dry_run"], true);
        assert_eq!(body["would_import"], 2);
        assert_eq!(body["sample"].as_array().unwrap().len(), 2);
        assert_eq!(body["sample"][0]["kind"], "deposit");

        let (_, txs) = request(state.clone(), "GET", "/api/transactions", None).await;
        assert_eq!(txs.as_array().unwrap().len(), 0, "dry run must not insert");

        // A dry run that fails validation still inserts nothing AND reports
        // the failing line — same contract as a real import.
        let bad = "kind,symbol,qty,price,fees,taxes,ts,note\n\
                   sell,AAPL,6,110,,,,";
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/import/csv?dry_run=true")
            .header("content-type", "text/csv")
            .body(axum::body::Body::from(bad))
            .unwrap();
        let response = app(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Confirming (no dry_run) then actually writes the rows.
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/import/csv")
            .header("content-type", "text/csv")
            .body(axum::body::Body::from(csv))
            .unwrap();
        let response = app(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (_, txs) = request(state.clone(), "GET", "/api/transactions", None).await;
        assert_eq!(txs.as_array().unwrap().len(), 2);
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

    // ----------------------------------------------------- trading tests --

    struct MockBroker;

    #[async_trait::async_trait]
    impl rekt_broker::Broker for MockBroker {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn mode(&self) -> rekt_broker::TradeMode {
            rekt_broker::TradeMode::Paper
        }
        async fn submit_order(
            &self,
            client_order_id: &str,
            _ticket: &rekt_core::orders::OrderTicket,
        ) -> Result<rekt_broker::BrokerOrder, rekt_broker::BrokerError> {
            Ok(rekt_broker::BrokerOrder {
                broker_order_id: format!("mock-{client_order_id}"),
                client_order_id: client_order_id.to_string(),
                status: rekt_core::orders::OrderStatus::Accepted,
                filled_qty: rust_decimal::Decimal::ZERO,
                avg_fill_price: None,
                updated_ts: None,
                symbol: Some(_ticket.symbol.clone()),
                side: Some(_ticket.side),
                qty: Some(_ticket.qty),
                order_type: Some(_ticket.order_type.as_str().to_string()),
                limit_price: _ticket.limit_price,
                tif: Some(_ticket.tif.as_str().to_string()),
            })
        }
        async fn cancel_order(&self, _id: &str) -> Result<(), rekt_broker::BrokerError> {
            Ok(())
        }
        async fn cancel_all(&self) -> Result<(), rekt_broker::BrokerError> {
            Ok(())
        }
        async fn order_by_client_id(
            &self,
            _id: &str,
        ) -> Result<Option<rekt_broker::BrokerOrder>, rekt_broker::BrokerError> {
            Ok(None)
        }
        async fn list_orders(
            &self,
        ) -> Result<Vec<rekt_broker::BrokerOrder>, rekt_broker::BrokerError> {
            Ok(Vec::new())
        }
        async fn executions_since(
            &self,
            _after: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<Vec<rekt_broker::Execution>, rekt_broker::BrokerError> {
            Ok(Vec::new())
        }
        async fn account(&self) -> Result<rekt_broker::AccountInfo, rekt_broker::BrokerError> {
            Ok(rekt_broker::AccountInfo {
                cash: rust_decimal::Decimal::from(100_000),
                buying_power: rust_decimal::Decimal::from(100_000),
                equity: rust_decimal::Decimal::from(100_000),
                daytrade_count: 0,
            })
        }
    }

    async fn trading_state() -> AppState {
        let mut state = test_state().await;
        state.broker = Some(Arc::new(MockBroker));
        state
            .trading
            .ready
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Seed cash so guardrails have equity to work with.
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/transactions",
            Some(serde_json::json!({ "kind": "deposit", "price": "100000" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        state
    }

    #[tokio::test]
    async fn orders_without_broker_are_503() {
        let (status, json) = request(
            test_state().await,
            "POST",
            "/api/orders",
            Some(serde_json::json!({
                "symbol": "AAPL", "side": "buy", "order_type": "limit",
                "qty": "1", "limit_price": "100"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(json["error"].as_str().unwrap().contains("ALPACA_PAPER_KEY"));
    }

    #[tokio::test]
    async fn orders_locked_until_reconciled() {
        let mut state = test_state().await;
        state.broker = Some(Arc::new(MockBroker));
        // trading.ready deliberately left false
        let (status, json) = request(
            state,
            "POST",
            "/api/orders",
            Some(serde_json::json!({
                "symbol": "AAPL", "side": "buy", "order_type": "limit",
                "qty": "1", "limit_price": "100"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(json["error"].as_str().unwrap().contains("reconciliation"));
    }

    #[tokio::test]
    async fn submit_persists_deterministic_client_id_then_accepts() {
        let state = trading_state().await;
        // Tickets obey the same symbol rule as watchlist/alerts — a
        // malformed symbol fails locally, not as a broker rejection.
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/orders",
            Some(serde_json::json!({
                "symbol": "not a symbol!", "side": "buy", "order_type": "market", "qty": "1"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, json) = request(
            state.clone(),
            "POST",
            "/api/orders",
            Some(serde_json::json!({
                "symbol": "aapl", "side": "buy", "order_type": "limit",
                "qty": "5", "limit_price": "100"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
        let client_order_id = json["client_order_id"].as_str().unwrap();
        assert!(client_order_id.starts_with("rekt-paper-"));
        assert_eq!(json["status"], "accepted");
        assert_eq!(json["est_notional"], "500");

        let (_, orders) = request(state, "GET", "/api/orders", None).await;
        assert_eq!(orders[0]["symbol"], "AAPL");
        assert_eq!(orders[0]["status"], "accepted");
        assert_eq!(
            orders[0]["broker_order_id"].as_str().unwrap(),
            format!("mock-{client_order_id}")
        );
    }

    #[tokio::test]
    async fn guardrails_block_paused_and_oversized_orders() {
        let state = trading_state().await;
        state
            .trading
            .paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let ticket = serde_json::json!({
            "symbol": "AAPL", "side": "buy", "order_type": "limit",
            "qty": "1", "limit_price": "100"
        });
        let (status, json) = request(state.clone(), "POST", "/api/orders", Some(ticket)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(json["error"].as_str().unwrap().contains("paused"));

        state
            .trading
            .paused
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // Default notional cap is 10k; this is 20k.
        let big = serde_json::json!({
            "symbol": "AAPL", "side": "buy", "order_type": "limit",
            "qty": "200", "limit_price": "100"
        });
        let (status, json) = request(state, "POST", "/api/orders", Some(big)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(json["error"].as_str().unwrap().contains("cap"));
    }

    #[tokio::test]
    async fn fills_ingest_idempotently_into_transactions() {
        let state = trading_state().await;
        // Submit an order so the broker_order_id mapping exists.
        let (status, json) = request(
            state.clone(),
            "POST",
            "/api/orders",
            Some(serde_json::json!({
                "symbol": "AAPL", "side": "buy", "order_type": "limit",
                "qty": "5", "limit_price": "100"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let client_order_id = json["client_order_id"].as_str().unwrap().to_string();

        let fill_event = || rekt_broker::stream::BrokerEvent::OrderUpdate {
            order: Box::new(
                serde_json::from_value(serde_json::json!({
                "id": format!("mock-{client_order_id}"),
                "client_order_id": client_order_id,
                "status": "filled",
                "filled_qty": "5",
                "filled_avg_price": "99.50"
                }))
                .unwrap(),
            ),
            event: "fill".into(),
            execution: Some(rekt_broker::stream::StreamExecution {
                execution_id: "exec-same-id".into(),
                qty: "5".parse().unwrap(),
                price: "99.50".parse().unwrap(),
                ts: chrono::Utc::now(),
            }),
        };

        // Apply the same fill twice — stream replays guarantee duplicates.
        trading::apply_event(&state, fill_event()).await.unwrap();
        trading::apply_event(&state, fill_event()).await.unwrap();

        use sqlx::Row;
        let fills: i64 = sqlx::query("SELECT COUNT(*) AS n FROM fills")
            .fetch_one(&state.db)
            .await
            .unwrap()
            .get("n");
        assert_eq!(fills, 1, "duplicate execution must not double-ingest");
        let broker_txs: i64 =
            sqlx::query("SELECT COUNT(*) AS n FROM transactions WHERE source = 'broker_fill'")
                .fetch_one(&state.db)
                .await
                .unwrap()
                .get("n");
        assert_eq!(broker_txs, 1);

        // Mode segregation (PLAN §7): the paper fill is recorded as a
        // paper transaction but must NOT appear in the live portfolio.
        let modes: Vec<String> =
            sqlx::query("SELECT mode FROM transactions WHERE source = 'broker_fill'")
                .fetch_all(&state.db)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.get("mode"))
                .collect();
        assert_eq!(modes, vec!["paper"]);

        let (_, snapshot) = request(state, "GET", "/api/portfolio", None).await;
        // Live portfolio holds only the deposit — no AAPL position.
        assert!(snapshot["portfolio"]["positions"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(snapshot["trading"]["mode"], "paper");
        assert_eq!(snapshot["trading"]["orders"][0]["status"], "filled");
    }

    #[tokio::test]
    async fn stale_status_update_cannot_regress_a_filled_order() {
        let state = trading_state().await;
        let (_, json) = request(
            state.clone(),
            "POST",
            "/api/orders",
            Some(serde_json::json!({
                "symbol": "AAPL", "side": "buy", "order_type": "limit",
                "qty": "5", "limit_price": "100"
            })),
        )
        .await;
        let client_order_id = json["client_order_id"].as_str().unwrap().to_string();

        let order_json = |status: &str| {
            serde_json::json!({
                "id": format!("mock-{client_order_id}"),
                "client_order_id": client_order_id,
                "status": status,
                "filled_qty": if status == "filled" { "5" } else { "0" }
            })
        };

        // Fast fill arrives first…
        trading::apply_event(
            &state,
            rekt_broker::stream::BrokerEvent::OrderUpdate {
                order: Box::new(serde_json::from_value(order_json("filled")).unwrap()),
                event: "fill".into(),
                execution: Some(rekt_broker::stream::StreamExecution {
                    execution_id: "exec-race".into(),
                    qty: "5".parse().unwrap(),
                    price: "100".parse().unwrap(),
                    ts: chrono::Utc::now(),
                }),
            },
        )
        .await
        .unwrap();

        // …then the slower submit-response/duplicate event says "accepted".
        trading::apply_event(
            &state,
            rekt_broker::stream::BrokerEvent::OrderUpdate {
                order: Box::new(serde_json::from_value(order_json("accepted")).unwrap()),
                event: "new".into(),
                execution: None,
            },
        )
        .await
        .unwrap();

        use sqlx::Row;
        let status: String = sqlx::query("SELECT status FROM orders WHERE client_order_id = ?")
            .bind(&client_order_id)
            .fetch_one(&state.db)
            .await
            .unwrap()
            .get("status");
        assert_eq!(status, "filled", "terminal state must never regress");
    }

    #[tokio::test]
    async fn history_endpoint_builds_equity_curve_with_benchmark() {
        let state = test_state().await;
        // Empty log → empty curve with an explanation.
        let (status, json) = request(state.clone(), "GET", "/api/history?range=all", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["points"].as_array().unwrap().is_empty());

        // Deposit + buy on day 1; candles for the symbol and SPY.
        for body in [
            serde_json::json!({"kind":"deposit","price":"1000","ts":"2026-06-01T15:00:00Z"}),
            serde_json::json!({"kind":"buy","symbol":"AAPL","qty":"10","price":"100","ts":"2026-06-01T15:30:00Z"}),
        ] {
            let (status, _) = request(state.clone(), "POST", "/api/transactions", Some(body)).await;
            assert_eq!(status, StatusCode::CREATED);
        }
        let candle = |date: &str, close: &str| rekt_core::Candle {
            date: date.parse().unwrap(),
            open: close.parse().unwrap(),
            high: close.parse().unwrap(),
            low: close.parse().unwrap(),
            close: close.parse().unwrap(),
            volume: 1,
        };
        repo::upsert_candles(
            &state.db,
            "AAPL",
            &[candle("2026-06-01", "100"), candle("2026-06-02", "120")],
        )
        .await
        .unwrap();
        repo::upsert_candles(
            &state.db,
            "SPY",
            &[candle("2026-06-01", "500"), candle("2026-06-02", "505")],
        )
        .await
        .unwrap();

        let (status, json) = request(state, "GET", "/api/history?range=all", None).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let points = json["points"].as_array().unwrap();
        assert!(points.len() >= 2);
        // Day 1: 10×100 = 1000 equity (cash spent); benchmark = 1000 in SPY.
        assert_eq!(points[0]["equity"], "1000");
        assert_eq!(points[0]["benchmark"], "1000");
        // Latest: AAPL at 120 → equity 1200; SPY 505/500 → benchmark 1010.
        let last = points.last().unwrap();
        assert_eq!(last["equity"], "1200");
        assert_eq!(last["benchmark"], "1010");
        // Metrics: TWR 20%, benchmark 1%.
        assert_eq!(json["metrics"]["twr_pct"], "20.00");
        assert_eq!(json["metrics"]["benchmark_twr_pct"], "1.00");
        assert_eq!(json["totals"]["deposited"], "1000");
    }

    #[tokio::test]
    async fn snapshot_carries_tx_revision_that_bumps_on_mutation() {
        let state = test_state().await;
        let (_, before) = request(state.clone(), "GET", "/api/portfolio", None).await;
        let rev_before = before["tx_revision"].as_u64().unwrap();

        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/transactions",
            Some(serde_json::json!({ "kind": "deposit", "price": "100" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (_, after) = request(state, "GET", "/api/portfolio", None).await;
        assert!(after["tx_revision"].as_u64().unwrap() > rev_before);
    }

    #[tokio::test]
    async fn alert_lifecycle_trigger_dismiss_rearm() {
        let state = test_state().await;

        // Bad drafts fail at creation, not at 3am.
        let (status, body) = request(
            state.clone(),
            "POST",
            "/api/alerts",
            Some(serde_json::json!({
                "symbol": "AAPL", "condition": "price_at", "threshold": "170"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/alerts",
            Some(serde_json::json!({
                "symbol": "AAPL", "condition": "price_below", "threshold": "170",
                "draft_order": {"side": "buy", "order_type": "market", "qty": "5", "limit_price": "168"}
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // A valid alert with a pre-staged limit buy.
        let (status, created) = request(
            state.clone(),
            "POST",
            "/api/alerts",
            Some(serde_json::json!({
                "symbol": "aapl", "condition": "price_below", "threshold": "170",
                "draft_order": {"side": "buy", "order_type": "limit", "qty": "5", "limit_price": "168"},
                "note": "dip buy"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let id = created["id"].as_i64().unwrap();
        // No price data yet → the arm-time check couldn't run; the response
        // says so instead of implying it passed.
        assert!(created["note"].as_str().unwrap().contains("no market data"));

        // Bad symbols are rejected like the watchlist path.
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/alerts",
            Some(serde_json::json!({
                "symbol": "not a symbol!", "condition": "price_below", "threshold": "170"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // No price data yet → evaluation leaves it armed.
        let mut drawdowns = alerts::DrawdownCache::default();
        alerts::evaluate_alerts(&state, &mut drawdowns)
            .await
            .unwrap();
        let (_, list) = request(state.clone(), "GET", "/api/alerts", None).await;
        assert_eq!(list[0]["status"], "active");
        assert_eq!(list[0]["symbol"], "AAPL");

        // Price above threshold → still armed; at/below → triggered once.
        state
            .live
            .set_price("AAPL", rust_decimal::Decimal::from(171))
            .await;
        alerts::evaluate_alerts(&state, &mut drawdowns)
            .await
            .unwrap();
        let (_, list) = request(state.clone(), "GET", "/api/alerts", None).await;
        assert_eq!(list[0]["status"], "active");

        state.live.set_price("AAPL", "169.5".parse().unwrap()).await;
        alerts::evaluate_alerts(&state, &mut drawdowns)
            .await
            .unwrap();
        alerts::evaluate_alerts(&state, &mut drawdowns)
            .await
            .unwrap(); // idempotent
        let (_, list) = request(state.clone(), "GET", "/api/alerts", None).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["status"], "triggered");
        assert_eq!(list[0]["triggered_value"], "169.5");
        assert_eq!(list[0]["draft_order"]["side"], "buy");
        assert_eq!(list[0]["draft_order"]["qty"], "5");

        // The snapshot payload carries the alert for the dashboard banner.
        let (_, snapshot) = request(state.clone(), "GET", "/api/portfolio", None).await;
        assert_eq!(snapshot["alerts"][0]["status"], "triggered");

        // dismiss: only valid from triggered.
        let (status, _) = request(
            state.clone(),
            "POST",
            &format!("/api/alerts/{id}/dismiss"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = request(
            state.clone(),
            "POST",
            &format!("/api/alerts/{id}/dismiss"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // rearm while the condition is STILL true (price 169.5 ≤ 170) is
        // refused — it would just re-fire on the next tick.
        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/alerts/{id}/rearm"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["error"].as_str().unwrap().contains("still true"));

        // Once the condition resets, rearm clears the trigger record.
        state
            .live
            .set_price("AAPL", rust_decimal::Decimal::from(175))
            .await;
        let (status, _) = request(
            state.clone(),
            "POST",
            &format!("/api/alerts/{id}/rearm"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, list) = request(state.clone(), "GET", "/api/alerts", None).await;
        assert_eq!(list[0]["status"], "active");
        assert!(list[0]["triggered_value"].is_null());

        let (status, _) =
            request(state.clone(), "DELETE", &format!("/api/alerts/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, list) = request(state, "GET", "/api/alerts", None).await;
        assert!(list.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn drawdown_alert_uses_cached_candles() {
        let state = test_state().await;
        let candle = |date: &str, close: &str| rekt_core::Candle {
            date: date.parse().unwrap(),
            open: close.parse().unwrap(),
            high: close.parse().unwrap(),
            low: close.parse().unwrap(),
            close: close.parse().unwrap(),
            volume: 1,
        };
        // Peak 200 → 150 = 25% drawdown.
        repo::upsert_candles(
            &state.db,
            "TSLA",
            &[
                candle("2026-06-01", "200"),
                candle("2026-06-02", "180"),
                candle("2026-06-03", "150"),
            ],
        )
        .await
        .unwrap();

        // The condition is ALREADY true (25% ≥ 20%) → arming would fire
        // instantly, so creation is rejected with an explanation…
        let (status, body) = request(
            state.clone(),
            "POST",
            "/api/alerts",
            Some(serde_json::json!({
                "symbol": "TSLA", "condition": "drawdown_above", "threshold": "20"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body["error"].as_str().unwrap().contains("already true"));

        // …unless the user explicitly opts into an immediate fire.
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/alerts",
            Some(serde_json::json!({
                "symbol": "TSLA", "condition": "drawdown_above", "threshold": "20",
                "allow_immediate": true
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let mut drawdowns = alerts::DrawdownCache::default();
        alerts::evaluate_alerts(&state, &mut drawdowns)
            .await
            .unwrap();
        let (_, list) = request(state, "GET", "/api/alerts", None).await;
        assert_eq!(list[0]["status"], "triggered");
        assert_eq!(list[0]["triggered_value"], "25.00");
    }

    #[tokio::test]
    async fn watchlist_add_appears_in_snapshot_then_removes() {
        let state = test_state().await;

        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/watchlist",
            Some(serde_json::json!({ "symbol": "vti" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        // Re-adding is fine (200, not duplicate).
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/watchlist",
            Some(serde_json::json!({ "symbol": "VTI" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = request(
            state.clone(),
            "POST",
            "/api/watchlist",
            Some(serde_json::json!({ "symbol": "not a symbol!" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (_, list) = request(state.clone(), "GET", "/api/watchlist", None).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0], "VTI");

        // The live snapshot carries the watchlist block (price unknown here).
        let (_, snapshot) = request(state.clone(), "GET", "/api/portfolio", None).await;
        assert_eq!(snapshot["watchlist"][0]["symbol"], "VTI");
        assert!(snapshot["watchlist"][0]["price"].is_null());

        let (status, _) = request(state.clone(), "DELETE", "/api/watchlist/VTI", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = request(state.clone(), "DELETE", "/api/watchlist/VTI", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, snapshot) = request(state, "GET", "/api/portfolio", None).await;
        assert!(snapshot["watchlist"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn broker_preset_csv_imports_and_reports_skips() {
        let state = test_state().await;
        let schwab = "\
\"Date\",\"Action\",\"Symbol\",\"Description\",\"Quantity\",\"Price\",\"Fees & Comm\",\"Amount\"\n\
\"06/09/2026\",\"MoneyLink Transfer\",\"\",\"Tfr FROM CHECKING\",\"\",\"\",\"\",\"$10,000.00\"\n\
\"06/10/2026\",\"Buy\",\"VOO\",\"VANGUARD S&P 500\",\"5\",\"$520.00\",\"$0.03\",\"-$2,600.03\"\n\
\"06/11/2026\",\"Bank Interest\",\"\",\"INTEREST\",\"\",\"\",\"\",\"$1.23\"\n";
        let request_body = axum::http::Request::builder()
            .method("POST")
            .uri("/api/import/csv?format=schwab")
            .header("content-type", "text/csv")
            .body(axum::body::Body::from(schwab))
            .unwrap();
        let response = app(state.clone()).oneshot(request_body).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["imported"], 2);
        assert!(json["skipped"][0]
            .as_str()
            .unwrap()
            .contains("Bank Interest"));

        let (_, txs) = request(state.clone(), "GET", "/api/transactions", None).await;
        let txs = txs.as_array().unwrap();
        assert_eq!(txs.len(), 2);
        // Newest first: the buy, then the deposit.
        assert_eq!(txs[0]["kind"], "buy");
        assert_eq!(txs[0]["symbol"], "VOO");
        assert_eq!(txs[0]["fees"], "0.03");
        assert_eq!(txs[1]["kind"], "deposit");
        assert_eq!(txs[1]["price"], "10000.00");

        // An unknown preset is rejected with guidance.
        let request_body = axum::http::Request::builder()
            .method("POST")
            .uri("/api/import/csv?format=etrade")
            .header("content-type", "text/csv")
            .body(axum::body::Body::from("x"))
            .unwrap();
        let response = app(state).oneshot(request_body).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn ibkr_activity_statement_imports_through_the_endpoint() {
        let state = test_state().await;
        // A minimal Activity Statement: a funding deposit, a stock buy, an
        // option trade (skipped), and the section subtotals IBKR emits.
        let ibkr = "\
Statement,Header,Field Name,Field Value\n\
Statement,Data,BrokerName,Interactive Brokers LLC\n\
Deposits & Withdrawals,Header,Currency,Settle Date,Description,Amount\n\
Deposits & Withdrawals,Data,USD,2026-06-08,Electronic Fund Transfer,5000\n\
Deposits & Withdrawals,Data,Total,,,5000\n\
Trades,Header,DataDiscriminator,Asset Category,Currency,Symbol,Date/Time,Quantity,T. Price,Comm/Fee\n\
Trades,Data,Order,Stocks,USD,AAPL,\"2026-06-10, 10:30:00\",10,150.25,-1\n\
Trades,Data,Order,Equity and Index Options,USD,AAPL 240920C,\"2026-06-10, 10:30:00\",1,2.50,-1\n";
        let request_body = axum::http::Request::builder()
            .method("POST")
            .uri("/api/import/csv?format=ibkr")
            .header("content-type", "text/csv")
            .body(axum::body::Body::from(ibkr))
            .unwrap();
        let response = app(state.clone()).oneshot(request_body).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["imported"], 2);
        assert!(json["skipped"][0].as_str().unwrap().contains("Options"));

        // The imported rows replay cleanly through the portfolio engine.
        let (status, portfolio) = request(state.clone(), "GET", "/api/portfolio", None).await;
        assert_eq!(status, StatusCode::OK);
        let p = &portfolio["portfolio"];
        // 5000 deposited − (10 × 150.25 + 1 fee) = 3496.50 cash.
        assert_eq!(p["cash"], "3496.50");
        assert_eq!(p["positions"][0]["symbol"], "AAPL");
        assert_eq!(p["positions"][0]["qty"], "10");
    }

    /// Scripted Claude transport: returns canned responses in order.
    struct MockClaude {
        responses: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl rekt_analyst::Transport for MockClaude {
        async fn send(
            &self,
            _request: &serde_json::Value,
        ) -> Result<rekt_analyst::MessageResponse, rekt_analyst::AnalystError> {
            let next =
                self.responses.lock().unwrap().pop().ok_or_else(|| {
                    rekt_analyst::AnalystError::BadResponse("mock exhausted".into())
                })?;
            Ok(serde_json::from_value(next).unwrap())
        }
    }

    async fn wait_for_analysis(state: &AppState, id: i64) -> repo::AnalysisRecord {
        for _ in 0..100 {
            let analysis = repo::get_analysis(&state.db, id).await.unwrap().unwrap();
            if analysis.status != "running" {
                return analysis;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("analysis {id} never finished");
    }

    #[tokio::test]
    async fn analyst_disabled_without_key_and_budget_gates() {
        let state = test_state().await;
        // Honest 503 without a key.
        let (status, body) = request(state.clone(), "POST", "/api/analyst/briefing", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let (_, summary) = request(state.clone(), "GET", "/api/analyst", None).await;
        assert_eq!(summary["enabled"], false);

        // With a key but an exhausted budget → 429 and NO run started.
        let mut state = state;
        state.analyst = Some(Arc::new(MockClaude {
            responses: std::sync::Mutex::new(vec![]),
        }));
        state.ai_budget = rust_decimal::Decimal::ZERO;
        let (status, body) = request(state.clone(), "POST", "/api/analyst/briefing", None).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert!(body["error"].as_str().unwrap().contains("budget"));
    }

    #[tokio::test]
    async fn weekly_review_persists_report_cost_and_recommendations() {
        let mut state = test_state().await;
        let review_json = serde_json::json!({
            "report_md": "## Review\nAll holdings fine.",
            "recommendations": [
                {"symbol": "aapl", "action": "trim", "sizing": "1% of equity",
                 "rationale": "Concentration is high", "confidence": "medium"},
                {"symbol": "not a symbol!", "action": "buy", "sizing": "",
                 "rationale": "should be skipped", "confidence": "low"}
            ]
        });
        state.analyst = Some(Arc::new(MockClaude {
            responses: std::sync::Mutex::new(vec![serde_json::json!({
                "content": [{"type": "text", "text": review_json.to_string()}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1000, "output_tokens": 500,
                          "cache_creation_input_tokens": 2000, "cache_read_input_tokens": 0}
            })]),
        }));

        let (status, body) = request(state.clone(), "POST", "/api/analyst/review", None).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_i64().unwrap();

        let analysis = wait_for_analysis(&state, id).await;
        assert_eq!(analysis.status, "done", "{:?}", analysis.error);
        assert_eq!(
            analysis.report_md.as_deref(),
            Some("## Review\nAll holdings fine.")
        );
        assert_eq!(analysis.input_tokens, 1000);
        assert_eq!(analysis.cache_write_tokens, 2000);
        // Opus 4.8: 1000×$5 + 2000×$6.25 + 500×$25 per MTok > 0.
        assert!(analysis.cost_usd > rust_decimal::Decimal::ZERO);

        // The valid recommendation persisted; the invalid symbol was skipped.
        let (_, summary) = request(state.clone(), "GET", "/api/analyst", None).await;
        // Poll payload stays light: list rows carry no report body; the
        // full report rides only on "latest".
        assert!(summary["analyses"][0]["report_md"].is_null());
        assert_eq!(
            summary["latest"]["report_md"].as_str().unwrap(),
            "## Review\nAll holdings fine."
        );
        let recs = summary["recommendations"].as_array().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["symbol"], "AAPL");
        assert_eq!(recs[0]["action"], "trim");
        assert_eq!(recs[0]["status"], "open");
        assert_eq!(summary["running"], false);
        assert!(
            summary["today_cost_usd"].as_str().is_some() || summary["today_cost_usd"].is_number()
        );

        // accept is one-shot; a second accept (or dismiss-after) conflicts.
        let rec_id = recs[0]["id"].as_i64().unwrap();
        let (status, _) = request(
            state.clone(),
            "POST",
            &format!("/api/recommendations/{rec_id}/accept"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = request(
            state.clone(),
            "POST",
            &format!("/api/recommendations/{rec_id}/dismiss"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn analyst_failures_are_recorded_and_release_the_latch() {
        let mut state = test_state().await;
        state.analyst = Some(Arc::new(MockClaude {
            responses: std::sync::Mutex::new(vec![serde_json::json!({
                "content": [],
                "stop_reason": "refusal",
                "usage": {"input_tokens": 1, "output_tokens": 0}
            })]),
        }));

        let (status, body) = request(state.clone(), "POST", "/api/analyst/briefing", None).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_i64().unwrap();
        let analysis = wait_for_analysis(&state, id).await;
        assert_eq!(analysis.status, "error");
        assert!(analysis.error.unwrap().contains("declined"));
        // The tokens the refused call billed are still accounted.
        assert_eq!(analysis.input_tokens, 1);
        assert!(analysis.cost_usd > rust_decimal::Decimal::ZERO);

        // The single-flight latch released — a new run can start.
        assert!(!state
            .analyst_running
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn lapsed_recommendations_present_as_expired_and_refuse_transitions() {
        let state = test_state().await;
        let analysis_id = repo::insert_analysis(&state.db, "weekly_review", "m", None)
            .await
            .unwrap();
        let rec_id = repo::insert_recommendation(
            &state.db,
            repo::NewRecommendation {
                analysis_id,
                symbol: "AAPL",
                action: "buy",
                sizing: "",
                rationale: "was timely a week ago",
                confidence: "low",
                expires_ts: chrono::Utc::now() - chrono::Duration::hours(1),
            },
        )
        .await
        .unwrap();

        // Read-time computed status: presents as expired without any write.
        let recs = repo::list_recommendations(&state.db, 10).await.unwrap();
        assert_eq!(recs[0].status, "expired");

        // And the transition guard refuses to act on it.
        let (status, _) = request(
            state,
            "POST",
            &format!("/api/recommendations/{rec_id}/accept"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn recommendation_outcomes_surface_in_the_summary() {
        let state = test_state().await;
        let analysis_id = repo::insert_analysis(&state.db, "weekly_review", "m", None)
            .await
            .unwrap();
        let rec_id = repo::insert_recommendation(
            &state.db,
            repo::NewRecommendation {
                analysis_id,
                symbol: "NVDA",
                action: "buy",
                sizing: "1 share",
                rationale: "up and to the right",
                confidence: "high",
                expires_ts: chrono::Utc::now() + chrono::Duration::days(7),
            },
        )
        .await
        .unwrap();
        // Closes: baseline on the recommendation's NY date at 100, then a
        // later close at 110 → +10%, favorable for a buy call. The base
        // date comes from the STORED row, not a second Utc::now() — the
        // two could land on opposite sides of NY midnight.
        let stored = repo::list_recommendations(&state.db, 1).await.unwrap();
        let created = chrono::DateTime::parse_from_rfc3339(&stored[0].created_ts).unwrap();
        let base = rekt_core::taxes::ny_date(created.to_utc());
        let dec = |s: &str| s.parse::<rust_decimal::Decimal>().unwrap();
        let candle = |date, close: &str| rekt_core::Candle {
            date,
            open: dec(close),
            high: dec(close),
            low: dec(close),
            close: dec(close),
            volume: 0,
        };
        repo::upsert_candles(
            &state.db,
            "NVDA",
            &[
                candle(base, "100"),
                candle(base + chrono::Duration::days(1), "110"),
            ],
        )
        .await
        .unwrap();

        let (status, json) = request(state, "GET", "/api/analyst", None).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let rec = &json["recommendations"][0];
        assert_eq!(rec["id"].as_i64().unwrap(), rec_id);
        let outcome = &rec["outcome"];
        assert_eq!(dec(outcome["return_pct"].as_str().unwrap()), dec("10"));
        assert_eq!(outcome["favorable"], true);
        assert_eq!(json["track_record"]["favorable"], 1);
        assert_eq!(json["track_record"]["tested"], 1);
    }

    #[tokio::test]
    async fn tax_report_flags_wash_sales_and_exports_csv() {
        let state = test_state().await;
        for body in [
            serde_json::json!({"kind": "buy", "symbol": "TSLA", "qty": "10", "price": "300",
                "ts": "2026-01-05T15:00:00Z"}),
            serde_json::json!({"kind": "sell", "symbol": "TSLA", "qty": "10", "price": "250",
                "ts": "2026-03-02T15:00:00Z"}),
            serde_json::json!({"kind": "buy", "symbol": "TSLA", "qty": "10", "price": "240",
                "ts": "2026-03-20T15:00:00Z"}),
            // Exit the replacement lot at its purchase price: the deferred
            // loss must re-emerge through the carried basis.
            serde_json::json!({"kind": "sell", "symbol": "TSLA", "qty": "10", "price": "240",
                "ts": "2026-12-01T15:00:00Z"}),
        ] {
            let (status, _) = request(state.clone(), "POST", "/api/transactions", Some(body)).await;
            assert_eq!(status, StatusCode::CREATED);
        }

        let (status, json) = request(state.clone(), "GET", "/api/taxes?year=2026", None).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["year"], 2026);
        assert_eq!(json["years"][0], 2026);
        let row = &json["rows"][0];
        assert_eq!(row["code"], "W");
        assert_eq!(row["disallowed"], "500");
        assert_eq!(row["gain"], "-500");
        assert_eq!(row["long_term"], false);
        // The replacement lot carries the deferred loss: basis 2400 + 500,
        // acquired tacked back by the 56 days the original lot was held.
        let row = &json["rows"][1];
        assert_eq!(row["basis"], "2900");
        assert_eq!(row["gain"], "-500");
        assert_eq!(row["acquired"], "2026-01-23");
        // Schedule D: deferred, not lost — the year nets to the economic
        // truth (bought 5400, sold 4900).
        assert_eq!(json["short"]["reportable"], "-500");

        // The CSV export carries the same row, Form 8949-shaped.
        let response = app(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/taxes/csv?year=2026")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/csv"));
        assert!(response
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("rekt-form8949-2026.csv"));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let csv = std::str::from_utf8(&body).unwrap();
        assert!(csv.starts_with("Description,Date Acquired,Date Sold,"));
        assert!(
            csv.contains("10 sh TSLA,01/05/2026,03/02/2026,2500,3000,W,500,0,short"),
            "{csv}"
        );
        // The replacement lot's row: tacked acquired date, carried basis.
        assert!(
            csv.contains("10 sh TSLA,01/23/2026,12/01/2026,2400,2900,,0,-500,short"),
            "{csv}"
        );
    }

    #[tokio::test]
    async fn empty_tax_year_reports_honestly() {
        let (status, json) = request(test_state().await, "GET", "/api/taxes", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["rows"].as_array().unwrap().is_empty());
        assert_eq!(json["short"]["reportable"], "0");
        // With no transactions, the only offered year is the current one.
        assert_eq!(json["years"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn candles_endpoint_serves_ohlcv_oldest_first() {
        let state = test_state().await;
        let dec = |s: &str| s.parse::<rust_decimal::Decimal>().unwrap();
        let candle = |y, mo, d, o: &str, h: &str, l: &str, c: &str, v| rekt_core::Candle {
            date: chrono::NaiveDate::from_ymd_opt(y, mo, d).unwrap(),
            open: dec(o),
            high: dec(h),
            low: dec(l),
            close: dec(c),
            volume: v,
        };
        repo::upsert_candles(
            &state.db,
            "AAPL",
            &[
                candle(2026, 6, 10, "150", "152", "149", "151", 1000),
                candle(2026, 6, 11, "151", "155", "151", "154", 1200),
            ],
        )
        .await
        .unwrap();

        let (status, json) = request(state.clone(), "GET", "/api/candles?symbol=aapl", None).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["symbol"], "AAPL");
        assert_eq!(json["timeframe"], "daily");
        let rows = json["candles"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // Oldest first; compact o/h/l/c/v keys.
        assert_eq!(rows[0]["date"], "2026-06-10");
        assert_eq!(rows[0]["o"], "150");
        assert_eq!(rows[0]["c"], "151");
        assert_eq!(rows[1]["c"], "154");
        assert_eq!(rows[1]["v"], 1200);

        // A symbol with no stored candles is an empty (not error) series.
        let (status, json) = request(state, "GET", "/api/candles?symbol=ZZZZ", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["candles"].as_array().unwrap().is_empty());
    }

    /// Post a transaction and assert it was created. The end-to-end tests
    /// below build a realistic log through the real router, then check that
    /// independent subsystems derive a consistent view of it.
    async fn post_tx(state: &AppState, body: serde_json::Value) {
        let (status, json) = request(state.clone(), "POST", "/api/transactions", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
    }

    #[tokio::test]
    async fn portfolio_taxes_and_history_agree_on_realized_pnl() {
        // The portfolio engine and the (independent) tax engine replay the
        // same log; with no wash sales and no fees their realized number is
        // the SAME 4000, and history's totals must match too. This is the
        // cross-subsystem invariant no single-module test can catch.
        let state = test_state().await;
        for body in [
            serde_json::json!({"kind": "deposit", "price": "50000",
                "ts": "2025-01-01T15:00:00Z"}),
            serde_json::json!({"kind": "buy", "symbol": "AAPL", "qty": "100", "price": "150",
                "ts": "2025-02-02T15:00:00Z"}),
            serde_json::json!({"kind": "buy", "symbol": "AAPL", "qty": "50", "price": "160",
                "ts": "2025-09-01T15:00:00Z"}),
            // FIFO sells 80 from the first lot — held >1yr, so long-term.
            serde_json::json!({"kind": "sell", "symbol": "AAPL", "qty": "80", "price": "200",
                "ts": "2026-03-02T15:00:00Z"}),
            serde_json::json!({"kind": "dividend", "symbol": "AAPL", "price": "120",
                "ts": "2026-04-01T15:00:00Z"}),
        ] {
            post_tx(&state, body).await;
        }

        let (_, portfolio) = request(state.clone(), "GET", "/api/portfolio", None).await;
        let p = &portfolio["portfolio"];
        // 50000 − 15000 − 8000 + 16000 + 120.
        assert_eq!(p["cash"], "43120");
        assert_eq!(p["positions"][0]["qty"], "70"); // 100 + 50 − 80
        assert_eq!(p["realized_pnl"], "4000"); // 80 × (200 − 150)
        assert_eq!(p["dividends"], "120");

        let (_, taxes) = request(state.clone(), "GET", "/api/taxes?year=2026", None).await;
        assert_eq!(taxes["rows"].as_array().unwrap().len(), 1);
        assert_eq!(taxes["rows"][0]["long_term"], true);
        assert_eq!(taxes["long"]["gain"], "4000");
        assert_eq!(taxes["short"]["gain"], "0");
        assert_eq!(taxes["long"]["disallowed"], "0"); // no rebuy → no wash
        assert_eq!(taxes["long"]["reportable"], "4000"); // Schedule D line: gain + disallowed

        let (_, history) = request(state.clone(), "GET", "/api/history?range=all", None).await;
        // Pin history's totals to the expected values directly, so a missing
        // key fails cleanly (not via a dec() panic) and an in-lockstep drift
        // with the portfolio engine can't hide behind the comparison below.
        assert_eq!(history["totals"]["realized_pnl"], "4000");
        assert_eq!(history["totals"]["dividends"], "120");

        // The invariant the test exists for: economic realized P&L
        // (portfolio) == capital gain (taxes, no wash) == history totals —
        // 4000 derived from one log by three independent paths.
        let dec = |v: &serde_json::Value| {
            v.as_str()
                .unwrap()
                .parse::<rust_decimal::Decimal>()
                .unwrap()
        };
        let realized = dec(&p["realized_pnl"]);
        assert_eq!(
            realized,
            dec(&taxes["short"]["gain"]) + dec(&taxes["long"]["gain"])
        );
        assert_eq!(realized, dec(&history["totals"]["realized_pnl"]));
        assert_eq!(dec(&history["totals"]["dividends"]), dec(&p["dividends"]));
    }

    #[tokio::test]
    async fn a_wash_sale_makes_economic_and_taxable_pnl_diverge() {
        // The two engines must NOT always agree: a wash sale means the
        // portfolio still shows the economic loss while the tax engine
        // defers it (§1091). The divergence is exactly the disallowed amount.
        let state = test_state().await;
        for body in [
            serde_json::json!({"kind": "deposit", "price": "10000",
                "ts": "2026-01-01T15:00:00Z"}),
            serde_json::json!({"kind": "buy", "symbol": "TSLA", "qty": "10", "price": "300",
                "ts": "2026-01-05T15:00:00Z"}),
            serde_json::json!({"kind": "sell", "symbol": "TSLA", "qty": "10", "price": "250",
                "ts": "2026-03-02T15:00:00Z"}),
            // Rebuy within 30 days → the loss is washed.
            serde_json::json!({"kind": "buy", "symbol": "TSLA", "qty": "10", "price": "240",
                "ts": "2026-03-20T15:00:00Z"}),
        ] {
            post_tx(&state, body).await;
        }

        let (_, portfolio) = request(state.clone(), "GET", "/api/portfolio", None).await;
        // The portfolio tracks the real economic loss, untouched by §1091.
        assert_eq!(portfolio["portfolio"]["realized_pnl"], "-500");

        let (_, taxes) = request(state.clone(), "GET", "/api/taxes?year=2026", None).await;
        assert_eq!(taxes["rows"][0]["code"], "W");
        assert_eq!(taxes["short"]["disallowed"], "500"); // fully disallowed
        assert_eq!(taxes["short"]["reportable"], "0"); // so nothing lands this year

        // The genuinely independent invariant (NOT the tautology
        // reportable == gain + disallowed, which holds by TermTotals'
        // definition): the portfolio's economic loss and the tax engine's
        // raw pre-adjustment gain are two separate replays that must agree
        // on -500 — while reportable (0) is what diverges via the wash.
        assert_eq!(
            portfolio["portfolio"]["realized_pnl"],
            taxes["short"]["gain"]
        );
        assert_eq!(taxes["short"]["gain"], "-500");
    }

    #[tokio::test]
    async fn generic_csv_import_flows_into_portfolio_and_taxes() {
        // Exercise the import → validate → insert → derive seam end to end:
        // a CSV the user uploads must feed both the portfolio and the tax
        // report consistently.
        let state = test_state().await;
        let csv = "kind,symbol,qty,price,fees,taxes,ts,note\n\
                   deposit,,0,10000,,,2025-01-01T15:00:00Z,\n\
                   buy,MSFT,10,400,,,2025-02-02T15:00:00Z,\n\
                   sell,MSFT,10,500,,,2026-03-02T15:00:00Z,\n";
        let response = app(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/import/csv?format=generic")
                    .header("content-type", "text/csv")
                    .body(axum::body::Body::from(csv))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let imported: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(imported["imported"], 3);

        let (_, portfolio) = request(state.clone(), "GET", "/api/portfolio", None).await;
        let p = &portfolio["portfolio"];
        assert_eq!(p["cash"], "11000"); // 10000 − 4000 + 5000
        assert_eq!(p["realized_pnl"], "1000"); // 10 × (500 − 400)
        assert!(p["positions"].as_array().unwrap().is_empty()); // MSFT flat

        let (_, taxes) = request(state.clone(), "GET", "/api/taxes?year=2026", None).await;
        assert_eq!(taxes["rows"].as_array().unwrap().len(), 1);
        assert_eq!(taxes["rows"][0]["long_term"], true); // held >1yr
                                                         // These two standalone pins are the real coverage: both engines,
                                                         // fed from one uploaded CSV, independently arrive at 1000.
        assert_eq!(p["realized_pnl"], "1000"); // portfolio (10 × (500 − 400))
        assert_eq!(taxes["long"]["gain"], "1000"); // tax engine
    }
}
