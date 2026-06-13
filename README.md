# REKT — Real-time Equity & Capital Tracker

> Find out exactly how rekt you are, live — and do something about it.

A self-hosted, single-user web dashboard for **tracking and trading** a US
stocks & ETFs portfolio in real time, with an AI analyst watching over your
shoulder. One Rust binary, one SQLite file, your keys never leave your box.

**Status: Phase 10 (hardening toward v1.0)** — tracking, paper trading, history & insight,
alerts-to-action, the **AI analyst** (morning briefings, weekly deep
reviews with web search, on-demand analysis over the Claude API — all
**advisory only**: no dependency path to the broker, read-only tools,
recommendations only prefill the guarded order ticket, every run
cost-metered against a daily budget, and now with **outcome tracking**:
every recommendation is scored against what the price actually did —
direction-adjusted, so a sell call is right when the price falls — and
the analyst sees its own track record before making new calls), and
**taxes**: per-lot Form 8949 rows with full wash-sale treatment (IRC
§1091, code W) — disallowed losses **carry forward into the replacement
lot's basis** with the holding period tacked on, matching broker 1099-B
practice — plus Schedule D totals and CSV export. Same-symbol matching
only; reconcile with your broker's 1099-B; not tax advice. See
[PLAN.md](PLAN.md) for the full design and roadmap, and
[docs/RESEARCH.md](docs/RESEARCH.md) for the research behind it.

## Quick start

```sh
# market data (free key from https://finnhub.io)
export FINNHUB_API_KEY=your_key

# paper trading (free account at https://alpaca.markets — paper keys only;
# live trading is deliberately not wired up yet)
export ALPACA_PAPER_KEY=your_key
export ALPACA_PAPER_SECRET=your_secret

# AI analyst (advisory only). Defaults to the local Claude Code CLI — it
# reuses the `claude` you've already signed in (must be on PATH), so no API
# key is needed. Runs tool-less by design (it can never place orders).
# To use the HTTP API instead, set a key and opt in:
# export ANTHROPIC_API_KEY=your_key
# export REKT_ANALYST_BACKEND=http

cargo run -p rekt-server
# → http://127.0.0.1:7777
```

Everything degrades honestly: without keys, the affected features answer
with clear errors instead of pretending.

Configuration (env vars): `REKT_DB` (default `rekt.db`), `REKT_LISTEN`
(default `127.0.0.1:7777`), `FINNHUB_API_KEY`, `ALPACA_PAPER_KEY`/`_SECRET`,
alert push: `REKT_NTFY_TOPIC` (topic on the public ntfy.sh — **the topic
name is the only secret**: anyone who guesses it can read your trade
alerts, so use a long random string, or better, self-host ntfy and set
`REKT_NTFY_URL`), AI analyst: `REKT_ANALYST_BACKEND` (default `cli` — drives the local `claude`
CLI, reusing its auth and running tool-less so it can never place orders or
touch the filesystem; set to `http` to use the API instead),
`ANTHROPIC_API_KEY` (required only for the `http` backend),
`REKT_AI_DAILY_BUDGET`
(USD/day, default 2.50 — gates new runs, reserving each run's worst-case
output cost; a run already in flight may finish somewhat past the ceiling
and the next run is then blocked), `REKT_AI_AUTO` (set 0 to disable the
scheduled briefing/review), and guardrails:
`REKT_MAX_ORDER_NOTIONAL` (default 10000), `REKT_MAX_POSITION_PCT`
(default 25), `REKT_MAX_ORDERS_PER_DAY` (default 20),
`REKT_MAX_DAILY_LOSS` (circuit breaker on new buys, default 1000;
≤0 disables).

## Bringing over your portfolio

REKT is a transaction ledger: you import your **activity history** (deposits,
buys, sells, dividends) and it replays them into positions, cost basis, P&L,
and tax lots. Import the full history for accurate realized gains and
wash-sale handling — a snapshot of current holdings alone won't reconstruct
those.

Use the **⬆ IMPORT CSV** button on the Blotter tab. Pick a format — **Generic**
(REKT's native `kind,symbol,qty,price,fees,taxes,ts,note`) or a broker export
(**Fidelity**, **Schwab**, **Robinhood**, **Interactive Brokers**) — drop in a
file or paste the CSV, then **Preview** to see exactly what will import and what
gets skipped (and why) before you **Confirm**. Broker exports can be pasted raw,
preamble and all; rows that aren't portfolio transactions (interest, fees,
options, journal entries) are reported as skips, never silently dropped. The
same thing is scriptable: `POST /api/import/csv?format=robinhood`
(add `&dry_run=true` for the preview).

Two broker-specific notes: **splits** are reported but not auto-applied (broker
exports give a share delta, not a clean ratio — enter them manually), and
**Robinhood crypto** trades use the same buy/sell codes as equities and would
import as if they were stocks, so review the preview before confirming.

## Running it for real

Deployment (systemd), reverse-proxy + TLS, the SQLite backup/restore
procedure, monitoring via `/api/health`, the security posture, and the
deliberate paper-only stance on live trading are all in
[docs/OPERATIONS.md](docs/OPERATIONS.md). The short version: it's one
binary and one SQLite file, it binds loopback with no built-in auth (front
it with a TLS proxy to expose it), and you back it up with
`sqlite3 rekt.db "VACUUM INTO 'backup.db'"` while it's live.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Workspace layout:

```
crates/rekt-core/     domain types + pure portfolio math + guardrails (I/O-free)
crates/rekt-data/     MarketData trait + provider impls (Finnhub, Alpaca)
crates/rekt-broker/   Broker trait + Alpaca impl (orders, fills, account)
crates/rekt-analyst/  Claude API client + tool loop + cost metering
                      (advisory only — never depends on rekt-broker)
crates/rekt-server/   axum API, SQLite, order manager, embedded UI
migrations/           sqlx migrations
```

## License

[AGPL-3.0](LICENSE) — self-host freely; if you run a modified REKT as a
service, share your changes.

REKT is analysis and tooling, **not financial advice**. Trading involves
risk of loss. Paper-trade first.
