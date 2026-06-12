# REKT — Real-time Equity & Capital Tracker

> A self-hosted, single-user web dashboard for tracking **and trading** a
> stocks & ETFs portfolio in real time. Rust backend, browser frontend, runs
> on a laptop or home server. Find out exactly how rekt you are, live — and
> do something about it.

## 1. Vision

REKT answers three questions at a glance, with live data during market hours,
and lets you act on the answer without leaving the dashboard:

1. **What am I worth right now?** — total portfolio value, ticking in real time.
2. **How rekt am I today?** — day P&L, unrealized/realized gains per position.
3. **How did I get here?** — historical equity curve, cost basis, allocation.
4. **Do something about it** — place, monitor, and cancel orders from the
   dashboard, including one-click execution from price alerts.

Non-goals (deliberately out of scope):

- No algorithmic/bot trading in v1 — humans click, REKT executes. (The
  `Broker` trait shouldn't preclude it later.)
- No multi-tenant auth, billing, or SaaS plumbing. One user, one process.
- No options/futures/crypto in v1 (the data model shouldn't preclude them later).
- No margin or short selling in v1 — long-only, cash account semantics.
- No tax-form generation (we track lots well enough to add it later).

## 2. Product decisions (locked in)

| Decision | Choice | Rationale |
|---|---|---|
| Asset classes | US stocks & ETFs | Free real-time data exists for US equities; everything else costs money |
| Form factor | Web dashboard | Live-updating charts, easy to open anywhere on the LAN |
| Deployment | Single-user, self-hosted | One binary + one SQLite file, no auth complexity |
| Backend | Rust | User preference; great fit for a long-running streaming daemon |
| Broker | **Alpaca** | API-first, commission-free, free paper-trading sandbox, websocket order updates; doubles as fallback market data |
| Trading mode | **Manual + alerts-to-action** | Order tickets in the UI, plus one-click execution from triggered price alerts. Paper first, live behind an explicit switch |

## 3. Market data strategy

"Real-time" lives or dies on the data feed. Findings from research (June 2026):

| Provider | Free real-time? | Streaming | Limits | Notes |
|---|---|---|---|---|
| **Finnhub** | Yes (US stocks) | WebSocket trades | 60 REST calls/min | Generous free quota, <100ms latency |
| **Alpaca** | Yes (IEX feed only) | WebSocket | 30 symbols/conn, 1 conn, 200 REST/min | IEX is ~2-3% of volume but quotes track NBBO closely; account needed anyway for trading |
| Polygon.io | No (EOD only on free) | Paid | 5 REST calls/min free | Best data, but real-time is paid |
| Twelve Data | Trial only | Paid | 8 REST calls/min free | |
| Alpha Vantage | Delayed | No | 25 REST calls/day | Fine for fundamentals, useless for real-time |

**Decision: pluggable `MarketData` trait — Finnhub primary (better free
real-time breadth), Alpaca second impl.** Since trading requires an Alpaca
account anyway, the fallback feed is guaranteed to exist. The trait boundary
means a future paid upgrade (Polygon) is a new impl, not a refactor.

Provider-agnostic needs:

- **Live trades/quotes** → websocket subscription per held + watched symbol.
- **Historical daily candles** → backfill the equity curve from first
  transaction date.
- **Symbol search/metadata** → name, exchange, currency for order/trade entry.
- **Market calendar/hours** → don't burn quota nights and weekends; gate
  order tickets with honest "market closed" state (or queue as Alpaca
  day orders for the open).

## 4. Execution strategy (trading)

**Decision: a `Broker` trait (mirror of `MarketData`) with Alpaca as the
first implementation.** Alpaca gives us:

- REST order placement: market, limit, stop, stop-limit; day/GTC time-in-force.
- A **paper-trading environment** that is API-identical to live — same code,
  different base URL + keys.
- A `trade_updates` websocket: order accepted → partially filled → filled /
  canceled / rejected, streamed in real time.
- Positions & account endpoints for reconciliation.

### Safety rails (non-negotiable)

1. **Paper by default.** REKT boots in paper mode. Live mode requires live
   keys in config *and* an explicit UI toggle with a scary-red confirm. The
   UI permanently shows which mode you're in (paper = obvious banner).
2. **Confirm-before-send.** Every order shows a ticket summary (est. cost,
   buying power after) and requires confirmation. No double-click surprises.
3. **Idempotency.** Every order carries a locally-generated
   `client_order_id`; retries after a network blip can never double-submit.
4. **Guardrails in config.** Max order notional, max position % of portfolio,
   daily order count cap. Exceeding one blocks the order with an explanation.
5. **Kill switch.** One button: cancel all open orders.
6. **Long-only enforcement** in v1: sells are capped at held quantity.

### Order → portfolio flow

Fills from the `trade_updates` stream are **automatically ingested as
transactions** — trading through REKT means zero manual bookkeeping. The
transaction log stays the local source of truth; a periodic reconciliation
job diffs local positions/cash against Alpaca's account endpoints and flags
drift loudly instead of silently "fixing" it. Manual transactions remain
supported for holdings outside Alpaca (track-only positions).

### Alerts-to-action

Price alerts (e.g. "AAPL ≤ 170") fire a notification with an **arm-able
pre-staged order ticket**: the alert can carry a draft order (side, qty,
type) that triggering surfaces as a one-click-confirm ticket. The alert never
auto-executes — confirmation is still human. (Auto-execution = algo trading
= explicitly post-v1.)

## 5. Architecture

```
                  ┌──────────────────────────────────────────────┐
                  │               rekt (single binary)            │
                  │                                              │
  Finnhub WS ────▶│  ingest task ──▶ price cache (in-mem)        │
  Finnhub REST ──▶│  backfill task ─▶ SQLite (sqlx)              │
                  │                      │                       │
  Alpaca trade ──▶│  fill ingest ──▶ transactions ──┐            │
  updates WS      │                                 ▼            │
  Alpaca REST ◀──▶│  order manager ◀── portfolio engine          │
                  │  (place/cancel,    (positions, P&L, curve,   │
                  │   guardrails)       alert evaluation)        │
                  │        │                  │                  │
                  │  axum: REST API + WS broadcast + static UI   │
                  └────────┼──────────────────────────────────────┘
                           ▼
                  Browser SPA (Vite + Svelte + lightweight-charts)
```

### Backend: **axum** on tokio

- Axum is the 2026 pragmatic default: Tokio-team maintained, Tower middleware,
  first-class websocket support. Actix wins ~10-15% throughput at thousands of
  connections — irrelevant for one browser tab on a LAN.
- Core crates: `axum`, `tokio`, `sqlx` (SQLite), `tokio-tungstenite` (upstream
  feed clients), `serde`, `rust_decimal` (money is never `f64`), `chrono-tz`
  (market hours are America/New_York, always), `tracing`, `reqwest`.

### Storage: **SQLite** via sqlx

One file, zero ops, trivially backed up. WAL mode. Compile-time-checked
queries. Postgres would be pure overhead for one user.

### Frontend: **Vite + Svelte SPA**, served by the same binary

- Charting: **TradingView `lightweight-charts`** — purpose-built for financial
  series, tiny, free.
- Embedded into the binary at build time via `rust-embed`: deployment stays
  *one file + one DB*.
- One browser↔backend websocket carries portfolio deltas **and** order status
  events. The browser never talks to Finnhub or Alpaca directly — both API
  keys (especially the one that can move money) stay server-side.

### Secrets

Alpaca live keys can move real money. Keys live in a config file with `0600`
perms (or env vars), are never logged (`tracing` redaction), never sent to
the browser, and paper/live keys are separate config entries so a typo can't
cross the streams.

### Real-time flow

1. Market-data ingest task holds one upstream websocket for all held +
   watched symbols. Auto-reconnect with backoff; resubscribe on reconnect.
2. Ticks land in an in-memory price cache behind `tokio::sync::watch`.
3. Portfolio engine recomputes derived state (throttled ~1 update/sec) and
   evaluates alert conditions on each tick.
4. Order manager mirrors Alpaca order state from `trade_updates`; terminal
   fills become transactions.
5. Axum broadcasts compact JSON deltas (portfolio, orders, alerts) to the
   browser.
6. Scheduler: EOD portfolio snapshot, candle backfill on startup/new symbol,
   periodic broker reconciliation.

## 6. Data model

**Transactions are the source of truth; everything else is derived.**
Positions, cost basis, realized P&L, and the equity curve are all
recomputable from the transaction log — bugs are fixable retroactively and
imports/fill-ingestion are idempotent.

```sql
instruments   (id, symbol, name, exchange, currency, type)        -- stock|etf
transactions  (id, instrument_id, kind, qty, price, fees, ts, source, broker_fill_id, note)
              -- kind: buy | sell | dividend | split | deposit | withdrawal
              -- source: manual | csv | broker_fill   (broker_fill_id ⇒ idempotent ingest)
orders        (id, client_order_id, broker_order_id, instrument_id, side,
               order_type, qty, limit_price, stop_price, tif, status,
               filled_qty, avg_fill_price, mode, submitted_ts, updated_ts)
              -- mode: paper | live  — paper history survives mode switches
alerts        (id, instrument_id, condition, threshold, draft_order_json,
               status, triggered_ts)
lots          (derived) -- open tax lots for FIFO cost-basis tracking
snapshots     (date, total_value, cash, invested, realized_pnl, mode)
candles       (instrument_id, date, o, h, l, c, v)                -- cached history
watchlist     (instrument_id, added_ts)
```

- `deposit`/`withdrawal` track cash, so REKT knows **capital** (money in) vs
  **equity** (market value) — that distinction is the name of the app.
- Paper and live activity are segregated by `mode` so paper experiments never
  pollute the real equity curve.
- All money columns are integer cents or TEXT decimals — never floats.

## 7. Features by phase

### Phase 0 — Skeleton (the walking proof)
- Cargo workspace, axum server, SQLite migrations, CI (fmt, clippy, test).
- `MarketData` trait + Finnhub REST impl; one live quote end to end.
- Minimal SPA shell served from the binary.

### Phase 1 — MVP tracking: "How rekt am I?"
- Transaction CRUD (manual buy/sell/deposit via UI form + generic CSV import).
- Positions table: qty, avg cost, live price, market value, day Δ, total Δ.
- Header: total equity, day P&L (green/red — the dopamine number).
- Live ticking via the websocket pipeline; market-closed state handled honestly.

### Phase 2 — Trading: "Do something about it"
- `Broker` trait + Alpaca impl (REST orders + `trade_updates` websocket).
- **Paper mode end-to-end**: order ticket UI (market/limit first), live order
  status, cancel, fills auto-ingested as transactions, kill switch.
- Guardrails + confirm flow + mode banner.
- Reconciliation job against Alpaca positions/cash.
- Then: live mode behind the explicit toggle. Stop/stop-limit orders.

### Phase 3 — History & insight
- Daily snapshot job + candle backfill → **equity curve chart** (1M/1Y/All).
- Per-position detail page: price chart with buy/sell markers overlaid.
- Allocation donut; realized vs unrealized P&L; dividends received.
- Benchmark overlay: "would I be less rekt if I'd just bought SPY?"
  (cash-flow-matched SPY comparison — a signature REKT feature).

### Phase 4 — Alerts & quality of life
- Price/drawdown alerts (web push or ntfy.sh) with **pre-staged order
  tickets** → one-click-confirm execution (alerts-to-action).
- Watchlist quotes; broker CSV importers (Fidelity/Schwab/IBKR) for
  track-only accounts.
- Dark mode by default, obviously. A "REKT meter" easter egg for drawdowns.

## 8. Risks & open questions

- **Real money, real bugs.** The order path gets the highest test bar in the
  codebase: the guardrail/ticket logic lives in pure `rekt-core` functions,
  the Alpaca client is integration-tested against paper, and live mode ships
  only after a multi-week paper soak.
- **Free-tier fragility**: providers nerf free tiers (IEX Cloud died in 2024).
  Mitigated by the trait + two data implementations from day one.
- **Reconciliation drift**: external trades (made in Alpaca's own UI),
  partial-fill edge cases, or missed websocket events can desync local state.
  The reconciliation job + `broker_fill_id` idempotency + loud drift warnings
  are the answer; never silently overwrite the transaction log.
- **PDT rule**: accounts under $25k get flagged for >3 day-trades/5 days.
  Surface Alpaca's day-trade counter in the UI before it bites.
- **Splits & dividends correctness**: the classic portfolio-tracker bug
  source. Explicit `split` transactions; flag mismatches when candle backfill
  disagrees with computed cost basis.
- **CSV import formats**: deferred to Phase 4; manual entry + generic CSV
  unblock the MVP.

## 9. Proposed repo layout

```
rekt/
├── Cargo.toml            # workspace
├── crates/
│   ├── rekt-server/      # axum app, ws broadcast, embedded UI
│   ├── rekt-core/        # domain: portfolio engine, lots, P&L, order
│   │                     #   guardrails, alert rules (pure, heavily tested)
│   ├── rekt-data/        # MarketData trait + finnhub/alpaca impls
│   └── rekt-broker/      # Broker trait + alpaca impl (orders, fills, account)
├── ui/                   # Vite + Svelte + lightweight-charts
├── migrations/
└── PLAN.md
```

`rekt-core` stays I/O-free so the money math and the order-safety logic (the
parts that must be right) are testable without a network or database.
