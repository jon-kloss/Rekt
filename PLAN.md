# REKT — Real-time Equity & Capital Tracker

> A self-hosted, single-user web dashboard for tracking a stocks & ETFs
> portfolio in real time. Rust backend, browser frontend, runs on a laptop or
> home server. Find out exactly how rekt you are, live.

## 1. Vision

REKT answers three questions at a glance, with live data during market hours:

1. **What am I worth right now?** — total portfolio value, ticking in real time.
2. **How rekt am I today?** — day P&L, unrealized/realized gains per position.
3. **How did I get here?** — historical equity curve, cost basis, allocation.

Non-goals (deliberately out of scope):

- No trading/order execution — this is a tracker, not a broker.
- No multi-tenant auth, billing, or SaaS plumbing. One user, one process.
- No options/futures/crypto in v1 (the data model shouldn't preclude them later).
- No tax-form generation (we track lots well enough to add it later).

## 2. Product decisions (locked in)

| Decision | Choice | Rationale |
|---|---|---|
| Asset classes | US stocks & ETFs | Free real-time data exists for US equities; everything else costs money |
| Form factor | Web dashboard | Live-updating charts, easy to open anywhere on the LAN |
| Deployment | Single-user, self-hosted | One binary + one SQLite file, no auth complexity |
| Backend | Rust | User preference; great fit for a long-running streaming daemon |

## 3. Market data strategy (the load-bearing decision)

"Real-time" lives or dies on the data feed. Findings from research (June 2026):

| Provider | Free real-time? | Streaming | Limits | Notes |
|---|---|---|---|---|
| **Finnhub** | Yes (US stocks) | WebSocket trades | 60 REST calls/min | Generous free quota, <100ms latency |
| **Alpaca** | Yes (IEX feed only) | WebSocket | 30 symbols/conn, 1 conn, 200 REST/min | IEX is ~2-3% of volume but quotes track NBBO closely |
| Polygon.io | No (EOD only on free) | Paid | 5 REST calls/min free | Best data, but real-time is paid |
| Twelve Data | Trial only | Paid | 8 REST calls/min free | |
| Alpha Vantage | Delayed | No | 25 REST calls/day | Fine for fundamentals, useless for real-time |

**Decision: pluggable `MarketData` trait with Finnhub as the primary provider,
Alpaca as the second implementation.**

- Finnhub's free websocket streams real US trade data with no symbol cap that
  bites at personal-portfolio scale; REST covers quotes, candles, and company
  profiles for backfill.
- Alpaca needs only a free account (no funded brokerage) and is a clean
  fallback; its 30-symbol cap is fine for most personal portfolios.
- The trait boundary means a future paid upgrade (Polygon) or a scraping
  fallback is a new impl, not a refactor.

Provider-agnostic needs:

- **Live trades/quotes** → websocket subscription per held symbol.
- **Historical daily candles** → backfill the equity curve from first
  transaction date.
- **Symbol search/metadata** → name, exchange, currency for the add-trade UI.
- **Market calendar/hours** → don't burn API quota nights and weekends; show
  "market closed, last close $X" honestly instead of a frozen number.

## 4. Architecture

```
                 ┌────────────────────────────────────────────┐
                 │              rekt (single binary)           │
                 │                                            │
  Finnhub WS ───▶│  ingest task ──▶ price cache (in-mem)      │
  Finnhub REST ─▶│  backfill task ─▶ SQLite (sqlx)            │
                 │                      │                     │
                 │  portfolio engine ◀──┘                     │
                 │   (positions, P&L, equity curve)           │
                 │        │                                   │
                 │  axum: REST API + WS broadcast + static UI │
                 └────────┼───────────────────────────────────┘
                          ▼
                 Browser SPA (Vite + TS + lightweight-charts)
```

### Backend: **axum** on tokio

- Axum is the 2026 pragmatic default: Tokio-team maintained, Tower middleware,
  first-class websocket support. Actix wins ~10-15% throughput at thousands of
  connections — irrelevant for one browser tab on a LAN. Ecosystem alignment
  (sqlx, tokio-tungstenite, tower-http) matters more.
- Core crates: `axum`, `tokio`, `sqlx` (SQLite), `tokio-tungstenite` (upstream
  feed client), `serde`, `rust_decimal` (money is never `f64`), `time` or
  `chrono-tz` (market hours are America/New_York, always), `tracing`.

### Storage: **SQLite** via sqlx

One file, zero ops, trivially backed up. WAL mode. Compile-time-checked
queries. Postgres would be pure overhead for one user.

### Frontend: **Vite + TypeScript SPA**, served by the same binary

- Charting: **TradingView `lightweight-charts`** — purpose-built for financial
  series (candles, area equity curves), tiny, free.
- Framework: Svelte (lean, reactive — a good match for "numbers that tick").
  Embedded into the binary at build time via `rust-embed` so deployment stays
  *one file + one DB*.
- Live updates: a single browser↔backend websocket carrying portfolio-level
  deltas (the backend fans in upstream ticks, recomputes, and pushes derived
  state — the browser never talks to Finnhub directly, keeping the API key
  server-side).

### Real-time flow

1. Ingest task holds one upstream websocket, subscribed to all held symbols
   (+ watchlist). Auto-reconnect with backoff; resubscribe on reconnect.
2. Ticks land in an in-memory price cache (`HashMap<Symbol, Quote>` behind a
   `tokio::sync::watch`/broadcast).
3. Portfolio engine recomputes derived state, throttled to ~1 update/sec max
   (humans can't read faster, and it keeps the browser cheap).
4. Axum broadcasts a compact JSON delta to connected browser sessions.
5. A scheduler task: snapshot portfolio value at market close daily (the
   equity curve's source of truth), backfill candles on startup/new symbol.

## 5. Data model

**Transactions are the source of truth; everything else is derived.** This is
the one architectural hill to die on — positions, cost basis, realized P&L,
and the equity curve are all recomputable from the transaction log, which
makes bugs fixable retroactively and imports idempotent.

```sql
instruments   (id, symbol, name, exchange, currency, type)        -- stock|etf
transactions  (id, instrument_id, kind, qty, price, fees, ts, note)
              -- kind: buy | sell | dividend | split | deposit | withdrawal
lots          (derived) -- open tax lots for cost-basis tracking
snapshots     (date, total_value, cash, invested, realized_pnl)   -- EOD equity curve
candles       (instrument_id, date, o, h, l, c, v)                -- cached history
watchlist     (instrument_id, added_ts)
```

- `deposit`/`withdrawal` track cash, so REKT knows **capital** (money in) vs
  **equity** (market value) — that distinction is the name of the app.
- Cost basis: FIFO lots in v1; the lot engine keeps average-cost and
  specific-lot as future options.
- All money columns are integer cents or TEXT decimals — never floats.

## 6. Features by phase

### Phase 0 — Skeleton (the walking proof)
- Cargo workspace, axum server, SQLite migrations, CI (fmt, clippy, test).
- `MarketData` trait + Finnhub REST impl; fetch one live quote end to end.
- Minimal SPA shell served from the binary.

### Phase 1 — MVP: "How rekt am I?"
- Transaction CRUD (add buy/sell/deposit via UI form + CSV import).
- Positions table: qty, avg cost, live price, market value, day Δ, total Δ.
- Header: total equity, day P&L (green/red — the dopamine number).
- Live ticking via the websocket pipeline; market-closed state handled honestly.

### Phase 2 — History & insight
- Daily snapshot job + candle backfill → **equity curve chart** (1M/1Y/All).
- Per-position detail page: price chart with buy/sell markers overlaid.
- Allocation donut (by position, sector later); realized vs unrealized P&L;
  dividends received.
- Benchmark overlay: "would I be less rekt if I'd just bought SPY?" (a
  signature REKT feature — cash-flow-matched SPY comparison).

### Phase 3 — Quality of life
- Watchlist (quotes for things you don't own yet).
- Price/drawdown alerts (web push or ntfy.sh).
- Broker CSV importers (Fidelity/Schwab/IBKR formats).
- Dark mode by default, obviously. A "REKT meter" easter egg for drawdowns.

## 7. Risks & open questions

- **Free-tier fragility**: providers nerf free tiers (IEX Cloud died in 2024).
  Mitigated by the trait + two implementations from day one.
- **Splits & dividends correctness**: the classic portfolio-tracker bug
  source. Handle via explicit `split` transactions; flag mismatches when
  candle backfill disagrees with computed cost basis.
- **Finnhub websocket symbol limits**: fine at personal scale; if a giant
  watchlist ever exceeds it, fall back to REST polling for watchlist-only
  symbols (60/min is plenty).
- **CSV import formats**: deferred to Phase 3; manual entry + a generic CSV
  shape unblock the MVP.

## 8. Proposed repo layout

```
rekt/
├── Cargo.toml            # workspace
├── crates/
│   ├── rekt-server/      # axum app, ws broadcast, embedded UI
│   ├── rekt-core/        # domain: portfolio engine, lots, P&L (pure, heavily tested)
│   └── rekt-data/        # MarketData trait + finnhub/alpaca impls
├── ui/                   # Vite + Svelte + lightweight-charts
├── migrations/
└── PLAN.md
```

`rekt-core` stays I/O-free so the money math (the part that must be right) is
testable without a network or database.
