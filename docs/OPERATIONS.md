# Operating REKT

REKT is a single self-contained binary (SQLite is compiled in) plus one SQLite
file. This guide covers
running it for real: deployment, the data file and its backups, upgrades,
monitoring, the security posture, and the deliberate paper-only stance on live
trading. For what REKT *is* and the design behind it, see
[README.md](../README.md) and [PLAN.md](../PLAN.md).

## What you are running

- **One process.** The `rekt` binary serves the API and the embedded UI, runs the
  live market-data pipeline, the order manager, and the scheduled jobs (candle
  backfill, EOD snapshots, the AI analyst). There is no separate worker.
- **One data file.** All durable state lives in the SQLite database at `REKT_DB`
  (default `rekt.db`), opened in WAL mode. Everything else — positions, P&L,
  the equity curve, signals — is *derived* and recomputable from the
  transaction log, so the transaction rows are the only irreplaceable data.
- **Secrets stay in the environment.** API keys are read from env vars at boot
  and never written to the database, the logs, or the browser.
- **Four index gauges are always fetched.** The MARKET overview reads SPY/QQQ/
  IWM/DIA, so those four symbols join the backfill + live-refresh sets
  regardless of what you hold (SPY was already the benchmark, so the marginal
  cost is +3 symbols). On a streaming Finnhub feed this is three extra
  websocket subscriptions; on the REST-poll fallback it is ~3 extra quote calls
  per minute — well within free-tier limits, but it means even a zero-position
  install does a small amount of always-on market-data work.

## Configuration

All configuration is environment variables (no config file). Unset optional
keys disable the corresponding feature honestly — the app degrades to clear
errors or `None`, never fabricated data.

| Variable | Default | Purpose |
|---|---|---|
| `REKT_DB` | `rekt.db` | SQLite file path (the default/"real" portfolio — see Multiple portfolios) |
| `REKT_DATA_DIR` | parent of `REKT_DB` | directory holding the portfolio registry + per-portfolio DB files |
| `REKT_LISTEN` | `127.0.0.1:7777` | bind address (loopback by default — see Security) |
| `FINNHUB_API_KEY` | — | live quotes + trade stream; falls back to Alpaca data |
| `ALPACA_PAPER_KEY` / `ALPACA_PAPER_SECRET` | — | **paper** trading + daily bars |
| `REKT_ANALYST_BACKEND` | `cli` | analyst backend: `cli` (default) drives the local `claude` CLI (`claude -p`) — reuses its auth, no key needed; `http` (or `api`) uses the HTTP API with `ANTHROPIC_API_KEY`; `ollama` (or `local`) uses a local Ollama model — free, private, offline. All run advisory-only (tool-less for cli/ollama; the analyst can never place orders) |
| `ANTHROPIC_API_KEY` | — | AI analyst via HTTP API (advisory only); required only when `REKT_ANALYST_BACKEND=http` |
| `REKT_OLLAMA_URL` | `http://localhost:11434` | Ollama server URL (when `REKT_ANALYST_BACKEND=ollama`) |
| `REKT_OLLAMA_MODEL` | `llama3.1` | Ollama model to use; `ollama pull` it first. Market-idea theses stay grounded because the deterministic screener picks the candidates — a small local model only narrates them, so it bills $0 |
| `REKT_AI_DAILY_BUDGET` | `2.50` | USD/day ceiling that gates analyst runs |
| `REKT_AI_AUTO` | enabled (unset) | `0`, `false`, or `off` disable the scheduled briefing/review |
| `REKT_NTFY_TOPIC` / `REKT_NTFY_URL` | — | alert push (see Security for the topic warning) |
| `REKT_MAX_ORDER_NOTIONAL` | `10000` | per-order notional cap |
| `REKT_MAX_POSITION_PCT` | `25` | max single-position % of equity |
| `REKT_MAX_ORDERS_PER_DAY` | `20` | daily order count cap |
| `REKT_MAX_DAILY_LOSS` | `1000` | circuit breaker on new buys (`≤0` disables) |
| `RUST_LOG` | `info` | tracing filter (e.g. `rekt_server=debug,info`) |

