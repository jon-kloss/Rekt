//! Multiple portfolios: one SQLite file per portfolio, selectable at runtime.
//!
//! REKT is single-portfolio by design (one process, one DB file). To let a
//! user keep e.g. a "test" portfolio apart from their "real" data WITHOUT the
//! fragility of hot-swapping the live DB pool + ~10 DB-keyed caches across ~10
//! background tasks, a switch simply RE-EXECS the process onto a different file
//! — a fresh process is a clean slate and reuses the entire existing boot path.
//!
//! The active selection lives in a small JSON registry OUTSIDE any single
//! portfolio DB (you can't know which DB to open from inside one of them).
//! Per-portfolio Alpaca paper keys stay env-only (a naming convention),
//! resolved by `main()` at boot — never persisted.

use std::path::{Path, PathBuf};

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{err, internal, ApiError};
use crate::AppState;

const REGISTRY_FILE: &str = "portfolios.json";
const DEFAULT_NAME: &str = "real";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioEntry {
    pub name: String,
    /// DB file path, relative to the data dir.
    pub db: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "one")]
    pub version: u32,
    pub active: String,
    pub portfolios: Vec<PortfolioEntry>,
}

fn one() -> u32 {
    1
}

impl Registry {
    /// The active entry, falling back to the first portfolio if the `active`
    /// pointer is dangling (corrupt registry should never blank the app).
    pub fn active_entry(&self) -> &PortfolioEntry {
        self.portfolios
            .iter()
            .find(|p| p.name == self.active)
            .or_else(|| self.portfolios.first())
            .expect("registry always has at least one portfolio")
    }
}

/// Data directory holding the registry + portfolio DB files: `REKT_DATA_DIR`
/// if set, else the parent of `REKT_DB` (so an existing
/// `/var/lib/rekt/rekt.db` install needs no config change), else `.`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var("REKT_DATA_DIR")
        .ok()
        .filter(|d| !d.is_empty())
    {
        return PathBuf::from(dir);
    }
    let db = std::env::var("REKT_DB").unwrap_or_else(|_| "rekt.db".into());
    match Path::new(&db).parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The default ("real") portfolio's DB filename — the basename of `REKT_DB`,
/// so the user's existing database becomes the default portfolio untouched.
fn default_db_basename() -> String {
    let db = std::env::var("REKT_DB").unwrap_or_else(|_| "rekt.db".into());
    Path::new(&db)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rekt.db".into())
}

fn registry_path(data_dir: &Path) -> PathBuf {
    data_dir.join(REGISTRY_FILE)
}

/// Load the registry, or synthesize the default single-portfolio one in memory
/// (NOT written to disk — a pure single-portfolio user never gets a new file
/// until they actually create/switch a portfolio).
pub fn load(data_dir: &Path) -> Registry {
    match std::fs::read_to_string(registry_path(data_dir)) {
        Ok(s) => match serde_json::from_str::<Registry>(&s) {
            Ok(reg) if !reg.portfolios.is_empty() => reg,
            _ => {
                tracing::warn!("portfolios.json unreadable — using the default portfolio");
                default_registry()
            }
        },
        Err(_) => default_registry(),
    }
}

fn default_registry() -> Registry {
    Registry {
        version: 1,
        active: DEFAULT_NAME.into(),
        portfolios: vec![PortfolioEntry {
            name: DEFAULT_NAME.into(),
            db: default_db_basename(),
        }],
    }
}

/// Atomically persist the registry (temp file + rename).
pub fn save(data_dir: &Path, reg: &Registry) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let body = serde_json::to_string_pretty(reg).map_err(std::io::Error::other)?;
    let tmp = registry_path(data_dir).with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, registry_path(data_dir))
}

/// Absolute DB path for an entry, with a path-traversal guard: the resolved
/// file must stay inside the data dir.
pub fn db_path_for(data_dir: &Path, entry: &PortfolioEntry) -> Result<PathBuf, String> {
    let joined = data_dir.join(&entry.db);
    // Lexical containment check (the file may not exist yet, so we can't
    // canonicalize it): every component must descend, none may be `..`.
    if Path::new(&entry.db).components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) || Path::new(&entry.db).is_absolute()
    {
        return Err(format!("invalid portfolio db path {:?}", entry.db));
    }
    Ok(joined)
}

