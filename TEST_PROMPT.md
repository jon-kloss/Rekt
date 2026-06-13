# REKT — long-running end-to-end test mission (for a Claude Code CLI agent)

You are a meticulous QA engineer. Your job is to exercise **every** feature of a
self-hosted web app called **REKT**, end-to-end, through a real browser driven by
**Playwright**, over a long session (several hours), and produce an honest,
evidence-backed report of what works and what doesn't. You run on the same
machine as the app, so you can build it, run it, read its logs, hit its HTTP API,
and drive its UI.

Treat this as adversarial, careful testing — not a demo. If something is broken,
flaky, or merely *looks* right but is wrong under the hood, find it and prove it
with a screenshot, the network response, and/or the server log line. **Never
report a check as passing unless you actually observed it pass.** Honest
degradation ("this needs an API key I don't have, so I skipped it") is a fine
outcome; a fabricated green check is a failure of your job.

---

## 1. What REKT is

REKT ("Real-time Equity & Capital Tracker") is a **single-user, self-hosted Rust
web app** for tracking and **paper-trading** US stocks & ETFs, with an AI analyst.
Stack: Rust (axum 0.8) + SQLite (WAL) + an embedded vanilla-JS single-page UI
(no framework, no CDN). The whole UI is one file compiled into the binary.

Ground truth lives in the repo — **read these first**, they override anything here
if they conflict:
- `README.md` — overview, quickstart, env vars.
- `CLAUDE.md` — hard invariants (read the "Hard invariants" section carefully).
- `docs/OPERATIONS.md` — env var table, security, health endpoint, guardrails.
- `crates/rekt-server/src/main.rs` — the route table (`.route(...)`).
- `crates/rekt-server/assets/index.html` — the entire UI (keyboard map, command
  bar, rendering) is here; grep it when you need exact behavior.

### Architecture (crates)
- `rekt-core` — pure, I/O-free domain logic: money is `rust_decimal::Decimal`
  (never `f64`), portfolio replay, cost basis, taxes (wash sales, Form 8949),
  performance (TWR/IRR), signals (RSI, SMA, drawdown).
- `rekt-data` — market data (Finnhub / Alpaca).
- `rekt-broker` — Alpaca **paper** trading.
- `rekt-analyst` — the AI analyst (Claude). **Advisory only**: it can NEVER place
  orders (enforced both by the crate graph and, for the CLI backend, an empty
  tool allowlist).
- `rekt-server` — axum server, SQLite, the embedded UI, websocket broadcaster.

### Invariants you should actively try to violate (and confirm the app refuses)
1. **Money is decimal, never float** — no rounding drift; cent-exact totals.
2. **Paper vs live segregation** — paper fills must never alter the live
   portfolio (`transactions.mode`).
3. **The analyst never trades** — it only *prefills* an order ticket a human must
   confirm. Confirm there is no path from an analyst recommendation to an
   executed order without a human clicking PLACE.
4. **All server strings rendered into the DOM are HTML-escaped** — try injecting
   `<img src=x onerror=...>` into a transaction note / symbol / alert note and
   confirm it renders as text, never executes.
5. **Honest degradation** — missing data/keys produce clear errors or `—`, never
   fabricated prices or fake values.
6. **Guardrails always apply** to `/api/orders`, regardless of how the ticket was
   populated (manual, command bar, alert draft, or analyst recommendation).

---

## 2. Build & run

```sh
cd <repo root>            # the directory containing Cargo.toml + crates/
git pull                  # make sure you're on the latest

# Gates should be green before you start (sanity check the build):
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Run the server on a dedicated test DB and port so you don't clobber anything:
REKT_DB=/tmp/rekt-test.db REKT_LISTEN=127.0.0.1:7788 cargo run -p rekt-server
# → the UI is at http://127.0.0.1:7788
```

Leave the server running in the background and tail its log — the log is a primary
source of truth (it traces every order, fill, alert, analyst run, and I/O call).
Relaunch on a **fresh** `/tmp/rekt-test.db` whenever you want a clean slate; delete
`*.db`, `*.db-wal`, `*.db-shm` together.

