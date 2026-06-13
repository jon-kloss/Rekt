# REKT — Real-time Equity & Capital Tracker

> A self-hosted, single-user web dashboard for tracking **and trading** a
> stocks & ETFs portfolio in real time, with an AI analyst watching over your
> shoulder. Rust backend, browser frontend, runs on a laptop or home server.
> Find out exactly how rekt you are, live — and do something about it.

## 1. Vision

REKT answers four questions at a glance, with live data during market hours,
and lets you act on the answers without leaving the dashboard:

1. **What am I worth right now?** — total portfolio value, ticking in real time.
2. **How rekt am I today?** — day P&L, unrealized/realized gains per position.
3. **How did I get here?** — historical equity curve, cost basis, allocation.
4. **What should I consider doing next?** — quant signals + an AI analyst
   that synthesizes news, portfolio state, and market context into
   recommendations with reasoning.
5. **Do something about it** — place, monitor, and cancel orders from the
   dashboard; recommendations and alerts arrive as pre-staged order tickets.

Non-goals (deliberately out of scope):

- No algorithmic/bot trading, and **the AI never executes trades** — every
  order requires a human click on a confirm button, no exceptions.
- No multi-tenant auth, billing, or SaaS plumbing. One user, one process.
- No options/futures/crypto in v1 (the data model shouldn't preclude them later).
- No margin or short selling in v1 — long-only, cash account semantics.
- No pretense of price prediction — the AI synthesizes and reasons; it does
  not see the future, and the UI never implies otherwise.
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
| Analysis | **Quant signals (local) + Claude API analyst** | Deterministic indicators computed in Rust for free; Claude for news synthesis, portfolio review, and recommendations with reasoning |
| License | **AGPL-3.0** | The norm in this niche (Ghostfolio, Wealthfolio, OpenBB); blocks hosted-clone freeriding, guarantees fork-survival; dual-license path stays open. See docs/RESEARCH.md §7 |

> **Competitive positioning** (research: `docs/RESEARCH.md`): no open-source
> self-hosted tool both tracks *and* trades — the top-2 community wants. AI
> portfolio analysis is requested and implemented by zero incumbents. REKT's
> tracker+trader+analyst in a single binary sits exactly in that gap.

## 3. Market data strategy

"Real-time" lives or dies on the data feed. Findings from research (June 2026):

| Provider | Free real-time? | Streaming | Limits | Notes |
|---|---|---|---|---|
| **Finnhub** | Yes (US stocks) | WebSocket trades | 60 REST calls/min | Generous free quota, <100ms latency; also free company news |
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
- **Company news** → Finnhub's free news endpoint feeds the AI analyst.
- **Market calendar/hours** → don't burn quota nights and weekends; gate
  order tickets with honest "market closed" state.

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
3. **Idempotency.** Every order carries a **deterministic** locally-generated
   `client_order_id` (derived from intent + sequence, persisted *before*
   submission — never a random UUID at retry time; see NautilusTrader bug
   #3176 in docs/RESEARCH.md). Venue↔client ID mapping stored bidirectionally.
4. **Guardrails in config.** Max order notional, max position % of portfolio,
   daily order count cap, **loss-streak / drawdown circuit breaker**
   (Freqtrade's `StoplossGuard` pattern). Guardrails are a standalone module
   with a global `trading_paused` flag checked unconditionally on the
   submission path — never strategy-adjacent code.
5. **Kill switch.** One button: cancel all open orders.
6. **Long-only enforcement** in v1: sells are capped at held quantity.
7. **Reconciliation gates trading.** On startup and after any websocket gap:
   load cached state → fetch broker mass status (orders/positions/fills) →
   diff, materialize externally-placed orders (client IDs derived from venue
   IDs), flag drift. **No order submission until `reconciled == true`.**
   Never trust local state across a disconnect.

### Order lifecycle

Orders move through an explicit state machine (a Rust enum, exhaustively
matched): `Draft → Submitted → Accepted → PartiallyFilled → Filled /
Canceled / Rejected / Expired`, **plus in-flight mutation states
`PendingCancel` and `PendingReplace`** — the states that prevent races when
a fill arrives while a cancel is in flight (NautilusTrader models 14 states
for this reason). Alpaca's `trade_updates` events (`new`, `fill`,
`partial_fill`, `pending_cancel`, `replaced`, `order_cancel_rejected`, …)
map onto these transitions. Fills are deduplicated by Alpaca's unique
execution ID — duplicate events are guaranteed on stream reconnect, and the
consumer owns idempotency.

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
auto-executes — confirmation is still human. AI recommendations reuse this
exact pipeline (see §5).

## 5. Analysis & AI recommendations

Two layers with a hard boundary between them: **deterministic quant signals
computed locally in Rust** (facts), and an **AI analyst powered by the Claude
API** (synthesis and judgment). The UI always distinguishes which is which.

### Layer 1 — Quant signal engine (rekt-core, pure Rust, free)

Computed from cached candles + live quotes on every relevant tick/close:

- Trend: SMA/EMA (20/50/200) positions and crossovers; 52-week high/low distance.
- Momentum: RSI(14), rate of change.
- Risk: max drawdown, realized volatility, position concentration (% of
  portfolio per name/sector), cash drag.
- Relative: performance vs cash-flow-matched SPY benchmark.

These are cheap, testable, and honest — they state what *is*, not what will
be. They render as badges/sparklines on positions and feed the AI as inputs.

### Layer 2 — AI analyst (Claude API)

No official Rust SDK exists, so `rekt-analyst` speaks the Messages API over
raw HTTP via `reqwest` — it's a clean JSON API and we already depend on
reqwest. The analyst is an **agentic tool-use loop**: Claude is handed tools
and iterates (call tool → get result → reason → call next) until it produces
a structured report.

**Tools we expose to Claude (read-only, all local):**

| Tool | Returns |
|---|---|
| `get_portfolio` | positions, cost basis, P&L, cash, allocation, quant signals |
| `get_quote` / `get_candles` | current + historical prices for any symbol |
| `get_news` | recent company news headlines (Finnhub free endpoint) |
| `get_transactions` | recent activity, realized P&L history |
| **`web_search` (server-side)** | Anthropic-hosted web search — Claude researches market context, macro events, analyst commentary itself; no scraping code on our side |

Claude never gets a tool that mutates anything. The analyst cannot place,
cancel, or stage orders directly — it can only *return a report*.

**Products of the analyst:**

1. **Morning briefing** (scheduled, pre-market): cheap model summarizes
   overnight news on held symbols, flags today's earnings/events, notes
   signal changes. Lands as a card on the dashboard.
2. **Weekly deep review** (scheduled, weekend): strong model with web search
   does a full portfolio review — concentration risks, thesis check per
   position, what the quant signals say, benchmark comparison — and emits
   recommendations.
3. **On-demand**: "analyze my portfolio" button and a per-position "what's
   going on with NVDA?" deep dive; ad-hoc chat panel grounded in the same tools.

**Recommendations** are structured records, not chat prose: action
(buy/sell/trim/hold/watch), instrument, sizing suggestion, rationale,
the data it relied on (tool calls are logged with the report), confidence,
and an expiry. Each renders as a card with full reasoning expandable, and
carries an optional **pre-staged order ticket that flows into the existing
alerts-to-action confirm pipeline** — accepting a recommendation is still a
human clicking a confirm button on a normal order ticket. We use the API's
structured-output mode (`output_config.format` with a JSON schema) so
recommendation records parse reliably.

**Model tiering & cost** (current API pricing, June 2026):

| Job | Model | Pricing (in/out per MTok) | Est. cost |
|---|---|---|---|
| Morning briefing, news summarization | `claude-haiku-4-5` | $1 / $5 | ~pennies/day |
| Weekly deep review, on-demand analysis, chat | `claude-opus-4-8` (adaptive thinking + web search) | $5 / $25 | ~$0.50–$1.50 per deep run |

A realistic month (daily briefings + weekly deep reviews + occasional
on-demand) lands in the **single dollars**. Cost controls: a configurable
monthly API budget with usage tracked per run, and **prompt caching** —
the system prompt + tool definitions are stable and cached
(`cache_control: ephemeral`; note Opus 4.8 needs a ≥4096-token prefix to
cache, which our tool schemas + instructions will exceed), so repeat runs pay
~10% of input price on the static prefix.

### Honesty rails (the AI equivalent of the trading safety rails)

- **No prediction theater.** The system prompt forbids price targets and
  certainty language; recommendations must be framed as reasoning about
  risk/positioning, and the UI labels them "analysis, not financial advice."
- **Grounding required.** Every claim in a recommendation must trace to a
  tool result (news item, signal, web source); the report stores citations
  and we render them. A recommendation with no supporting tool calls is
  rejected at ingestion.
- **AI output is quarantined from execution.** Recommendations create
  *draft* tickets only, subject to the same guardrails (max notional,
  position caps) as manual orders. There is no code path from Claude's
  output to Alpaca that doesn't pass through a human confirm.
- **Budget cap.** When the monthly API budget is exhausted, scheduled runs
  pause and say so, instead of silently degrading.

## 6. Architecture

```
                  ┌────────────────────────────────────────────────┐
                  │               rekt (single binary)              │
                  │                                                │
  Finnhub WS ────▶│  ingest task ──▶ price cache (in-mem)          │
  Finnhub REST ──▶│  backfill/news ─▶ SQLite (sqlx)                │
                  │                      │                         │
  Alpaca trade ──▶│  fill ingest ──▶ transactions ──┐              │
  updates WS      │                                 ▼              │
  Alpaca REST ◀──▶│  order manager ◀── portfolio engine ──▶ quant  │
                  │  (place/cancel,    (positions, P&L,    signals │
                  │   guardrails)       curve, alerts)        │    │
                  │        ▲                                  ▼    │
  Claude API ◀───▶│  AI analyst (tool-use loop, read-only tools,   │
  (Messages,      │   briefings/reviews → recommendation records)  │
   web_search)    │        │                                       │
                  │  axum: REST API + WS broadcast + static UI     │
                  └────────┼────────────────────────────────────────┘
                           ▼
                  Browser SPA (Vite + Svelte + lightweight-charts)
```

### Backend: **axum** on tokio

- Axum is the 2026 pragmatic default: Tokio-team maintained, Tower middleware,
  first-class websocket support. Actix wins ~10-15% throughput at thousands of
  connections — irrelevant for one browser tab on a LAN.
- Core crates: `axum`, `tokio`, `sqlx` (SQLite), `tokio-tungstenite` (upstream
  feed clients), `reqwest` (Alpaca REST + Claude API), `serde`, `rust_decimal`
  (money is never `f64`), `chrono-tz` (market hours are America/New_York,
  always), `tracing`.

### Storage: **SQLite** via sqlx

One file, zero ops, trivially backed up. WAL mode. Compile-time-checked
queries. Postgres would be pure overhead for one user.

### Frontend: **Vite + Svelte SPA**, served by the same binary

- Charting: **TradingView `lightweight-charts`** — purpose-built for financial
  series, tiny, free.
- Embedded into the binary at build time via `rust-embed`: deployment stays
  *one file + one DB*.
- One browser↔backend websocket carries portfolio deltas, order status
  events, and analyst updates. The browser never talks to Finnhub, Alpaca,
  or Anthropic directly — all three API keys stay server-side.

### Secrets

Three keys now: Finnhub, Alpaca (paper + live separately), Anthropic. All in
a `0600` config file or env vars, never logged (`tracing` redaction), never
sent to the browser. The Alpaca live key can move money and the Anthropic key
can spend money — same discipline for both.

### Real-time flow

1. Market-data ingest task holds one upstream websocket for all held +
   watched symbols. Auto-reconnect with backoff; resubscribe on reconnect.
2. Ticks land in an in-memory price cache behind `tokio::sync::watch`.
3. Portfolio engine recomputes derived state (throttled ~1 update/sec),
   evaluates alert conditions, and refreshes quant signals.
4. Order manager mirrors Alpaca order state from `trade_updates`; terminal
   fills become transactions.
5. Axum broadcasts compact JSON deltas (portfolio, orders, alerts,
   briefings) to the browser.
6. Scheduler: EOD portfolio snapshot, candle backfill, broker reconciliation,
   **pre-market briefing run, weekend deep-review run**.

## 7. Data model

**Transactions are the source of truth; everything else is derived.**
Positions, cost basis, realized P&L, and the equity curve are all
recomputable from the transaction log — bugs are fixable retroactively and
imports/fill-ingestion are idempotent.

```sql
instruments   (id, symbol, name, exchange, currency, type)        -- stock|etf
transactions  (id, instrument_id, kind, qty, price, fees, taxes, ts, source, fill_id, note, mode)
              -- mode: paper | live — paper fills NEVER pollute the live portfolio
              -- kind: buy | sell | dividend | split | deposit | withdrawal
              -- source: manual | csv | broker_fill
              -- fees and taxes are SEPARATE columns (Ghostfolio conflates
              --   them and can't do clean tax reporting — issue #3900)
orders        (id, client_order_id, broker_order_id, instrument_id, side,
               order_type, qty, limit_price, stop_price, tif, status,
               filled_qty, avg_fill_price, mode, submitted_ts, updated_ts)
              -- mode: paper | live  — paper history survives mode switches
              -- status: full state machine incl. pending_cancel/pending_replace
fills         (id, execution_id UNIQUE, order_id, qty, price, ts)
              -- raw broker executions; UNIQUE execution_id makes ingestion
              --   idempotent across stream reconnects; fills roll up into
              --   transactions
alerts        (id, instrument_id, condition, threshold, draft_order_json,
               status, triggered_ts)
analyses      (id, kind, model, started_ts, prompt_tokens, output_tokens,
               cost_usd, report_md, tool_log_json, status)
              -- kind: briefing | weekly_review | on_demand | position_dive
recommendations (id, analysis_id, instrument_id, action, sizing, rationale,
               citations_json, confidence, expires_ts, status, draft_order_json)
              -- action: buy | sell | trim | hold | watch
              -- status: open | accepted | dismissed | expired
lots          (derived) -- open tax lots for FIFO cost-basis tracking
snapshots     (date, total_value, cash, invested, realized_pnl, mode)
candles       (instrument_id, date, o, h, l, c, v)                -- cached history
news_cache    (instrument_id, headline, source, url, published_ts)
watchlist     (instrument_id, added_ts)
```

- `deposit`/`withdrawal` track cash, so REKT knows **capital** (money in) vs
  **equity** (market value) — that distinction is the name of the app.
- **Cost basis is a strategy trait**, not a hardcoded rule: FIFO ships in
  v1; average cost, specific-lot, and Canadian ACB are additional impls
  later. (FIFO-only is the single biggest complaint against Wealthfolio —
  it's legally wrong in several jurisdictions.)
- Paper and live activity are segregated by `mode` so paper experiments never
  pollute the real equity curve.
- `analyses.tool_log_json` records every tool call Claude made — full audit
  trail for how a recommendation was derived, plus per-run cost tracking.
- All money columns are integer cents or TEXT decimals — never floats.

## 8. Features by phase

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
- **Proper performance metrics**: time-weighted return (TWR) *and*
  money-weighted return (IRR), with realized / unrealized / dividend / fee
  components separated — the thing power users love Portfolio Performance
  for and every other tracker fumbles.
- Data-freshness indicators on prices (stale-quote honesty).
- **Quant signal engine**: SMA/RSI/drawdown/concentration badges per position.
- Benchmark overlay: "would I be less rekt if I'd just bought SPY?"
  (cash-flow-matched SPY comparison — a signature REKT feature).

### Phase 4 — Alerts & quality of life
- Price/drawdown alerts (web push or ntfy.sh) with **pre-staged order
  tickets** → one-click-confirm execution (alerts-to-action).
- Watchlist quotes; broker CSV importers (Fidelity/Schwab/IBKR) for
  track-only accounts.
- Dark mode by default, obviously. A "REKT meter" easter egg for drawdowns.

### Phase 5 — AI analyst: "What should I do next?"
- `rekt-analyst` crate: Claude Messages API client (reqwest), tool-use loop,
  structured-output recommendation parsing, prompt caching, cost metering.
- Read-only tool suite (`get_portfolio`, `get_news`, candles/quotes,
  server-side `web_search`).
- **Morning briefing** (Haiku) → dashboard card.
- **Weekly deep review** (Opus, adaptive thinking + web search) →
  recommendation cards with citations and expandable reasoning.
- Recommendations → pre-staged tickets through the alerts-to-action pipeline.
- On-demand portfolio/position analysis + grounded chat panel.

### Phase 6 — Taxes: wash sales + Form 8949 / Schedule D
- Per-lot disposal records from the FIFO engine (proceeds allocated pro
  rata across consumed lots, exact-sum remainder correction).
- `rekt-core::taxes`: Form 8949 rows (NY trade dates, short/long split at
  strictly-more-than-one-year, split-preserving holding period) and
  wash-sale detection (±30-day window across year boundaries, sold shares
  excluded as their own replacement, per-share replacement capacity).
- `/api/taxes` + `/api/taxes/csv` (8949-shaped export); dashboard section
  with year picker and honest limitation notes.

### Phase 7 — Wash-sale basis carry-forward + holding-period tacking
- The tax ledger became its own chronological replay (`rekt-core::taxes`):
  TAX basis diverges from book basis once adjustments apply, so the
  portfolio engine stays purely economic.
- Disallowed losses are added to the replacement shares' basis (split into
  adjusted sub-lots when only part of a buy is the replacement; pending
  adjustments applied when a post-sale replacement buy is replayed) and
  the surrendered holding period tacks onto the replacement — deferred,
  not erased, matching broker 1099-B treatment. Conservation tests prove
  the deferred loss re-emerges on the replacement lot's sale.
- Replacement capacities are split-adjusted (pre-split buys scale with
  the ratio), closing the earlier split-scale caveat.

### Phase 8 — Recommendation outcome tracking
- Outcomes are DERIVED, never stored (`rekt-core::outcomes`): baseline =
  first daily close on/after the recommendation's NY date vs the latest
  close, direction-adjusted per action (sell/trim calls are favorable when
  the price falls; hold/watch have no testable direction). No close yet →
  no outcome, honestly.
- Recommended symbols join the candle backfill set, so history arrives
  automatically and outcomes fill in over time.
- The analyst sees its own track record (last 10 calls + hit rate) in
  every run's user turn — volatile data after the cached prefix — and the
  weekly review is told to weigh calls that aged badly.
- UI: outcome column on the recommendations table (colored by verdict,
  not raw direction) + hit rate in the analyst header; `/api/analyst`
  carries per-recommendation outcomes and the aggregate.

### Phase 9 — Interactive Brokers CSV preset
- `import::parse_ibkr`: parses the IBKR Activity Statement (multi-section
  CSV — each row prefixed with its section + row type, each section with
  its own header) into the generic transaction shape.
- Translates Trades (order-level, deduped against per-execution/per-lot
  detail rows), Dividends (ticker pulled from the description), and
  Deposits & Withdrawals; structural sections are ignored; options, forex,
  non-USD rows, and withholding tax are skipped and reported. USD +
  stocks/ETFs only. Rounds out the broker preset set (Fidelity, Schwab,
  IBKR).

### Backlog (post-v1, demand-validated by research)
- Options tracking (expands the locked stocks-&-ETFs scope — needs its own
  planning pass: data sources, OCC symbology, exercise/assignment flows).
- Full net-worth aggregation (other accounts, cash, real estate).
- Additional cost-basis strategies (average, specific-lot, ACB).

## 9. Risks & open questions

- **Real money, real bugs.** The order path gets the highest test bar in the
  codebase: the guardrail/ticket logic lives in pure `rekt-core` functions,
  the Alpaca client is integration-tested against paper, and live mode ships
  only after a multi-week paper soak.
- **AI overtrust.** The biggest product risk in Phase 5 is the user treating
  synthesis as prophecy. Mitigations are structural (citations required,
  no-prediction prompt rules, advisory labeling, human confirm on every
  ticket) — but worth revisiting once real usage exists.
- **Free-tier fragility**: providers nerf free tiers (IEX Cloud died in 2024).
  Mitigated by the trait + two data implementations from day one.
- **Reconciliation drift**: external trades (made in Alpaca's own UI),
  partial-fill edge cases, or missed websocket events can desync local state.
  The reconciliation job + `broker_fill_id` idempotency + loud drift warnings
  are the answer; never silently overwrite the transaction log.
- **PDT rule**: accounts under $25k get flagged for >3 day-trades/5 days.
  Surface Alpaca's day-trade counter in the UI before it bites.
- **API spend control**: the Anthropic key spends real money per call.
  Metered per run in `analyses`, monthly budget cap, caching for the static
  prefix. Scheduled runs skip cleanly when the market was closed.
- **Splits & dividends correctness**: the classic portfolio-tracker bug
  source. Explicit `split` transactions; flag mismatches when candle backfill
  disagrees with computed cost basis.
- **CSV import formats**: deferred to Phase 4; manual entry + generic CSV
  unblock the MVP.

## 10. Proposed repo layout

```
rekt/
├── Cargo.toml            # workspace
├── crates/
│   ├── rekt-server/      # axum app, ws broadcast, scheduler, embedded UI
│   ├── rekt-core/        # domain: portfolio engine, lots, P&L, order
│   │                     #   guardrails, alert rules, quant signals
│   │                     #   (pure, heavily tested)
│   ├── rekt-data/        # MarketData trait + finnhub/alpaca impls, news
│   ├── rekt-broker/      # Broker trait + alpaca impl (orders, fills, account)
│   └── rekt-analyst/     # Claude API client, tool-use loop, briefings,
│                         #   recommendations, cost metering
├── ui/                   # Vite + Svelte + lightweight-charts
├── migrations/
└── PLAN.md
```

`rekt-core` stays I/O-free so the money math, order-safety logic, and signal
math (the parts that must be right) are testable without a network or
database. `rekt-analyst` depends on `rekt-core` for tool implementations but
never on `rekt-broker` — the type system enforces that the AI can't touch
order placement.