/// Lowercase, alphanumeric-or-dash slug used for the DB filename and the
/// per-portfolio key env var.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Validate a user-supplied portfolio name.
pub fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim().to_string();
    if name.is_empty() || name.chars().count() > 40 {
        return Err("name must be 1–40 characters".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-')
    {
        return Err("name may only contain letters, numbers, spaces, _ and -".into());
    }
    if slugify(&name).is_empty() {
        return Err("name must contain at least one letter or number".into());
    }
    Ok(name)
}

/// Per-portfolio Alpaca paper keys by convention: `ALPACA_PAPER_KEY_<SLUG>` /
/// `ALPACA_PAPER_SECRET_<SLUG>` (slug upper-cased, dashes → underscores).
/// Returns `None` when not both set, so the caller falls back to the global
/// `ALPACA_PAPER_KEY`/`_SECRET`. Secrets are never persisted — only read here.
pub fn resolve_alpaca_keys(name: &str) -> Option<(String, String)> {
    let suffix = slugify(name).to_uppercase().replace('-', "_");
    if suffix.is_empty() {
        return None;
    }
    let key = std::env::var(format!("ALPACA_PAPER_KEY_{suffix}"))
        .ok()
        .filter(|k| !k.is_empty())?;
    let secret = std::env::var(format!("ALPACA_PAPER_SECRET_{suffix}"))
        .ok()
        .filter(|k| !k.is_empty())?;
    Some((key, secret))
}

/// Re-exec the current binary in place (same PID on unix), so the fresh
/// `main()` re-reads the registry and boots onto the now-active portfolio.
/// Env is inherited (REKT_DB/REKT_LISTEN/keys carry over); the listener fd is
/// CLOEXEC so the new image re-binds cleanly. Returns only on failure.
#[cfg(unix)]
fn reexec() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from(std::env::args_os().next().unwrap_or_default()));
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec()
}

#[cfg(not(unix))]
fn reexec() -> std::io::Error {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return e,
    };
    match std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn()
    {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}

// ------------------------------------------------------------- handlers --

#[derive(Debug, Deserialize)]
pub struct NameInput {
    pub name: String,
}

/// GET /api/portfolios — the registry as the UI needs it.
pub async fn list(State(state): State<AppState>) -> Json<Value> {
    let reg = load(&state.data_dir);
    let portfolios: Vec<Value> = reg
        .portfolios
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "active": p.name == reg.active,
                // Whether this portfolio has its own paper-trading account.
                "isolated_broker": resolve_alpaca_keys(&p.name).is_some(),
            })
        })
        .collect();
    Json(json!({ "active": reg.active, "portfolios": portfolios }))
}

/// POST /api/portfolios — create a new named portfolio (does not switch). The
/// DB file is created lazily when the process first opens it (on switch).
pub async fn create(
    State(state): State<AppState>,
    Json(input): Json<NameInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    crate::demo_guard(&state)?;
    let name = validate_name(&input.name).map_err(|m| err(StatusCode::BAD_REQUEST, m))?;
    // Serialize registry mutations so two concurrent creates can't lose an
    // entry (load → push → save is read-modify-write).
    let _guard = state.mutations.lock().await;
    let mut reg = load(&state.data_dir);
    let slug = slugify(&name);
    // Reject on either a name OR a slug collision: distinct names can slugify
    // to the same file (e.g. "growth test" and "growth-test" both → growth-test),
    // which would silently make two portfolios share one DB.
    if reg
        .portfolios
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(&name))
    {
        return Err(err(
            StatusCode::CONFLICT,
            format!("a portfolio named {name:?} already exists"),
        ));
    }
    if let Some(clash) = reg.portfolios.iter().find(|p| slugify(&p.name) == slug) {
        return Err(err(
            StatusCode::CONFLICT,
            format!(
                "{name:?} maps to the same file as the existing portfolio {:?} — pick a more distinct name",
                clash.name
            ),
        ));
    }
    let db = format!("portfolios/{slug}.db");
    let entry = PortfolioEntry {
        name: name.clone(),
        db,
    };
    // Path-traversal guard (defense in depth — slug is already constrained).
    db_path_for(&state.data_dir, &entry).map_err(|m| err(StatusCode::BAD_REQUEST, m))?;
    reg.portfolios.push(entry.clone());
    save(&state.data_dir, &reg).map_err(internal)?;
    tracing::info!(name = %name, db = %entry.db, "portfolio created");
    Ok((
        StatusCode::CREATED,
        Json(json!({ "name": name, "db": entry.db })),
    ))
}

/// POST /api/portfolios/switch — persist the new active pointer, then re-exec.
/// Blocks if any order is still working (a switch must not orphan an order).
pub async fn switch(
    State(state): State<AppState>,
    Json(input): Json<NameInput>,
) -> Result<Json<Value>, ApiError> {
    crate::demo_guard(&state)?;
    let name = input.name.trim().to_string();
    let mut reg = load(&state.data_dir);
    if !reg.portfolios.iter().any(|p| p.name == name) {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no portfolio named {name:?}"),
        ));
    }
    if reg.active == name {
        return Ok(Json(json!({ "switching": name, "noop": true })));
    }

    // Hold the mutations lock across the whole flip so no transaction/order can
    // interleave between the open-order check and the pool close (otherwise a
    // write could land in a DB the registry no longer points at, or an order
    // could be submitted to the broker with no local record after re-exec).
    let _guard = state.mutations.lock().await;

    // Don't switch out from under a working order.
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM orders
         WHERE status NOT IN ('filled','canceled','rejected','expired','replaced','failed')",
    )
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;
    if open > 0 {
        return Err(err(
            StatusCode::CONFLICT,
            "cancel or wait for open orders before switching portfolios",
        ));
    }

    reg.active = name.clone();
    save(&state.data_dir, &reg).map_err(internal)?;
    tracing::info!(to = %name, "switching active portfolio — re-exec");

    // Fold the WAL back into the file and drain the pool NOW, while we still
    // hold the lock — once the pool is closed no later write can land in the
    // stale (old) DB even though the registry already names the new one. (exec
    // runs no destructors, so this must happen before the image is replaced.)
    let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&state.db)
        .await;
    state.db.close().await;

    // Re-exec only AFTER this response has had a moment to flush: the pool is
    // already closed, so the brief window is safe (further writes just error).
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let e = reexec();
        tracing::error!(error = %e, "re-exec failed — exiting for a supervisor to restart");
        std::process::exit(1);
    });

    Ok(Json(json!({ "switching": name })))
}