### Keys (optional, but unlock more surface)
The app degrades honestly without keys. Decide per-feature:
- **AI analyst** — works with **no key**: the default backend is the local Claude
  Code CLI (`claude -p`). Just ensure `claude` is on PATH (it is, since you're a
  Claude Code agent). Health will show `ai_analyst: true`. (Set
  `REKT_ANALYST_BACKEND=http` + `ANTHROPIC_API_KEY` only if you want to test the
  API backend too.)
- **Live quotes + daily bars + charts + price/drawdown alerts** — need
  `FINNHUB_API_KEY` (free at finnhub.io). Without it, positions show `—` for
  price and the candle chart / NET LIQ / donut are sparse. Get a free key if you
  want to test the pricing/alerts/chart paths fully.
- **Paper order placement** — needs `ALPACA_PAPER_KEY` + `ALPACA_PAPER_SECRET`
  (free **paper** account at alpaca.markets — *paper only, never live-money
  keys*). Without them, the order ticket math + guardrail warnings still work
  client-side, but PLACE will return "NO BROKER CONFIGURED".
- **Budget knobs:** `REKT_AI_DAILY_BUDGET` (USD/day, default 2.50) gates analyst
  runs. To run many analyst tests, raise it (e.g. `REKT_AI_DAILY_BUDGET=20`).
  `REKT_AI_AUTO=0` disables the scheduled briefing/review if you want manual
  control.

Set keys in the environment **before** launching; never paste live-money keys,
never log secrets, never commit them.

---

## 3. Playwright setup

Use Playwright (Python or Node — your choice) headless or headed Chromium. If a
browser binary isn't installed, install it (`npx playwright install chromium` or
`playwright install chromium`). Drive `http://127.0.0.1:7788`. For every test:
capture a screenshot, capture relevant network responses, and capture
`console`/`pageerror` events (the page should have **zero** uncaught JS errors —
treat any `pageerror` as a bug).

---

## 4. The UI you're testing

Single page, dark "terminal" theme, **7 tabs** plus a command bar, a KPI strip, an
alert banner, an order-ticket dock, and a status bar. The whole UI re-renders from
app state on each change; data arrives via REST + a websocket (`/api/ws`) that
pushes live portfolio updates.

### Tabs (number keys `1`–`7` switch between them)
1. **Overview** — KPIs (NET LIQ, day P&L, etc.), equity curve, allocation donut.
2. **Positions** — holdings with qty, avg cost, price, P&L, quant signal badges.
3. **Performance** — time-weighted return, IRR, SPY benchmark comparison.
4. **Analyst** — latest briefing/review, recommendations, track record, today's
   AI spend; a **backend badge** that reads `CLAUDE CODE · CLI` or `CLAUDE API`.
5. **Alerts** — price/drawdown alerts; dismiss / re-arm; "review" stages a draft.
6. **Blotter** — transaction + order history.
7. **Taxes** — realized gains, wash sales, Form 8949; year selector; CSV export.

### Keyboard shortcuts (verify each actually works)
- `/` — focus the command bar.
- `1`–`7` — switch tabs.
- `?` — toggle the help overlay.
- `Esc` — close overlay / clear & blur the command bar.
- In Positions: `↑`/`↓` (or `k`/`j`) move the selection; `Enter` loads the
  selected symbol into the ticket.

### Command bar syntax (type into the `/`-focused input, then Enter)
- `b 10 nvda` → stage a **buy** of 10 NVDA (market). `s 5 tsla` → stage a sell.
- `b 10 nvda @ 240` → stage a **limit** buy at 240. (`@ <price>` ⇒ limit.)
- Just a symbol (e.g. `aapl`) → loads that symbol into the ticket.
- Staging only **prefills** the ticket; nothing is placed until you click PLACE.

### Order ticket dock (client-side math + guardrail warnings)
Side (buy/sell), market/limit, qty, limit price. It shows estimated notional and
**guardrail warnings**. Placing posts to `/api/orders`; the server re-checks all
guardrails server-side and the broker (paper) executes the fill.

