-- Phase 4: price/drawdown alerts with optional pre-staged order tickets
-- (alerts-to-action, PLAN.md §4). The alert NEVER auto-executes: a trigger
-- only surfaces the draft ticket for one-click human confirmation.

CREATE TABLE alerts (
    id               INTEGER PRIMARY KEY,
    instrument_id    INTEGER NOT NULL REFERENCES instruments(id),
    condition        TEXT NOT NULL CHECK (condition IN ('price_below', 'price_above', 'drawdown_above')),
    threshold        TEXT NOT NULL,           -- decimal string: price ($) or drawdown (%)
    draft_order_json TEXT,                    -- optional pre-staged ticket (side/qty/type/limit/tif)
    status           TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'triggered', 'dismissed')),
    created_ts       TEXT NOT NULL,
    triggered_ts     TEXT,
    triggered_value  TEXT,                    -- observed price/drawdown at trigger time
    note             TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_alerts_status ON alerts(status);
