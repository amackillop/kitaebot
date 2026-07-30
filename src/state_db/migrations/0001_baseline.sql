-- Baseline: the operational state database (state/kitaebot.db).
-- Usage ledger, review ledger, and the doc store for cursor state.

CREATE TABLE turns (
    id                INTEGER PRIMARY KEY,
    recorded_at       TEXT    NOT NULL DEFAULT (datetime('now')),
    git_sha           TEXT,
    session           TEXT    NOT NULL,
    source            TEXT    NOT NULL,
    model             TEXT    NOT NULL,
    calls             INTEGER NOT NULL,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    cost              REAL
);

CREATE TABLE reviews (
    id          INTEGER PRIMARY KEY,
    recorded_at TEXT NOT NULL DEFAULT (datetime('now')),
    repo        TEXT NOT NULL,
    gate        TEXT NOT NULL,
    git_ref     TEXT NOT NULL,
    verdict     TEXT NOT NULL,
    confidence  REAL
);

CREATE TABLE findings (
    id          INTEGER PRIMARY KEY,
    recorded_at TEXT NOT NULL DEFAULT (datetime('now')),
    repo        TEXT NOT NULL,
    gate        TEXT NOT NULL,
    git_ref     TEXT NOT NULL,
    source      TEXT NOT NULL,
    category    TEXT NOT NULL,
    severity    TEXT,
    confidence  REAL,
    file        TEXT,
    line        INTEGER,
    note        TEXT NOT NULL,
    disposition      TEXT,
    disposition_note TEXT,
    disposed_at      TEXT
);

-- One row per named state document (duty cursors, poll state,
-- distillation watermarks). Values are opaque JSON owned by their
-- writers; nothing queries inside them.
CREATE TABLE docs (
    name       TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
