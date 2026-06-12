//! SQLite repository. Money travels as TEXT decimal strings (PLAN.md §7);
//! parsing failures are data corruption and surface as errors, never zeros.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rekt_core::portfolio::{Tx, TxKind};
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
