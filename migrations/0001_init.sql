-- Phase 0/1 schema: tracking only. Trading tables (orders, fills, alerts)
-- arrive with Phase 2; analyses/recommendations with Phase 5.
--
-- Money columns are TEXT holding decimal strings — never REAL (PLAN.md §7).

CREATE TABLE instruments (
    id          INTEGER PRIMARY KEY,
    symbol      TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL DEFAULT '',
    exchange    TEXT NOT NULL DEFAULT '',
    currency    TEXT NOT NULL DEFAULT 'USD',
    kind        TEXT NOT NULL DEFAULT 'stock' CHECK (kind IN ('stock', 'etf'))
);

CREATE TABLE transactions (
    id            INTEGER PRIMARY KEY,
    instrument_id INTEGER REFERENCES instruments(id),
    kind          TEXT NOT NULL CHECK (kind IN
                    ('buy', 'sell', 'dividend', 'split', 'deposit', 'withdrawal')),
    qty           TEXT NOT NULL DEFAULT '0',
    price         TEXT NOT NULL DEFAULT '0',
    fees          TEXT NOT NULL DEFAULT '0',
    taxes         TEXT NOT NULL DEFAULT '0',  -- separate from fees, deliberately
    ts            TEXT NOT NULL,              -- RFC 3339 UTC
    source        TEXT NOT NULL DEFAULT 'manual' CHECK (source IN
                    ('manual', 'csv', 'broker_fill')),
    fill_id       INTEGER,                    -- FK once fills table exists (Phase 2)
    note          TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_transactions_instrument_ts ON transactions(instrument_id, ts);

CREATE TABLE candles (
    instrument_id INTEGER NOT NULL REFERENCES instruments(id),
    date          TEXT NOT NULL,              -- YYYY-MM-DD, exchange-local
    open          TEXT NOT NULL,
    high          TEXT NOT NULL,
    low           TEXT NOT NULL,
    close         TEXT NOT NULL,
    volume        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (instrument_id, date)
);

CREATE TABLE snapshots (
    date          TEXT NOT NULL,              -- YYYY-MM-DD
    mode          TEXT NOT NULL DEFAULT 'live' CHECK (mode IN ('paper', 'live')),
    total_value   TEXT NOT NULL,
    cash          TEXT NOT NULL,
    invested      TEXT NOT NULL,
    realized_pnl  TEXT NOT NULL,
    PRIMARY KEY (date, mode)
);

CREATE TABLE watchlist (
    instrument_id INTEGER PRIMARY KEY REFERENCES instruments(id),
    added_ts      TEXT NOT NULL
);
