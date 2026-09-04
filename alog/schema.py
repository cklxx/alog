DDL = """
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS run (
    sid    INTEGER PRIMARY KEY,
    ext    TEXT NOT NULL UNIQUE,
    path   TEXT NOT NULL UNIQUE,
    fmt    TEXT NOT NULL,
    size   INTEGER NOT NULL DEFAULT 0,
    cursor INTEGER NOT NULL DEFAULT 0,
    n_ev   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS ev (
    sid     INTEGER NOT NULL,
    seq     INTEGER NOT NULL,
    off     INTEGER NOT NULL,
    len     INTEGER NOT NULL,
    ts      INTEGER,
    kind    TEXT,
    role    TEXT,
    model   TEXT,
    tool    TEXT,
    target  TEXT,
    in_tok  INTEGER,
    out_tok INTEGER,
    cache_r INTEGER,
    is_err  INTEGER,
    crc     INTEGER NOT NULL,
    PRIMARY KEY (sid, seq)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS reject (
    sid INTEGER NOT NULL,
    off INTEGER NOT NULL,
    err TEXT NOT NULL,
    PRIMARY KEY (sid, off)
) WITHOUT ROWID;

-- contentless: 46.3% of text size vs 172% for ordinary fts5.
-- contentless_delete=1 costs +1.7% size and 0.9->3.2ms per query, and is required:
-- a plain contentless table cannot DELETE, so a rewritten file would leave stale hits.
CREATE VIRTUAL TABLE IF NOT EXISTS ftx USING fts5(body, content='', contentless_delete=1);

CREATE TABLE IF NOT EXISTS ftx_map (
    rid INTEGER PRIMARY KEY,
    sid INTEGER NOT NULL,
    seq INTEGER NOT NULL
);
"""

# Each costs ~1.5% of corpus size and ~30% of ingest speed. Built on demand.
INDEXES = {
    "kind": "CREATE INDEX IF NOT EXISTS ix_kind ON ev(kind, ts)",
    "tool": "CREATE INDEX IF NOT EXISTS ix_tool ON ev(tool, ts)",
    "target": "CREATE INDEX IF NOT EXISTS ix_target ON ev(target)",
    "err": "CREATE INDEX IF NOT EXISTS ix_err ON ev(is_err) WHERE is_err = 1",
}