## Multiple portfolios

REKT serves **one portfolio at a time**, but you can keep several side by side —
e.g. a `test` portfolio for experimenting and your `real` data — and switch
between them from the header dropdown. Each portfolio is its own SQLite file, so
the isolation of tracked data (holdings, imports, analyses, taxes, alerts,
orders, candles) is **total** — test activity can never touch real data.

**How it works.** A small registry, `<data_dir>/portfolios.json`, records the
named portfolios and which is active (`data_dir` = `REKT_DATA_DIR`, else the
parent of `REKT_DB`). Your existing `REKT_DB` becomes the default portfolio,
`real`, untouched — and `portfolios.json` isn't even written until you create a
second portfolio, so single-portfolio installs are unchanged. New portfolios get
their own file under `<data_dir>/portfolios/<name>.db`, created (with migrations)
the first time it's opened.

**Switching restarts the process.** Picking a portfolio re-execs the binary onto
the chosen file (same PID on unix; the page reconnects in ~1s). This reuses the
entire normal startup path instead of risking a live database swap — the cleanest
possible isolation. A switch is refused while any order is still working; cancel
or wait first. (Operators who prefer their supervisor to own restarts can run
behind systemd; re-exec works there too, no unit change needed.)

**Per-portfolio paper trading (optional).** The Alpaca **paper account is a
single external account**, so by default every portfolio shares it — a paper
order under `test` lands on the same broker positions as `real`. To give a
portfolio its own paper account, set its keys by convention:
`ALPACA_PAPER_KEY_<NAME>` / `ALPACA_PAPER_SECRET_<NAME>` (name upper-cased,
dashes → underscores; e.g. `ALPACA_PAPER_KEY_TEST`). Keys are **never written to
the registry** — they stay env-only. When present, that portfolio uses them;
otherwise it falls back to the global `ALPACA_PAPER_KEY`/`_SECRET`.

**Still shared across portfolios:** the Finnhub/market-data quota and the Claude
analyst's auth + daily budget (`REKT_AI_DAILY_BUDGET` is *tracked* per-portfolio,
but the underlying account/quota is one). Guardrail env limits are global too.

**Backups:** each portfolio is an independent file — apply the backup guidance
below per file, and back up the tiny `portfolios.json` alongside them.

## Deployment (systemd)

Build a release binary and run it as an unprivileged service with the data file
on persistent storage.

```sh
cargo build --release -p rekt-server   # → target/release/rekt  (the bin is named `rekt`)
```

`/etc/rekt/rekt.env` (owned `root:rekt`, mode `0640` — it holds your keys):

```sh
REKT_DB=/var/lib/rekt/rekt.db
REKT_LISTEN=127.0.0.1:7777
FINNHUB_API_KEY=...
ALPACA_PAPER_KEY=...
ALPACA_PAPER_SECRET=...
ANTHROPIC_API_KEY=...
```

`/etc/systemd/system/rekt.service`:

```ini
[Unit]
Description=REKT — Real-time Equity & Capital Tracker
After=network-online.target
Wants=network-online.target

[Service]
User=rekt
Group=rekt
EnvironmentFile=/etc/rekt/rekt.env
ExecStart=/usr/local/bin/rekt
Restart=on-failure
RestartSec=5
# Hardening: the process only needs its data directory. WorkingDirectory is
# the writable StateDirectory so even a relative REKT_DB resolves there
# (ProtectSystem=strict makes the default working dir, /, read-only).
WorkingDirectory=/var/lib/rekt
StateDirectory=rekt
ProtectSystem=strict
ReadWritePaths=/var/lib/rekt
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin rekt
sudo install -m755 target/release/rekt /usr/local/bin/
sudo systemctl daemon-reload && sudo systemctl enable --now rekt
journalctl -u rekt -f         # logs
```

