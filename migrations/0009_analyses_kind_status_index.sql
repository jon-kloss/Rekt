-- Index for the kind-scoped "latest analysis" lookups that run on the hot path:
--   * GET /api/market reads the newest finished market_brief on every page load
--     (repo::latest_market_brief)
--   * /api/analyst reads the newest portfolio-narrative analysis
--     (repo::latest_portfolio_analysis)
-- Both filter by kind (+ status) and take the newest by id. Without this they
-- backward-scan the analyses PK b-tree row-by-row, a slow creep as the table
-- grows. (kind, status, id) lets the planner seek straight to the match.

CREATE INDEX IF NOT EXISTS idx_analyses_kind_status ON analyses(kind, status, id);
