-- Allow the 'market_brief' analysis kind (state-of-market context). Same
-- CHECK-widening table rebuild as 0007; migrations run FK-off (see open_db),
-- so the drop+recreate is safe and recommendations.analysis_id resolves to the
-- rebuilt table by name.

CREATE TABLE analyses_new (
    id                 INTEGER PRIMARY KEY,
    kind               TEXT NOT NULL CHECK (kind IN ('briefing', 'weekly_review', 'on_demand', 'market_ideas', 'market_brief')),
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
