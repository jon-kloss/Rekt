-- Phase 2: trading. Orders mirror broker state; fills are the raw broker
-- executions (idempotent by execution_id) that roll up into transactions.

CREATE TABLE orders (
    id               INTEGER PRIMARY KEY,
    client_order_id  TEXT NOT NULL UNIQUE,   -- deterministic: rekt-{mode}-{id}
    broker_order_id  TEXT UNIQUE,            -- set once the broker accepts
    instrument_id    INTEGER NOT NULL REFERENCES instruments(id),
    side             TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
    order_type       TEXT NOT NULL CHECK (order_type IN ('market', 'limit')),
    qty              TEXT NOT NULL,
    limit_price      TEXT,
    tif              TEXT NOT NULL DEFAULT 'day' CHECK (tif IN ('day', 'gtc')),
    -- Full state machine incl. in-flight mutation states (docs/RESEARCH.md §3.1)
    status           TEXT NOT NULL CHECK (status IN
                       ('pending_submit', 'submitted', 'accepted',
                        'partially_filled', 'filled', 'canceled', 'rejected',
                        'expired', 'pending_cancel', 'replaced', 'failed')),
    filled_qty       TEXT NOT NULL DEFAULT '0',
    avg_fill_price   TEXT,
    mode             TEXT NOT NULL CHECK (mode IN ('paper', 'live')),
    note             TEXT NOT NULL DEFAULT '',
    submitted_ts     TEXT NOT NULL,
    updated_ts       TEXT NOT NULL
);

CREATE INDEX idx_orders_status ON orders(status);

CREATE TABLE fills (
    id            INTEGER PRIMARY KEY,
    execution_id  TEXT NOT NULL UNIQUE,      -- broker execution id ⇒ idempotent ingest
    order_id      INTEGER NOT NULL REFERENCES orders(id),
    qty           TEXT NOT NULL,
    price         TEXT NOT NULL,
    ts            TEXT NOT NULL
);