/// DELETE /api/portfolios/{name} — remove a portfolio and (unless another
/// entry still points at the same file) delete its DB. Refuses the active
/// portfolio (switch away first) and the last remaining one.
pub async fn delete(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    crate::demo_guard(&state)?;
    let name = name.trim().to_string();
    // Serialize against create/switch/other deletes (read-modify-write).
    let _guard = state.mutations.lock().await;
    let mut reg = load(&state.data_dir);
    let Some(idx) = reg.portfolios.iter().position(|p| p.name == name) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no portfolio named {name:?}"),
        ));
    };
    if reg.active == name {
        return Err(err(
            StatusCode::CONFLICT,
            "switch to another portfolio before deleting this one",
        ));
    }
    if reg.portfolios.len() <= 1 {
        return Err(err(StatusCode::CONFLICT, "can't delete the only portfolio"));
    }

    let entry = reg.portfolios.remove(idx);
    // Persist the registry BEFORE touching the file. A crash between the two
    // then leaves at worst an orphaned DB file (harmless, recoverable) rather
    // than a registry that still names a portfolio whose file is gone (which
    // would fail to open on next boot).
    save(&state.data_dir, &reg).map_err(internal)?;

    // Only delete the file if no remaining entry references it (defends against
    // a legacy registry where two names mapped to the same DB) — never delete a
    // file another portfolio, or the active pool, still uses.
    let still_referenced = reg.portfolios.iter().any(|p| p.db == entry.db);
    if !still_referenced {
        if let Ok(db_path) = db_path_for(&state.data_dir, &entry) {
            // Drop the DB and its WAL/SHM sidecars so re-creating the same name
            // yields a fresh dataset, never stale rows. Best-effort: a file that
            // was never opened simply isn't there.
            for suffix in ["", "-wal", "-shm"] {
                let mut os = db_path.clone().into_os_string();
                os.push(suffix);
                let p = PathBuf::from(os);
                match std::fs::remove_file(&p) {
                    Ok(()) => tracing::info!(path = %p.display(), "deleted portfolio db file"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(path = %p.display(), error = %e, "could not delete db file")
                    }
                }
            }
        }
    }

    tracing::info!(name = %name, "portfolio deleted");
    Ok(Json(json!({ "deleted": name })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("Real"), "real");
        assert_eq!(slugify("My Test 2!"), "my-test-2");
        assert_eq!(slugify("  a  b  "), "a-b");
        assert_eq!(slugify("___"), "");
    }

    #[test]
    fn distinct_names_can_collide_on_slug() {
        // The create handler must reject the second of these (same DB file).
        assert_eq!(slugify("growth test"), slugify("growth-test"));
        assert_eq!(slugify("growth test"), slugify("growth_test"));
        assert_eq!(slugify("growth test"), "growth-test");
        // ...but genuinely distinct names keep distinct slugs.
        assert_ne!(slugify("real"), slugify("test"));
    }

    #[test]
    fn validate_name_rules() {
        assert_eq!(validate_name(" Test ").unwrap(), "Test");
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("../etc").is_err()); // '/' and '.' rejected
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(&"x".repeat(41)).is_err());
        assert!(validate_name("!!!").is_err()); // slug empty
        assert!(validate_name("ok-name_1").is_ok());
    }

    #[test]
    fn db_path_guard_blocks_traversal() {
        let dir = Path::new("/data");
        let ok = PortfolioEntry {
            name: "test".into(),
            db: "portfolios/test.db".into(),
        };
        assert_eq!(
            db_path_for(dir, &ok).unwrap(),
            PathBuf::from("/data/portfolios/test.db")
        );
        let evil = PortfolioEntry {
            name: "x".into(),
            db: "../../etc/passwd".into(),
        };
        assert!(db_path_for(dir, &evil).is_err());
        let abs = PortfolioEntry {
            name: "x".into(),
            db: "/etc/passwd".into(),
        };
        assert!(db_path_for(dir, &abs).is_err());
    }

    #[test]
    fn registry_active_entry_falls_back() {
        let reg = Registry {
            version: 1,
            active: "missing".into(),
            portfolios: vec![PortfolioEntry {
                name: "real".into(),
                db: "rekt.db".into(),
            }],
        };
        assert_eq!(reg.active_entry().name, "real"); // dangling active → first
    }

    #[test]
    fn registry_round_trips() {
        let reg = default_registry();
        let s = serde_json::to_string(&reg).unwrap();
        let back: Registry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.active, reg.active);
        assert_eq!(back.portfolios.len(), 1);
    }
}