### Guardrails (server-enforced; defaults — see OPERATIONS.md)
- `REKT_MAX_ORDER_NOTIONAL` = `10000` (per-order $ cap)
- `REKT_MAX_POSITION_PCT` = `25` (max single position as % of equity)
- `REKT_MAX_DAILY_LOSS`, `REKT_MAX_ORDERS_PER_DAY` (rate/loss limits)
Test that exceeding each is **rejected by the API** (not just warned in the UI),
and that the UI surfaces the rejection. Tweak the env values low to trigger them
easily, then restart.

---

## 5. HTTP API (for seeding and for asserting against the UI)

Full route list is in `main.rs`. Key ones:

| Method & path | Purpose |
|---|---|
| `GET /api/health` | health probe (components, db, uptime) |
| `GET /api/portfolio` | live snapshot (positions, cash, equity, watchlist) |
| `GET/POST /api/transactions`, `DELETE /api/transactions/{id}` | ledger |
| `POST /api/import/csv` | CSV import (incl. an IBKR preset) |
| `GET /api/history`, `GET /api/candles` | equity curve, OHLCV |
| `GET /api/taxes`, `GET /api/taxes/csv` | tax report + Form 8949 export |
| `GET/POST /api/watchlist`, `DELETE /api/watchlist/{symbol}` | watchlist |
| `GET/POST /api/alerts`, `DELETE /api/alerts/{id}`, `POST .../dismiss`, `POST .../rearm` | alerts |
| `GET /api/analyst`, `POST /api/analyst/briefing`, `POST /api/analyst/review`, `POST /api/analyst/ask`, `GET /api/analyses/{id}` | AI analyst |
| `POST /api/recommendations/{id}/accept`, `POST .../dismiss` | recs |
| `GET/POST /api/orders`, `DELETE /api/orders/{id}`, `POST /api/orders/cancel_all` | paper orders |
| `POST /api/trading/pause` | halt/resume trading |
| `GET /api/broker/account` | paper account |
| `GET /api/ws` | websocket (live updates) |

### Request body shapes (JSON)
- **Transaction** (`POST /api/transactions`): `{ "kind", "symbol?", "qty?",
  "price?", "fees?", "taxes?", "ts?", "note?" }`. `kind` ∈ `deposit`,
  `withdrawal`, `buy`, `sell`, `dividend`, `split`. Conventions:
  - deposit/withdrawal: cash amount goes in `price` (no symbol).
  - buy/sell: `qty` shares at per-share `price` (symbol required).
  - dividend: total cash received goes in `price` (symbol required).
- **Alert** (`POST /api/alerts`): `{ "symbol", "condition", "threshold",
  "draft_order?", "note?" }`. `condition` ∈ `price_below`, `price_above`,
  `drawdown_above`. `draft_order` (optional) prefills a ticket when it fires.
- **Order** (`POST /api/orders`): `{ "symbol", "side": "buy"|"sell",
  "order_type": "market"|"limit", "qty", "limit_price?", "tif?": "day" }`.

### Suggested seed (a coherent demo portfolio)
Post in this order, then reload the UI:
1. `deposit` `price: 60000`
2. `buy` AAPL `qty: 60` `price: 180`
3. `buy` NVDA `qty: 42` `price: 120`
4. `buy` VOO `qty: 30` `price: 480`
5. `buy` PLTR `qty: 150` `price: 25`
6. `sell` AAPL `qty: 10` `price: 195` (creates a realized gain for the tax tab)
7. `dividend` VOO `price: 86.40`
Then add a few watchlist symbols and a couple of alerts. With a Finnhub key,
prices/charts populate; without, cost-basis/tax/blotter/analyst still work.

---

## 6. Test plan — exercise ALL of this

For each area: drive it through the UI with Playwright, assert the visible result,
cross-check against the API response and the server log, screenshot it, and record
PASS/FAIL with evidence. Where a feature needs a key you don't have, mark it
SKIPPED (with the reason) — don't fake it.

1. **Boot & health** — server starts; `/api/health` is `ok`; the page loads with
   zero console/page errors; the analyst backend badge reflects reality.
