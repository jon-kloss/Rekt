-- Review fixes (PR #4): paper/live segregation on the transaction log,
-- persisted settings (pause switch), and the daily-cap index.

ALTER TABLE transactions ADD COLUMN mode TEXT NOT NULL DEFAULT 'live'
    CHECK (mode IN ('paper', 'live'));

CREATE INDEX idx_transactions_mode ON transactions(mode);
CREATE INDEX idx_orders_submitted_ts ON orders(submitted_ts);

-- Durable key/value settings; first user: trading.paused, which must
-- survive restarts (it's a safety switch).
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
