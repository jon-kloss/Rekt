//! SQLite repository. Money travels as TEXT decimal strings (PLAN.md §7);
//! parsing failures are data corruption and surface as errors, never zeros.

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use rekt_core::history::Closes;
use rekt_core::portfolio::{Tx, TxKind};
use rekt_core::Candle;
use rust_decimal::Decimal;
use sqlx::{Row, SqliteConnection, SqlitePool};

pub fn parse_dec(value: &str, what: &str) -> Result<Decimal> {
    value
        .parse()
        .with_context(|| format!("corrupt decimal in column {what}: {value:?}"))
}

/// Where a transaction came from. Matches the schema CHECK constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxSource {
    Manual,
    Csv,
    /// Phase 2: ingested from a broker fill (carries a fill_id).
    #[allow(dead_code)]
    BrokerFill,
}

impl TxSource {
    fn as_str(&self) -> &'static str {
        match self {
            TxSource::Manual => "manual",
            TxSource::Csv => "csv",
            TxSource::BrokerFill => "broker_fill",
        }
    }
}

/// A transaction plus its storage metadata (mode) for listing.
#[derive(Debug, serde::Serialize)]
pub struct TxRecord {
    #[serde(flatten)]
    pub tx: Tx,
    pub mode: String,
}

/// Transactions in replay order (ts, then id), optionally filtered to one
/// mode. The portfolio engine must NEVER mix modes (paper fills must not
/// pollute the real equity curve — PLAN.md §7).
pub async fn fetch_txs(pool: &SqlitePool, mode: Option<&str>) -> Result<Vec<TxRecord>> {
    let base = r#"SELECT t.id, t.kind, i.symbol, t.qty, t.price, t.fees, t.taxes, t.ts, t.note, t.mode
           FROM transactions t
           LEFT JOIN instruments i ON i.id = t.instrument_id"#;
    let query = match mode {
        Some(_) => format!("{base} WHERE t.mode = ? ORDER BY t.ts, t.id"),
        None => format!("{base} ORDER BY t.ts, t.id"),
    };
    let mut q = sqlx::query(&query);
    if let Some(mode) = mode {
        q = q.bind(mode);
    }
    let rows = q.fetch_all(pool).await?;

    rows.into_iter()
        .map(|row| {
            let kind: String = row.get("kind");
            let ts: String = row.get("ts");
            Ok(TxRecord {
                tx: Tx {
                    id: row.get("id"),
                    kind: kind.parse::<TxKind>().map_err(anyhow::Error::msg)?,
                    symbol: row.get("symbol"),
                    qty: parse_dec(&row.get::<String, _>("qty"), "qty")?,
                    price: parse_dec(&row.get::<String, _>("price"), "price")?,
                    fees: parse_dec(&row.get::<String, _>("fees"), "fees")?,
                    taxes: parse_dec(&row.get::<String, _>("taxes"), "taxes")?,
                    ts: DateTime::parse_from_rfc3339(&ts)
                        .with_context(|| format!("corrupt timestamp: {ts:?}"))?
                        .with_timezone(&Utc),
                    note: row.get("note"),
                },
                mode: row.get("mode"),
            })
        })
        .collect()
}

/// Replay-ready transactions for ONE mode.
pub async fn fetch_mode_txs(pool: &SqlitePool, mode: &str) -> Result<Vec<Tx>> {
    Ok(fetch_txs(pool, Some(mode))
        .await?
        .into_iter()
        .map(|r| r.tx)
        .collect())
}