2. **Ledger** — create each transaction kind; confirm Blotter + Positions +
   cash/equity update; delete a transaction and confirm the portfolio re-derives
   correctly. Try an **oversell** (sell more than held) and confirm it's rejected
   honestly.
3. **Decimal exactness** — seed prices/qtys with awkward cents; confirm totals are
   cent-exact (no float drift) across Positions, Overview, and Taxes.
4. **CSV import** — import a small CSV (and try the IBKR preset); confirm rows land
   and bad rows are reported by line number, not silently dropped.
5. **Charts & history** — equity curve and candle chart render; hover tooltips
   show correct values; with no/low data they degrade gracefully (no `NaN%` /
   `Infinity%` — specifically hover a chart with sparse data and confirm sane %).
6. **Performance tab** — TWR, IRR, and the SPY benchmark display; sanity-check the
   sign/magnitude against the seeded cash flows.
7. **Watchlist** — add/remove symbols; confirm persistence across reloads.
8. **Alerts** — create price_below/price_above/drawdown_above alerts; confirm
   they evaluate (needs candles/quotes), fire into the alert banner, and that
   dismiss/re-arm work. If an alert has a `draft_order`, confirm "review" stages
   the ticket but **places nothing** automatically.
9. **AI analyst** (works without keys via the CLI backend) — trigger a
   **briefing** and a **weekly review**; confirm they complete, the report
   renders, cost is metered, and the weekly review produces **structured
   recommendations** that appear in the recommendations list. Confirm the report
   is grounded in the actual seeded portfolio and that it is honest about missing
   data (e.g. no web access / no live quotes). Use the "ask" path with a free-form
   question. Confirm a recommendation can be **staged** into the ticket and
   **accept/dismiss** work — but that staging never auto-places an order.
   Test the budget gate: set `REKT_AI_DAILY_BUDGET` very low and confirm runs are
   refused once spent, with the spent tokens still accounted.
10. **Order ticket + guardrails** (needs Alpaca paper keys for actual fills) —
    stage via command bar, via clicking a position, via an alert draft, and via an
    analyst rec; for each, confirm the server applies guardrails. Drive each
    guardrail over its limit and confirm a **server-side rejection** surfaced in
    the UI. Place a valid paper order; confirm the fill appears in Blotter and the
    position updates. Cancel an open order; cancel-all; halt trading and confirm
    placing is blocked, then resume.
11. **Paper/live segregation** — confirm paper fills are tagged paper and don't
    corrupt the manually-entered ("live") holdings or the tax lots.
12. **Taxes** — switch tax years; confirm realized gains, short/long-term split,
    **wash-sale** handling (sell at a loss then rebuy within 30 days and confirm
    the disallowed loss + basis adjustment), and the **Form 8949 CSV export**
    downloads and matches the on-screen figures.
13. **XSS / escaping** — put `<script>`/`<img onerror>` payloads in notes,
    symbols, alert notes, and an analyst "ask" prompt; confirm they render as
    inert text everywhere and never execute (no `pageerror`, no alert dialog).
14. **Websocket liveness** — with the page open, mutate state via the API in a
    second client and confirm the open UI updates without a manual reload.
15. **Resilience over time** — keep the server + a browser session running for the
    full window. Periodically: re-trigger analyst runs, fire alerts, place/cancel
    orders, reload the page, and re-assert state is consistent. Watch the server
    log for panics, leaked tasks, climbing memory, wedged single-flight latches,
    or per-tick DB query storms (the broadcaster path should NOT query the DB per
    tick). Note anything that degrades after hours that was fine at minute one.

---

## 7. Reporting

Keep a running log as you go (a markdown file). For every check record: area,
steps, expected, observed, PASS/FAIL/SKIPPED, and links to screenshots / saved
network responses / log excerpts. At the end produce a summary with: total
pass/fail/skip counts, every defect with a concrete repro, severity, and (where
you can see it) the likely code location. Call out anything that *looked* fine in
the UI but was wrong underneath. Do **not** modify the app's source to make a test
pass; your job is to find and document, not to fix (unless the user later asks).

Be skeptical, be thorough, and prove everything you claim.
