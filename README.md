# REKT — Real-time Equity & Capital Tracker

> Find out exactly how rekt you are, live — and do something about it.

A self-hosted, single-user web dashboard for **tracking and trading** a US
stocks & ETFs portfolio in real time, with an AI analyst watching over your
shoulder. One Rust binary, one SQLite file, your keys never leave your box.

**Status: Phase 0** — workspace skeleton, axum server, SQLite migrations,
Finnhub quotes end to end, embedded UI shell. See [PLAN.md](PLAN.md) for the
full design and roadmap, and [docs/RESEARCH.md](docs/RESEARCH.md) for the
research behind it.

## Quick start

```sh
# optional but recommended: free key from https://finnhub.io
export FINNHUB_API_KEY=your_key

cargo run -p rekt-server
# → http://127.0.0.1:7777
```

Configuration (env vars): `REKT_DB` (default `rekt.db`), `REKT_LISTEN`
(default `127.0.0.1:7777`), `FINNHUB_API_KEY`.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Workspace layout:

```
crates/rekt-core/     domain types + pure portfolio math (I/O-free)
crates/rekt-data/     MarketData trait + provider impls (Finnhub)
crates/rekt-server/   axum API, SQLite, embedded UI
migrations/           sqlx migrations
```

## License

[AGPL-3.0](LICENSE) — self-host freely; if you run a modified REKT as a
service, share your changes.

REKT is analysis and tooling, **not financial advice**. Trading involves
risk of loss. Paper-trade first.
