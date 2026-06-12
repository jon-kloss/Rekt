# CLAUDE.md — working agreements for this repo

## PR workflow (standing instructions from Jon)

1. Build a phase/change on the branch, run all gates, push.
2. Open a PR when asked (never unprompted).
3. **After every PR is opened, immediately run the full code-review cycle
   without being asked**: multi-angle review subagents (be critical, multiple
   disciplines), post the surviving findings as inline PR comments, then fix
   every finding, push, and **reply to each original review comment with the
   fix and the commit hash it landed in**.
4. After merge, continue on the same branch (`claude/rekt-brainstorm-plan-4x83yg`).

## Gates (all must pass before any push)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# UI script syntax (vanilla JS embedded in index.html):
sed -n '/<script>/,/<\/script>/p' crates/rekt-server/assets/index.html | sed '1d;$d' > /tmp/ui.js && node --check /tmp/ui.js
```

## Hard invariants (do not relax in review fixes)

- Money is `rust_decimal::Decimal`, never `f64`; stored as TEXT decimal columns.
- `transactions.mode` segregates paper/live — paper fills must NEVER touch the
  live portfolio.
- The AI/alerts layer never executes orders: `rekt-analyst` must never depend
  on `rekt-broker`; alert drafts only prefill a ticket the human confirms via
  `/api/orders` (all guardrails apply).
- Every server-sourced string interpolated into `innerHTML` goes through `esc()`.
- Derived data is cached behind revision counters (`tx_revision`,
  `candles_revision`, `alerts_revision`, `watchlist_revision`) — no per-tick
  DB queries in the broadcaster path.
- Secrets (API keys) are env-only: never logged, never sent to the browser.
- Honest degradation: missing data/keys produce clear errors or `None`, never
  fabricated values.