Migrations run automatically at boot, so first start creates the schema and
later starts apply any new migrations before serving.

## Exposing it safely (reverse proxy + TLS)

REKT binds **loopback** by default and has **no authentication of its own** — it
assumes a single trusted user on a trusted host. Do not move `REKT_LISTEN` to
`0.0.0.0` and call it done. To reach it from elsewhere, put a TLS-terminating
reverse proxy in front and add auth there (mTLS, HTTP basic over TLS, or an
auth proxy / VPN). Keep `REKT_LISTEN=127.0.0.1:7777` and have the proxy connect
to loopback.

Example Caddy block (automatic TLS + basic auth):

```
rekt.example.com {
    basic_auth {
        you $2a$14$...        # caddy hash-password
    }
    reverse_proxy 127.0.0.1:7777
}
```

The WebSocket at `/api/ws` upgrades over the same origin, so a proxy that
forwards `Upgrade`/`Connection` headers (Caddy and nginx do by default) needs no
extra config.

## Backups & restore

The transaction log is the only irreplaceable data, but back up the whole file —
it is small and a restore is then a single copy.

The commands below use the standalone **`sqlite3` CLI** — a separate OS package
(`sqlite3` on Debian/Ubuntu, `sqlite` on Alpine), not shipped with REKT even
though the server bundles its own SQLite. Install it on the host (and confirm
`command -v sqlite3`) before wiring up the cron job, or the backup silently does
nothing.

**Do not `cp` a live database.** In WAL mode the latest writes live in the
`-wal` sidecar; a raw copy of just `rekt.db` can be stale or torn. Take a
consistent snapshot with SQLite's online backup, which is safe to run while REKT
is serving:

```sh
# Consistent point-in-time snapshot into a single file (safe while live):
sqlite3 /var/lib/rekt/rekt.db "VACUUM INTO '/var/backups/rekt-$(date +%F).db'"
```

`VACUUM INTO` writes one self-contained, already-compacted file — no `-wal`/`-shm`
sidecars to carry along. Drop that command in a `cron`/timer and keep a rolling
set (e.g. 14 daily + 8 weekly). Verify a backup occasionally:

```sh
sqlite3 /var/backups/rekt-2026-06-13.db "PRAGMA integrity_check; SELECT count(*) FROM transactions;"
```

**Restore:** stop the service, replace the data file with a backup, restart.

```sh
sudo systemctl stop rekt
sudo install -o rekt -g rekt -m600 /var/backups/rekt-2026-06-13.db /var/lib/rekt/rekt.db
sudo rm -f /var/lib/rekt/rekt.db-wal /var/lib/rekt/rekt.db-shm  # stale sidecars, if any
sudo systemctl start rekt
```

Because every derived view is recomputed from the transaction log at startup,
a restored file needs no extra rebuild step — candle history re-backfills on its
own from the providers.

## Upgrades

```sh
git pull
cargo build --release -p rekt-server
sudo install -m755 target/release/rekt /usr/local/bin/
sudo systemctl restart rekt
```

Migrations are forward-only and apply automatically on the next start. Take a
backup (above) before upgrading across a migration so you can roll back the
binary *and* the data together if needed.

## Monitoring

`GET /api/health` is an unauthenticated liveness + readiness probe:

```jsonc
{
  "status": "ok",            // "degraded" if the DB is unreachable
  "version": "0.1.0",
  "uptime_seconds": 86400,
  "db": true,
  "components": {            // honest readiness — present/absent, no secrets
    "market_data": "finnhub",  // provider name, or "unconfigured"
    "daily_bars": true,
    "trading_paper": true,
    "ai_analyst": true,
    "alert_push": false
  }
}
```

Point your uptime monitor at `status == "ok"`. The `components` map is for
confirming a deployment wired up the env you intended — if `market_data` reads
`unconfigured` or `ai_analyst` is `false` when you expected otherwise, a key is
missing.

### Log levels

Logs are structured (tracing) on stdout/journald. `RUST_LOG` selects the level
per module; every code path has logging available:

