# REKT — Real-time Equity & Capital Tracker

> Find out exactly how rekt you are, live — and do something about it.

A self-hosted, single-user web dashboard for **tracking and trading** a US
stocks & ETFs portfolio in real time, with an AI analyst watching over your
shoulder. One Rust binary, one SQLite file, your keys never leave your box.

**Status: Phase 2** — live-ticking portfolio tracking plus **paper trading**
end to end: order tickets with guardrails, real-time order status via
Alpaca's `trade_updates` stream, fills auto-ingested into the transaction
log, startup reconciliation, kill switch. See [PLAN.md](PLAN.md) for the full
design and roadmap, and [docs/RESEARCH.md](docs/RESEARCH.md) for the research
behind it.

## Quick start

```sh
# market data (free key from https://finnhub.io)
export FINNHUB_API_KEY=your_key

# paper trading (free account at https://alpaca.markets — paper keys only;
# live trading is deliberately not wired up yet)
export ALPACA_PAPER_KEY=your_key
export ALPACA_PAPER_SECRET=your_secret

cargo run -p rekt-server
# → http://127.0.0.1:7777
```

Everything degrades honestly: without keys, the affected features answer
with clear errors instead of pretending.

Configuration (env vars): `REKT_DB` (default `rekt.db`), `REKT_LISTEN`
(default `127.0.0.1:7777`), `FINNHUB_API_KEY`, `ALPACA_PAPER_KEY`/`_SECRET`,
and guardrails: `REKT_MAX_ORDER_NOTIONAL` (default 10000),
`REKT_MAX_POSITION_PCT` (default 25), `REKT_MAX_ORDERS_PER_DAY` (default 20).

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Workspace layout:

```
crates/rekt-core/     domain types + pure portfolio math + guardrails (I/O-free)
crates/rekt-data/     MarketData trait + provider impls (Finnhub)
crates/rekt-broker/   Broker trait + Alpaca impl (orders, fills, account)
crates/rekt-server/   axum API, SQLite, order manager, embedded UI
migrations/           sqlx migrations
```

## License

[AGPL-3.0](LICENSE) — self-host freely; if you run a modified REKT as a
service, share your changes.

REKT is analysis and tooling, **not financial advice**. Trading involves
risk of loss. Paper-trade first.
