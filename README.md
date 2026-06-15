# REKT — Real-time Equity & Capital Tracker

> Find out exactly how rekt you are, live — and do something about it.

A self-hosted, single-user web dashboard for **tracking and trading** a US
stocks & ETFs portfolio in real time, with an AI analyst watching over your
shoulder. One Rust binary, one SQLite file, your keys never leave your box.

### ▶ Live demo — **https://rekt-production-545b.up.railway.app**

A fully working instance with a sample portfolio: real charts and market
gauges, live paper trading, and pre-generated AI analyses. Trading and
watchlists are live; the AI tabs are pre-baked (no spend); it resets to the
sample data every few hours, or hit **↺ reset** in the banner. *(Demo keys are
throwaway; don't enter anything real.)*

![REKT dashboard](docs/img/screenshot.png)

---

## What it does

- **📒 Tracks your real book.** Import your activity history (deposits, buys,
  sells, dividends, splits) from Fidelity, Schwab, Robinhood, IBKR, or a generic
  CSV; REKT replays it into positions, cost basis, realized/unrealized P&L, an
  equity curve vs. SPY, and tax lots. Money is exact decimals, never floats.
- **📈 Paper-trades for real.** A built-in order ticket trades a simulated Alpaca
  paper account (market/limit, basket orders) with guardrails — per-order
  notional cap, max position %, daily order count, and a daily-loss circuit
  breaker. Paper fills are strictly segregated from your tracked book.
- **🤖 An AI analyst on your shoulder — advisory only.** Morning briefings,
  weekly deep reviews (with web search), and on-demand Q&A over your portfolio.
  It **can never place orders**: no code path to the broker, read-only tools,
  and recommendations only *prefill* the guarded order ticket you confirm. Every
  recommendation is **scored against what the price actually did** (direction-
  adjusted), and the analyst sees its own track record before making new calls.
  Runs on the local **Claude Code CLI** by default (reuses your `claude` login —
  no API key), the **Claude API**, or a **local Ollama model** (free, private,
  offline).
- **🔭 Finds ideas across the market.** A deterministic screener ranks buy/sell
  candidates from your **named watchlists** (RSI / SMA distance / drawdown), with
  per-equity-type **aggressiveness** (separate for stocks vs ETFs). The AI then
  *narrates* the ranked candidates — so the picks are trustworthy math, not a
  hallucination, and a small local model is enough. Plus **market gauges**
  (SPY / QQQ / IWM / DIA with live RSI/trend) and a daily **state-of-market
  brief** grounded in those gauges.
- **🔔 Alerts that become actions.** Price-above / price-below / drawdown alerts
  with optional push (ntfy). A triggered alert can pre-stage an order ticket for
  one-click review — it never trades on its own.
- **🧾 Taxes done properly.** Per-lot Form 8949 rows with full **wash-sale**
  treatment (IRC §1091, code W): disallowed losses carry forward into the
  replacement lot's basis with the holding period tacked on, matching broker
  1099-B practice. Schedule D totals and CSV export. *(Same-symbol matching;
  reconcile with your 1099-B; not tax advice.)*
- **🗂 Multiple portfolios.** Keep a `test` book and your `real` data side by
  side, each its own SQLite file; switch from the header.
- **🔒 Yours, honestly.** One process, one file, keys are env-only (never logged
  or sent to the browser). Missing data or keys produce clear errors or `None` —
  **never fabricated values**.

## Quick start

```sh
# market data (free key from https://finnhub.io)
export FINNHUB_API_KEY=your_key

# paper trading + daily candles (free account at https://alpaca.markets —
# paper keys only; live trading is deliberately not wired up)
export ALPACA_PAPER_KEY=your_key
export ALPACA_PAPER_SECRET=your_secret

# AI analyst (advisory only). Defaults to the local Claude Code CLI — it reuses
# the `claude` you've already signed in (must be on PATH), so no API key is
# needed. It runs tool-less by design and can never place orders. Alternatives:
#   export REKT_ANALYST_BACKEND=http   && export ANTHROPIC_API_KEY=your_key
#   export REKT_ANALYST_BACKEND=ollama && ollama pull llama3.1   # free + local

cargo run -p rekt-server
# → http://127.0.0.1:7777
```

Everything degrades honestly: without keys, the affected features answer with
clear errors instead of pretending. Full env-var reference (guardrails, AI
budget, alert push, ports, multiple portfolios) is in
[docs/OPERATIONS.md](docs/OPERATIONS.md).

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

Deployment (systemd or Docker), reverse-proxy + TLS, SQLite backup/restore,
monitoring via `/api/health`, the security posture, and the deliberate
paper-only stance on live trading are all in
[docs/OPERATIONS.md](docs/OPERATIONS.md). The short version: it's one binary and
one SQLite file, it binds loopback with no built-in auth (front it with a TLS
proxy to expose it), and you back it up with
`sqlite3 rekt.db "VACUUM INTO 'backup.db'"` while it's live.

**Host your own public demo** (like the one above) with `REKT_DEMO=1`: the AI
analyst is forced off (pre-baked analyses, no spend), cost-bearing/destructive
routes are blocked, and a baked sample portfolio self-heals on a timer. Deploy
to [Railway](https://railway.app) from the included `Dockerfile` — see the
[Public demo](docs/OPERATIONS.md#public-demo-railway) section.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The toolchain is pinned in `rust-toolchain.toml` so local and CI match. The UI
is a single vanilla-JS file (`crates/rekt-server/assets/index.html`) embedded
into the binary — no build step, no framework.

Workspace layout:

```
crates/rekt-core/     domain types + pure portfolio math + guardrails (I/O-free)
crates/rekt-data/     MarketData trait + provider impls (Finnhub, Alpaca)
crates/rekt-broker/   Broker trait + Alpaca impl (orders, fills, account)
crates/rekt-analyst/  AI client (CLI / API / Ollama) + tool loop + cost metering
                      (advisory only — never depends on rekt-broker)
crates/rekt-server/   axum API, SQLite, order manager, screener, embedded UI
migrations/           sqlx migrations
```

See [PLAN.md](PLAN.md) for the full design and roadmap, and
[docs/RESEARCH.md](docs/RESEARCH.md) for the research behind it.

## License

[AGPL-3.0](LICENSE) — self-host freely; if you run a modified REKT as a
service, share your changes.

REKT is analysis and tooling, **not financial advice**. Trading involves
risk of loss. Paper-trade first.
