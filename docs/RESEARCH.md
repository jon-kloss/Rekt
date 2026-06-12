# Open-Source Trading & Portfolio Platform Research

> Deep research conducted June 2026 to inform REKT's design. Five parallel
> research threads: portfolio trackers, trading bot platforms, institutional
> engines, the Rust/Alpaca/AI ecosystem, and community demand + licensing.
> Claims are cited inline; claims that could only be sourced from search
> snippets (not a fetched page) are marked *[snippet]*.

## 1. Executive summary — the gap REKT sits in

**No open-source self-hosted tool both tracks and trades.** Ghostfolio,
Wealthfolio, and Maybe/Sure are read-only trackers; Freqtrade, Hummingbot,
and friends are crypto-only bots; StockSharp does US equities but is a heavy
Windows/.NET platform. The community demand signals (below) put *live broker
sync + real order execution* as the #1–2 unmet wants, and a paid SaaS
([AllInvestView](https://www.allinvestview.com/articles/ghostfolio-vs-allinvestview/))
exists specifically to sell "Ghostfolio + Alpaca + US tax lots" — proof the
gap is real and monetizable. AI portfolio analysis is requested
([Ghostfolio discussion #6434](https://github.com/ghostfolio/ghostfolio/discussions/6434),
March 2026) and implemented by **zero** incumbent trackers.

REKT's tracker + trader + AI-analyst combination, as a single Rust binary
(itself a praised differentiator — see Wealthfolio), targets precisely this
hole.

## 2. Landscape survey

| Project | What | Lang | License | Stars (≈6/2026) | Status | Trades? |
|---|---|---|---|---|---|---|
| [Ghostfolio](https://github.com/ghostfolio/ghostfolio) | Web wealth dashboard | TS (Angular/NestJS/Postgres) | AGPL-3.0 | 8.7k | Active, premium-cloud funded | No |
| [Wealthfolio](https://github.com/wealthfolio/wealthfolio) | Local-first tracker | Rust+Tauri/React, SQLite | AGPL-3.0 | 7.6k | Very active | No |
| [Maybe Finance](https://github.com/maybe-finance/maybe) | Personal finance app | Ruby/Rails | AGPL-3.0 | 54k | **Archived 7/2025**; fork: [Sure](https://github.com/we-promise/sure) | No |
| [Portfolio Performance](https://github.com/portfolio-performance/portfolio) | Desktop perf analytics | Java | EPL-1.0 | 3.9k | Very active (270 releases) | No |
| [Freqtrade](https://github.com/freqtrade/freqtrade) | Crypto bot | Python | GPL-3.0 | 51k | Very active | Crypto only |
| [Hummingbot](https://github.com/hummingbot/hummingbot) | Market-making bot | Python | Apache-2.0 | 19k | Active | Crypto only |
| [StockSharp](https://github.com/StockSharp/StockSharp) | Trading platform suite | C# | Apache-2.0 | 10k | Active | Yes (incl. Alpaca) |
| [NautilusTrader](https://github.com/nautechsystems/nautilus_trader) | Institutional engine, **Rust core** | Rust+Python | LGPL-3.0 | 23k | Very active | Yes (no Alpaca adapter) |
| [QuantConnect Lean](https://github.com/QuantConnect/Lean) | Algo engine | C# | Apache-2.0 | 20k | Active | Yes |
| [barter-rs](https://github.com/barter-rs/barter-rs) | Rust trading framework | Rust | MIT | 2.2k | Active, "educational only" | Framework |
| [apca](https://github.com/d-e-s-o/apca) | Alpaca Rust client | Rust | **GPL-3.0** | 195 | Mature, slow cadence | Client lib |
| [ccxt](https://github.com/ccxt/ccxt) | Exchange abstraction | Multi | MIT | 43k | Very active | Crypto only |
| [ai-hedge-fund](https://github.com/virattt/ai-hedge-fund) | LLM investor agents | Python | MIT | 60k | Active | **Simulation only** |
| [TradingAgents](https://github.com/TauricResearch/TradingAgents) | LLM agent debate | Python | Apache-2.0 | 85k | Active | **Simulation only** |

## 3. Architecture patterns to adopt (from the ones that do it right)

### 3.1 Order state machine — model the in-flight states

NautilusTrader's Rust core models **14 order states**
([enums.rs](https://raw.githubusercontent.com/nautechsystems/nautilus_trader/develop/crates/model/src/enums.rs)):
`INITIALIZED → SUBMITTED → ACCEPTED → TRIGGERED / PENDING_UPDATE /
PENDING_CANCEL / PARTIALLY_FILLED → FILLED / CANCELED / REJECTED / EXPIRED`
(+ `DENIED`, `EMULATED`, `RELEASED`). The non-obvious lesson: **explicit
in-flight mutation states** (`PENDING_CANCEL`, `PENDING_UPDATE`) prevent
races where a fill arrives while a cancel is in flight. Alpaca's
`trade_updates` stream maps cleanly onto this
([event taxonomy](https://docs.alpaca.markets/us/docs/websocket-streaming):
`new`, `accepted`, `pending_new`, `fill`, `partial_fill`, `canceled`,
`pending_cancel`, `replaced`, `order_cancel_rejected`,
`order_replace_rejected`, `rejected`, `done_for_day`, `stopped`).

**REKT decision:** model the order lifecycle as a Rust enum covering the
full set including pending-mutation states; exhaustive `match` everywhere.

### 3.2 Deterministic client order IDs — a correctness invariant

NautilusTrader's production bug
([issue #3176](https://github.com/nautechsystems/nautilus_trader/issues/3176)):
generating *random* UUIDs during reconciliation caused duplicate orders to
accumulate on every restart. Alpaca supports `client_order_id` (≤128 chars)
as an idempotency key, though forum reports say dedupe isn't 100% guaranteed
in edge cases ([forum](https://forum.alpaca.markets/t/idempotency-on-order-create/15801)).

**REKT decision:** client order IDs are deterministic (derived from intent +
sequence, persisted *before* submission), and the venue↔client ID mapping is
stored bidirectionally.

### 3.3 Startup reconciliation — "the hardest part and the most skipped"

NautilusTrader's startup sequence
([execution engine](https://raw.githubusercontent.com/nautechsystems/nautilus_trader/develop/crates/execution/src/engine/mod.rs)):
(a) load cached order/position state, (b) query the broker for a **mass
status** of orders/positions/fills, (c) diff, materialize externally-placed
orders with client IDs *derived from venue IDs*, (d) skip fill reports
already covered by materialization, (e) reject any fill that would
double-count or overfill. Freqtrade's equivalent re-fetches each stored
order on restart and has known lag issues
([issue #2021](https://github.com/freqtrade/freqtrade/issues/2021)).

**REKT decision:** a reconciliation phase gates trading at startup — **no
order submission until `reconciled == true`**. Same routine runs after any
websocket gap: never trust local state across a disconnect; REST-reconcile
first (the Freqtrade/Hummingbot fallback pattern,
[Hummingbot architecture](https://hummingbot.org/blog/hummingbot-architecture---part-1/)).

### 3.4 Fill deduplication by execution ID

Alpaca explicitly makes the consumer responsible for idempotent processing;
duplicate events are guaranteed on reconnect
([alpaca-trade-api stream docs](https://deepwiki.com/alpacahq/alpaca-trade-api-python/3.2-trading-updates-stream)).
Hummingbot's `ClientOrderTracker`/`InFlightOrder` accumulates
`executed_amount` keyed by client order ID
([connector architecture](https://hummingbot.org/connectors/connectors/architecture/)) —
and its partial-fill bugs ([#5170](https://github.com/hummingbot/hummingbot/issues/5170),
[#5037](https://github.com/hummingbot/hummingbot/issues/5037)) show this is
where implementations rot.

**REKT decision:** fills are persisted in their own table keyed by a
**unique `execution_id`**; transaction ingestion is idempotent on it.
Partial fills accumulate; a canceled-after-partial-fill order ends as
`partially_filled`+`canceled`, not naive `canceled`
([Alpaca forum](https://forum.alpaca.markets/t/partially-filled-order-returned-as-canceled-in-get-all-orders-api/18610)).

### 3.5 Paper/live parity through one trait

The strongest pattern in the space, three independent confirmations:
StockSharp's `IConnector` (same strategy code, emulated or live —
[docs](https://doc.stocksharp.com/topics/StrategyTesting.html)), Lean's
`IBrokerageModel` (fee/fill/buying-power models behind one interface —
[IBrokerageModel.cs](https://raw.githubusercontent.com/QuantConnect/Lean/master/Common/Brokerages/IBrokerageModel.cs)),
NautilusTrader (sim vs live adapter swapped at kernel startup). REKT's
`Broker` trait is exactly this; Alpaca makes it easy since paper and live
are API-identical.

### 3.6 Protections as an always-on subsystem (not strategy logic)

Freqtrade's most copied feature: `StoplossGuard` (halt after N losses in a
window), `MaxDrawdown` breaker, `CooldownPeriod`, plus
`cancel_open_orders_on_exit`
([protections docs](https://raw.githubusercontent.com/freqtrade/freqtrade/develop/docs/includes/protections.md)).
These live *outside* strategy code and are checked unconditionally on the
order-submission path.

**REKT decision:** guardrails are a standalone module with a global
`trading_paused` flag checked on every submission; add a loss-streak /
drawdown circuit breaker to the existing max-notional/position-cap list.

## 4. Correctness lessons (portfolio accounting)

1. **Transactions as source of truth is the converged design** — Wealthfolio
   ships 14 canonical activity types incl. SPLIT, TRANSFER, FEE, TAX
   ([activity-types.md](https://github.com/wealthfolio/wealthfolio/blob/main/docs/activities/activity-types.md));
   beancount gets correctness from double-entry. REKT's design is validated.
2. **FIFO-only is the top international complaint** — Wealthfolio's most
   active issue requests average-cost/LIFO
   ([#771](https://github.com/wealthfolio/wealthfolio/issues/771)), Canadian
   ACB unsupported ([#862](https://github.com/wealthfolio/wealthfolio/issues/862)).
   Cost basis must be a swappable strategy even if v1 ships FIFO only.
3. **Separate TAX from FEE** — Ghostfolio conflates them on sells and can't
   produce clean P&L/tax reports
   ([#3900](https://github.com/ghostfolio/ghostfolio/issues/3900)).
4. **Splits must be explicit transactions** — beancount's community learned
   that price-history-only split handling causes silent errors
   ([forum](https://groups.google.com/g/beancount/c/-z9_1nR0Wak) *[snippet]*).
5. **Performance metrics are the differentiator power users love** —
   Portfolio Performance's reputation rests on TTWROR vs IRR done right,
   currency gains separated from capital gains
   ([project](https://github.com/portfolio-performance/portfolio)). Ship
   time-weighted *and* money-weighted returns, and separate realized /
   unrealized / dividend / fee components.
6. **Never depend on one price source** — Yahoo Finance breakage is the
   chronic top complaint across Ghostfolio
   ([#6314](https://github.com/ghostfolio/ghostfolio/issues/6314),
   [#6188](https://github.com/ghostfolio/ghostfolio/issues/6188)) and
   Portfolio Performance. REKT's `MarketData` trait + local candle cache +
   two providers is the right call; add data-freshness indicators in the UI.

## 5. Community demand → REKT coverage map

Top-10 wants distilled from GitHub issues, r/selfhosted, r/algotrading, HN:

| # | Want | REKT status |
|---|---|---|
| 1 | Live broker API sync (read + execute) | ✅ Core (Phase 2, Alpaca) |
| 2 | Real order execution from the tool | ✅ Core (Phase 2) — **zero OSS trackers have this** |
| 3 | US-grade tax lots (FIFO/LIFO/SpecID, wash sales, Schedule D) | ⚠️ Lots engine yes; wash-sale detection + Schedule D export → backlog (post-v1) |
| 4 | Reliable price data, not Yahoo | ✅ Finnhub+Alpaca traits, cache, freshness UI |
| 5 | Robust CSV import (plugin-style presets) | ✅ Phase 4 (generic in MVP) |
| 6 | AI/LLM portfolio analysis | ✅ Phase 5 — first mover among self-hosted tools |
| 7 | Options tracking | ❌ Explicit non-goal v1; schema shouldn't preclude |
| 8 | Single binary, no Docker/Postgres stack | ✅ Core design (Rust + SQLite + rust-embed) |
| 9 | Backtesting / strategy simulation | ❌ Out of scope (paper mode covers "try before live") |
| 10 | Multi-account aggregation / net worth | ⚠️ Track-only manual accounts partially cover; full net-worth → backlog |

## 6. AI analyst — prior art and failure modes

The two giant LLM-trading projects —
[ai-hedge-fund](https://github.com/virattt/ai-hedge-fund) (60k stars) and
[TradingAgents](https://github.com/TauricResearch/TradingAgents) (85k stars)
— are both **simulation-only** with heavy disclaimers. The academic
literature documents why
([survey](https://arxiv.org/html/2602.14233v1)): **look-ahead bias** (LLMs
"recall" future events from training data), **data snooping/survivorship
bias** in backtest claims, and **overconfident hallucination** (models
rewarded for specific answers over "I don't know"). The patterns that work:
fetch structured data deterministically *first*, have the LLM reason over a
pre-validated snapshot, log the full decision trail, and frame output as
analysis/explanation rather than prediction.

This independently validates REKT's design: deterministic quant signals as
inputs, citation-required grounding, tool-call audit logs, no-prediction
prompt rules, and no code path from the LLM to execution. TradingAgents'
"persistent decision log with outcome-grounded reflections" is worth
borrowing later: track recommendation outcomes and feed them back into
future analyses.

## 7. Licensing

| Event | Lesson |
|---|---|
| OpenBB: MIT → AGPL ([announcement](https://openbb.co/blog/license-change-openbb-platform-goes-agpl)) | Permissive licensing invited SaaS freeriding; switched to AGPL to force hosted forks to share source |
| Maybe Finance: AGPL → archived → community fork "Sure" lives on | AGPL is what let the community survive the company |
| Quantopian shutdown 2020 ([wiki](https://en.wikipedia.org/wiki/Quantopian)) | Cloud-only platform died, users lost everything except the open-sourced Zipline engine |
| Gekko archived when sole maintainer left | Bus-factor risk; license must permit forks |
| Lean (Apache-2.0): 180+ contributors, enterprise adoption | Permissive maximizes adoption but allows un-contributed hosted clones |

**Decision: AGPL-3.0.** It's the norm for this exact niche (Ghostfolio,
Wealthfolio, Sure, OpenBB), blocks hosted-clone freeriding, guarantees
fork-survival, and leaves a dual-license path open later. Side effect: the
GPL-3.0 [`apca`](https://github.com/d-e-s-o/apca) crate is license-compatible
with AGPL-3.0, but we'll still write our own thin Alpaca client (it's a small
API surface, and owning the order-lifecycle code is the point) and treat
`apca` + barter-rs (MIT, self-described "educational") as reference code.

## 8. Resulting changes to PLAN.md

1. License decision: **AGPL-3.0** (new row in product decisions).
2. Order lifecycle: full state machine incl. `PENDING_CANCEL`/
   `PENDING_REPLACE`; deterministic client order IDs persisted pre-submission.
3. New `fills` table keyed by unique `execution_id`; transaction ingestion
   idempotent on it (replaces bare `broker_fill_id` column).
4. Startup/gap reconciliation gates trading: no submissions until reconciled.
5. Guardrails gain a loss-streak/drawdown circuit breaker + global pause flag.
6. Transactions schema separates `fees` and `taxes`.
7. Cost basis explicitly a strategy trait (FIFO v1; average/specific-lot/ACB
   as additional impls).
8. Phase 3 adds TWR + IRR (money-weighted) performance metrics and
   data-freshness indicators.
9. Backlog (post-v1, demand-validated): wash-sale detection, Schedule D
   export, options support, full net-worth aggregation, recommendation
   outcome tracking.
