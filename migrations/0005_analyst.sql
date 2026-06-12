-- Phase 5: the AI analyst (PLAN.md §5 layer 2). Analyses record every run
-- with full cost accounting; recommendations are ADVISORY rows that only
-- ever prefill an order ticket the human confirms via /api/orders.

CREATE TABLE analyses (
    id                 INTEGER PRIMARY KEY,
    kind               TEXT NOT NULL CHECK (kind IN ('briefing', 'weekly_review', 'on_demand')),
    model              TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'running' CHECK (status IN ('running', 'done', 'error')),
    started_ts         TEXT NOT NULL,
    finished_ts        TEXT,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd           TEXT NOT NULL DEFAULT '0',  -- decimal string, like all money
    question           TEXT,                       -- on_demand prompt, verbatim
    report_md          TEXT,
    tool_log_json      TEXT,
    error              TEXT
);

CREATE INDEX idx_analyses_started ON analyses(started_ts);

CREATE TABLE recommendations (
    id            INTEGER PRIMARY KEY,
    analysis_id   INTEGER NOT NULL REFERENCES analyses(id),
    instrument_id INTEGER NOT NULL REFERENCES instruments(id),
    action        TEXT NOT NULL CHECK (action IN ('buy', 'sell', 'trim', 'hold', 'watch')),
    sizing        TEXT NOT NULL DEFAULT '',
    rationale     TEXT NOT NULL,
    confidence    TEXT NOT NULL DEFAULT '' CHECK (confidence IN ('', 'low', 'medium', 'high')),
    status        TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'accepted', 'dismissed', 'expired')),
    created_ts    TEXT NOT NULL,
    expires_ts    TEXT
);

CREATE INDEX idx_recommendations_status ON recommendations(status);
