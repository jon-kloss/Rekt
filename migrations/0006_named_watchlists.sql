-- Named watchlists: the flat `watchlist` set becomes membership in named
-- lists, so a user can keep themed universes (e.g. "AI", "Energy") and screen
-- each separately. The union of all members preserves the old behavior for
-- stream subscriptions and signal backfill.

CREATE TABLE watchlists (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    created_ts TEXT NOT NULL
);

CREATE TABLE watchlist_members (
    list_id       INTEGER NOT NULL REFERENCES watchlists(id),
    instrument_id INTEGER NOT NULL REFERENCES instruments(id),
    added_ts      TEXT NOT NULL,
    PRIMARY KEY (list_id, instrument_id)
);

CREATE INDEX idx_watchlist_members_instrument ON watchlist_members(instrument_id);

-- Always seed a default list so there is somewhere to put symbols.
INSERT INTO watchlists (name, created_ts)
  VALUES ('Watchlist', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));

-- Migrate any existing flat-watchlist symbols into the default list.
INSERT INTO watchlist_members (list_id, instrument_id, added_ts)
  SELECT (SELECT id FROM watchlists WHERE name = 'Watchlist'),
         instrument_id, added_ts
  FROM watchlist;

DROP TABLE watchlist;
