-- Allow the 'market_ideas' analysis kind. SQLite can't ALTER a CHECK
-- constraint, so rebuild the table with the widened constraint and copy the
-- rows. recommendations.analysis_id references analyses(id), so the DROP needs
-- FK enforcement off — migrations run on an FK-off connection (see open_db);
-- the app pool keeps FK on.

CREATE TABLE analyses_new (
    id                 INTEGER PRIMARY KEY,
    kind               TEXT NOT NULL CHECK (kind IN ('briefing', 'weekly_review', 'on_demand', 'market_ideas')),
    model              TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'running' CHECK (status IN ('running', 'done', 'error')),
    started_ts         TEXT NOT NULL,
    finished_ts        TEXT,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd           TEXT NOT NULL DEFAULT '0',
    question           TEXT,
    report_md          TEXT,
    tool_log_json      TEXT,
    error              TEXT
);

INSERT INTO analyses_new
    (id, kind, model, status, started_ts, finished_ts, input_tokens,
     output_tokens, cache_read_tokens, cache_write_tokens, cost_usd,
     question, report_md, tool_log_json, error)
  SELECT id, kind, model, status, started_ts, finished_ts, input_tokens,
         output_tokens, cache_read_tokens, cache_write_tokens, cost_usd,
         question, report_md, tool_log_json, error
  FROM analyses;

DROP TABLE analyses;
ALTER TABLE analyses_new RENAME TO analyses;
CREATE INDEX idx_analyses_started ON analyses(started_ts);