/// Distinct symbols across all modes (for stream subscriptions).
pub async fn all_symbols(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query(
        r#"SELECT DISTINCT i.symbol FROM transactions t
           JOIN instruments i ON i.id = t.instrument_id"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("symbol")).collect())
}

/// Symbols on the watchlist (for stream subscriptions).
pub async fn watchlist_symbols(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query(
        r#"SELECT i.symbol FROM watchlist w JOIN instruments i ON i.id = w.instrument_id"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("symbol")).collect())
}

/// Symbols with active alerts (their conditions need data to evaluate).
pub async fn alert_symbols(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query(
        r#"SELECT DISTINCT i.symbol FROM alerts a
           JOIN instruments i ON i.id = a.instrument_id
           WHERE a.status = 'active'"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("symbol")).collect())
}

/// Add a symbol to the watchlist. Returns false if it was already there.
pub async fn watchlist_add(pool: &SqlitePool, symbol: &str) -> Result<bool> {
    let mut dbtx = pool.begin().await?;
    let instrument_id = ensure_instrument(&mut dbtx, symbol).await?;
    let result =
        sqlx::query("INSERT OR IGNORE INTO watchlist (instrument_id, added_ts) VALUES (?, ?)")
            .bind(instrument_id)
            .bind(Utc::now().to_rfc3339())
            .execute(&mut *dbtx)
            .await?;
    dbtx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Remove a symbol from the watchlist. Returns false if it wasn't there.
pub async fn watchlist_remove(pool: &SqlitePool, symbol: &str) -> Result<bool> {
    let result = sqlx::query(
        r#"DELETE FROM watchlist WHERE instrument_id IN
           (SELECT id FROM instruments WHERE symbol = ?)"#,
    )
    .bind(symbol.to_uppercase())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Find-or-create an instrument row for a symbol; returns its id.
pub async fn ensure_instrument(conn: &mut SqliteConnection, symbol: &str) -> Result<i64> {
    let symbol = symbol.to_uppercase();
    sqlx::query("INSERT OR IGNORE INTO instruments (symbol) VALUES (?)")
        .bind(&symbol)
        .execute(&mut *conn)
        .await?;
    let row = sqlx::query("SELECT id FROM instruments WHERE symbol = ?")
        .bind(&symbol)
        .fetch_one(&mut *conn)
        .await?;
    Ok(row.get("id"))
}

pub struct NewTx {
    pub kind: TxKind,
    pub symbol: Option<String>,
    pub qty: Decimal,
    pub price: Decimal,
    pub fees: Decimal,
    pub taxes: Decimal,
    pub ts: DateTime<Utc>,
    pub note: String,
    pub source: TxSource,
    /// Links broker-fill transactions to their execution record.
    pub fill_id: Option<i64>,
    /// 'live' for real holdings (manual/CSV), 'paper' for paper-broker fills.
    pub mode: &'static str,
}

pub async fn insert_one(conn: &mut SqliteConnection, tx: &NewTx) -> Result<i64> {
    let instrument_id = match &tx.symbol {
        Some(symbol) => Some(ensure_instrument(conn, symbol).await?),
        None => None,
    };
    let result = sqlx::query(
        r#"INSERT INTO transactions
           (instrument_id, kind, qty, price, fees, taxes, ts, source, fill_id, note, mode)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(instrument_id)
    .bind(tx.kind.as_str())
    .bind(tx.qty.to_string())
    .bind(tx.price.to_string())
    .bind(tx.fees.to_string())
    .bind(tx.taxes.to_string())
    .bind(tx.ts.to_rfc3339())
    .bind(tx.source.as_str())
    .bind(tx.fill_id)
    .bind(&tx.note)
    .bind(tx.mode)
    .execute(&mut *conn)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Insert a batch atomically: one SQL transaction, all rows or none.
/// Returns the inserted ids.
pub async fn insert_txs(pool: &SqlitePool, txs: &[NewTx]) -> Result<Vec<i64>> {
    let mut dbtx = pool.begin().await?;
    let mut ids = Vec::with_capacity(txs.len());
    for tx in txs {
        ids.push(insert_one(&mut dbtx, tx).await?);
    }
    dbtx.commit().await?;
    Ok(ids)
}

/// Earliest transaction date across all modes (history/backfill start).
pub async fn first_tx_date(pool: &SqlitePool) -> Result<Option<NaiveDate>> {
    let row = sqlx::query("SELECT MIN(ts) AS ts FROM transactions")
        .fetch_one(pool)
        .await?;
    let ts: Option<String> = row.get("ts");
    Ok(ts
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc).date_naive()))
}

/// Upsert daily candles for a symbol (idempotent by (instrument, date)).
pub async fn upsert_candles(pool: &SqlitePool, symbol: &str, candles: &[Candle]) -> Result<u64> {
    if candles.is_empty() {
        return Ok(0);
    }
    let mut dbtx = pool.begin().await?;
    let instrument_id = ensure_instrument(&mut dbtx, symbol).await?;
    let mut written = 0;
    for candle in candles {
        written += sqlx::query(
            r#"INSERT OR REPLACE INTO candles
               (instrument_id, date, open, high, low, close, volume)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(instrument_id)
        .bind(candle.date.to_string())
        .bind(candle.open.to_string())
        .bind(candle.high.to_string())
        .bind(candle.low.to_string())
        .bind(candle.close.to_string())
        .bind(candle.volume)
        .execute(&mut *dbtx)
        .await?
        .rows_affected();
    }
    dbtx.commit().await?;
    Ok(written)
}

pub async fn last_candle_date(pool: &SqlitePool, symbol: &str) -> Result<Option<NaiveDate>> {
    let row = sqlx::query(
        r#"SELECT MAX(c.date) AS d FROM candles c
           JOIN instruments i ON i.id = c.instrument_id WHERE i.symbol = ?"#,
    )
    .bind(symbol)
    .fetch_one(pool)
    .await?;
    let date: Option<String> = row.get("d");
    Ok(date.as_deref().and_then(|s| s.parse().ok()))
}

/// All cached closes for the given symbols, keyed by symbol.
pub async fn closes_map(pool: &SqlitePool, symbols: &[String]) -> Result<HashMap<String, Closes>> {
    let mut map: HashMap<String, Closes> = HashMap::new();
    for symbol in symbols {
        let rows = sqlx::query(
            r#"SELECT c.date, c.close FROM candles c
               JOIN instruments i ON i.id = c.instrument_id
               WHERE i.symbol = ? ORDER BY c.date"#,
        )
        .bind(symbol)
        .fetch_all(pool)
        .await?;
        let mut closes = Closes::new();
        for row in rows {
            let date: String = row.get("date");
            closes.insert(
                date.parse().context("corrupt candle date")?,
                parse_dec(&row.get::<String, _>("close"), "candles.close")?,
            );
        }
        if !closes.is_empty() {
            map.insert(symbol.clone(), closes);
        }
    }
    Ok(map)
}

/// Most recent `limit` closes for one symbol, oldest first (signals input —
/// see `live::SIGNAL_WINDOW_BARS`; indicators like drawdown are computed
/// over exactly this window, so labels must say so).
pub async fn recent_closes(pool: &SqlitePool, symbol: &str, limit: i64) -> Result<Vec<Decimal>> {
    let rows = sqlx::query(
        r#"SELECT c.close FROM candles c
           JOIN instruments i ON i.id = c.instrument_id
           WHERE i.symbol = ? ORDER BY c.date DESC LIMIT ?"#,
    )
    .bind(symbol)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut closes: Vec<Decimal> = rows
        .into_iter()
        .map(|r| parse_dec(&r.get::<String, _>("close"), "candles.close"))
        .collect::<Result<_>>()?;
    closes.reverse();
    Ok(closes)
}

/// True if an EOD snapshot exists for the given date+mode.
pub async fn has_snapshot(pool: &SqlitePool, date: NaiveDate, mode: &str) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM snapshots WHERE date = ? AND mode = ?")
        .bind(date.to_string())
        .bind(mode)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// One stored EOD snapshot (equity curve fallback when candles are absent).
pub struct SnapshotRow {
    pub date: NaiveDate,
    pub total_value: Decimal,
    pub cash: Decimal,
    pub invested: Decimal,
}

/// All EOD snapshots for a mode, oldest first.
pub async fn fetch_snapshots(pool: &SqlitePool, mode: &str) -> Result<Vec<SnapshotRow>> {
    let rows = sqlx::query(
        r#"SELECT date, total_value, cash, invested FROM snapshots
           WHERE mode = ? ORDER BY date"#,
    )
    .bind(mode)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let date: String = row.get("date");
            Ok(SnapshotRow {
                date: date.parse().context("corrupt snapshot date")?,
                total_value: parse_dec(&row.get::<String, _>("total_value"), "total_value")?,
                cash: parse_dec(&row.get::<String, _>("cash"), "cash")?,
                invested: parse_dec(&row.get::<String, _>("invested"), "invested")?,
            })
        })
        .collect()
}

/// Write (or overwrite) the EOD snapshot row for a date+mode.
pub async fn upsert_snapshot(
    pool: &SqlitePool,
    date: NaiveDate,
    mode: &str,
    total_value: Decimal,
    cash: Decimal,
    invested: Decimal,
    realized_pnl: Decimal,
) -> Result<()> {
    sqlx::query(
        r#"INSERT OR REPLACE INTO snapshots (date, mode, total_value, cash, invested, realized_pnl)
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(date.to_string())
    .bind(mode)
    .bind(total_value.to_string())
    .bind(cash.to_string())
    .bind(invested.to_string())
    .bind(realized_pnl.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// One alert row, joined with its symbol, ready for the API/evaluator.
#[derive(Debug, serde::Serialize)]
pub struct AlertRecord {
    pub id: i64,
    pub symbol: String,
    pub condition: String,
    pub threshold: Decimal,
    /// The pre-staged order ticket, if any (alerts-to-action).
    pub draft_order: Option<serde_json::Value>,
    pub status: String,
    pub created_ts: String,
    pub triggered_ts: Option<String>,
    pub triggered_value: Option<Decimal>,
    pub note: String,
}

fn alert_from_row(row: sqlx::sqlite::SqliteRow) -> Result<AlertRecord> {
    let draft: Option<String> = row.get("draft_order_json");
    let triggered_value: Option<String> = row.get("triggered_value");
    Ok(AlertRecord {
        id: row.get("id"),
        symbol: row.get("symbol"),
        condition: row.get("condition"),
        threshold: parse_dec(&row.get::<String, _>("threshold"), "alerts.threshold")?,
        draft_order: draft
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .context("corrupt draft_order_json")?,
        status: row.get("status"),
        created_ts: row.get("created_ts"),
        triggered_ts: row.get("triggered_ts"),
        triggered_value: triggered_value
            .as_deref()
            .map(|v| parse_dec(v, "alerts.triggered_value"))
            .transpose()?,
        note: row.get("note"),
    })
}

const ALERT_SELECT: &str = r#"SELECT a.id, i.symbol, a.condition, a.threshold,
       a.draft_order_json, a.status, a.created_ts, a.triggered_ts,
       a.triggered_value, a.note
       FROM alerts a JOIN instruments i ON i.id = a.instrument_id"#;

/// All alerts, triggered first, then newest.
pub async fn list_alerts(pool: &SqlitePool) -> Result<Vec<AlertRecord>> {
    let rows = sqlx::query(&format!(
        "{ALERT_SELECT} ORDER BY (a.status = 'triggered') DESC, a.id DESC"
    ))
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(alert_from_row).collect()
}

/// Active alerts only (the evaluator's working set).
pub async fn active_alerts(pool: &SqlitePool) -> Result<Vec<AlertRecord>> {
    let rows = sqlx::query(&format!("{ALERT_SELECT} WHERE a.status = 'active'"))
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(alert_from_row).collect()
}

pub async fn insert_alert(
    pool: &SqlitePool,
    symbol: &str,
    condition: &str,
    threshold: Decimal,
    draft_order_json: Option<&str>,
    note: &str,
) -> Result<i64> {
    let mut dbtx = pool.begin().await?;
    let instrument_id = ensure_instrument(&mut dbtx, symbol).await?;
    let result = sqlx::query(
        r#"INSERT INTO alerts (instrument_id, condition, threshold, draft_order_json, created_ts, note)
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(instrument_id)
    .bind(condition)
    .bind(threshold.to_string())
    .bind(draft_order_json)
    .bind(Utc::now().to_rfc3339())
    .bind(note)
    .execute(&mut *dbtx)
    .await?;
    dbtx.commit().await?;
    Ok(result.last_insert_rowid())
}

/// Returns true if a row was deleted.
pub async fn delete_alert(pool: &SqlitePool, id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM alerts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// active → triggered, recording when and at what value. The status guard
/// makes concurrent evaluator passes idempotent (first one wins).
pub async fn trigger_alert(pool: &SqlitePool, id: i64, observed: Decimal) -> Result<bool> {
    let result = sqlx::query(
        r#"UPDATE alerts SET status = 'triggered', triggered_ts = ?, triggered_value = ?
           WHERE id = ? AND status = 'active'"#,
    )
    .bind(Utc::now().to_rfc3339())
    .bind(observed.to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// triggered → dismissed. Returns false if the alert wasn't triggered.
pub async fn dismiss_alert(pool: &SqlitePool, id: i64) -> Result<bool> {
    let result =
        sqlx::query("UPDATE alerts SET status = 'dismissed' WHERE id = ? AND status = 'triggered'")
            .bind(id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// Any status → active again, clearing the trigger record.
pub async fn rearm_alert(pool: &SqlitePool, id: i64) -> Result<bool> {
    let result = sqlx::query(
        r#"UPDATE alerts SET status = 'active', triggered_ts = NULL, triggered_value = NULL
           WHERE id = ?"#,
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Durable key/value settings (e.g. the trading pause switch).
pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get("value")))
}

pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns true if a row was deleted.
pub async fn delete_tx(pool: &SqlitePool, id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM transactions WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