- **`info`** (default) — lifecycle and notable events: startup config, orders
  submitted/filled, alerts triggered, scheduled analyst runs, backfills.
- **`debug`** — per-operation detail for tracing a single request end to end:
  every HTTP handler entry, each DB mutation (with the rows affected), every
  outbound provider/broker/Claude API call and its status, and each analyst
  loop iteration. Secrets are never logged — API keys live in request headers
  or query strings that are deliberately excluded.
- **`trace`** — the per-tick hot paths: inbound price ticks, snapshot
  broadcasts, scheduler ticks, and history cache hits. Noisy by design; scope
  it narrowly.

```sh
RUST_LOG=rekt_server=debug,rekt_broker=debug,info   # debug REKT, quiet deps
RUST_LOG=rekt_server::live=trace,info               # watch the live pipeline
```

The pure `rekt-core` crate (portfolio/tax/signal math) is intentionally
I/O-free and carries no logging of its own; it is deterministic, so its inputs
and outputs are visible in the `debug` logs of the server boundary that calls
it (e.g. "history cache miss — rebuilding series", "tax report built").

## Security posture

- **Loopback by default; no built-in auth.** Single-user, single-host by design.
  Front it with a TLS proxy + auth to expose it (above).
- **Secrets are env-only.** Keys are never persisted, logged, or sent to the
  browser. Keep the env file `0640` and owned by the service user.
- **The ntfy topic name is itself a secret.** `REKT_NTFY_TOPIC` publishes to the
  public `ntfy.sh` — anyone who guesses the topic can read your trade alerts.
  Use a long random topic, or self-host ntfy and set `REKT_NTFY_URL`. REKT warns
  about this at startup.
- **The AI analyst cannot trade.** `rekt-analyst` has no dependency path to
  `rekt-broker`; its tools are read-only and a recommendation can only *prefill*
  the normal guarded order ticket a human confirms. This is enforced by the
  crate graph, not just convention. The `REKT_ANALYST_BACKEND=cli` backend
  launches `claude -p` with an empty `--allowed-tools` allowlist, so the CLI
  can run no tools at all (no bash, no file reads, no network) — the
  advisory-only guarantee holds a second way, by allowlist as well as by graph.
- **License.** AGPL-3.0 — if you run a modified REKT as a network service, you
  must offer your changes to its users.

## Live trading: deliberately not wired

REKT trades **paper only** today. Live mode is intentionally absent, not
unfinished — the locked decision (PLAN.md §4) is that real money waits behind an
explicit opt-in after a multi-week paper soak proves the order path. Enabling it
later is a deliberate, reviewable change, not a flag flip:

1. **Separate credentials.** Live Alpaca keys are distinct env vars from the
   paper keys; the two never share configuration. The data layer already
   segregates by `transactions.mode`: **paper-broker fills are recorded
   `mode='paper'` and never touch the live portfolio**, while your manually
   entered and imported transactions are real tracked holdings (`mode='live'`).
   A soak runs against the Alpaca *paper broker* (mode='paper'), so its fills
   can never contaminate the live portfolio — wiring live trading would record
   real fills as `mode='live'` alongside your holdings.
2. **Explicit, logged opt-in.** Live requires an unambiguous switch (e.g.
   `REKT_TRADING_MODE=live`) that is loud at startup and in the UI — never the
   default.
3. **Every guardrail still applies**, plus live-only ones worth adding before
   the switch: a hard kill-switch, a first-run confirmation, and ingestion of
   real broker fees/commissions (paper fills are frictionless, so cost-basis and
   tax math should be validated against real fees during the soak).
4. **Soak first.** Run paper against your real strategy for weeks; reconcile the
   equity curve, P&L, taxes, and the analyst's track record against the broker's
   own statements before risking a dollar.

Until all of that is in place and reviewed, keep REKT on paper. Trading involves
risk of loss; REKT is analysis and tooling, **not financial advice**.
