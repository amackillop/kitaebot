-- LCM database schema.
--
-- Run inside a single BEGIN EXCLUSIVE / COMMIT by `schema::init`.
-- All statements use IF NOT EXISTS so re-opening an existing DB is a no-op.

CREATE TABLE IF NOT EXISTS conversations (
    conversation_id INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL UNIQUE,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    message_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id INTEGER NOT NULL REFERENCES conversations(conversation_id),
    seq             INTEGER NOT NULL,
    role            TEXT NOT NULL CHECK (role IN ('user','assistant','tool','system')),
    content         TEXT NOT NULL,
    token_count     INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE (conversation_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_messages_conversation
    ON messages(conversation_id, seq);

CREATE TABLE IF NOT EXISTS message_parts (
    part_id      TEXT PRIMARY KEY,
    message_id   INTEGER NOT NULL REFERENCES messages(message_id) ON DELETE CASCADE,
    part_type    TEXT NOT NULL CHECK (part_type IN ('text','tool_call','tool_output')),
    ordinal      INTEGER NOT NULL,
    text_content TEXT,
    tool_call_id TEXT,
    tool_name    TEXT,
    tool_input   TEXT,
    UNIQUE (message_id, ordinal)
);

CREATE INDEX IF NOT EXISTS idx_message_parts_message
    ON message_parts(message_id);

CREATE INDEX IF NOT EXISTS idx_message_parts_tool_call
    ON message_parts(tool_call_id) WHERE tool_call_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS summaries (
    summary_id                 TEXT PRIMARY KEY,
    conversation_id            INTEGER NOT NULL REFERENCES conversations(conversation_id),
    kind                       TEXT NOT NULL CHECK (kind IN ('leaf','condensed')),
    depth                      INTEGER NOT NULL,
    content                    TEXT NOT NULL,
    token_count                INTEGER NOT NULL,
    earliest_at                TEXT NOT NULL,
    latest_at                  TEXT NOT NULL,
    descendant_count           INTEGER NOT NULL,
    descendant_token_count     INTEGER NOT NULL,
    source_message_token_count INTEGER NOT NULL,
    model                      TEXT NOT NULL,
    created_at                 TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_summaries_conversation
    ON summaries(conversation_id, depth);

CREATE TABLE IF NOT EXISTS summary_messages (
    summary_id TEXT    NOT NULL REFERENCES summaries(summary_id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages(message_id)  ON DELETE CASCADE,
    PRIMARY KEY (summary_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_summary_messages_message
    ON summary_messages(message_id);

CREATE TABLE IF NOT EXISTS summary_parents (
    summary_id        TEXT NOT NULL REFERENCES summaries(summary_id) ON DELETE CASCADE,
    parent_summary_id TEXT NOT NULL REFERENCES summaries(summary_id) ON DELETE CASCADE,
    PRIMARY KEY (summary_id, parent_summary_id)
);

CREATE INDEX IF NOT EXISTS idx_summary_parents_parent
    ON summary_parents(parent_summary_id);

CREATE TABLE IF NOT EXISTS large_files (
    file_id               TEXT PRIMARY KEY,
    conversation_id       INTEGER NOT NULL REFERENCES conversations(conversation_id),
    path                  TEXT NOT NULL,
    mime_type             TEXT NOT NULL,
    byte_size             INTEGER NOT NULL,
    token_count           INTEGER NOT NULL,
    exploration_summary   TEXT NOT NULL,
    first_seen_message_id INTEGER REFERENCES messages(message_id),
    created_at            TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_large_files_conversation
    ON large_files(conversation_id);

CREATE TABLE IF NOT EXISTS summary_files (
    summary_id TEXT NOT NULL REFERENCES summaries(summary_id)   ON DELETE CASCADE,
    file_id    TEXT NOT NULL REFERENCES large_files(file_id)    ON DELETE CASCADE,
    PRIMARY KEY (summary_id, file_id)
);

CREATE INDEX IF NOT EXISTS idx_summary_files_file
    ON summary_files(file_id);

CREATE TABLE IF NOT EXISTS context_items (
    conversation_id INTEGER NOT NULL REFERENCES conversations(conversation_id),
    ordinal         INTEGER NOT NULL,
    item_type       TEXT NOT NULL CHECK (item_type IN ('message','summary')),
    message_id      INTEGER REFERENCES messages(message_id),
    summary_id      TEXT    REFERENCES summaries(summary_id),
    PRIMARY KEY (conversation_id, ordinal),
    CHECK (
        (item_type = 'message' AND message_id IS NOT NULL AND summary_id IS NULL)
     OR (item_type = 'summary' AND summary_id IS NOT NULL AND message_id IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_context_items_message
    ON context_items(message_id) WHERE message_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_context_items_summary
    ON context_items(summary_id) WHERE summary_id IS NOT NULL;

-- Full-text search.
--
-- `messages_fts` mirrors `messages.content` via the external content
-- pattern (rowid = message_id). `summaries_fts` is a standalone FTS5
-- table with `summary_id` carried as an UNINDEXED column so retrieval
-- can map FTS hits back to the TEXT primary key.

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    content=messages,
    content_rowid=message_id
);

CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content)
        VALUES (new.message_id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content)
        VALUES ('delete', old.message_id, old.content);
END;

CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content)
        VALUES ('delete', old.message_id, old.content);
    INSERT INTO messages_fts(rowid, content)
        VALUES (new.message_id, new.content);
END;

CREATE VIRTUAL TABLE IF NOT EXISTS summaries_fts USING fts5(
    content,
    summary_id UNINDEXED
);

CREATE TRIGGER IF NOT EXISTS summaries_ai AFTER INSERT ON summaries BEGIN
    INSERT INTO summaries_fts(rowid, content, summary_id)
        VALUES (new.rowid, new.content, new.summary_id);
END;

CREATE TRIGGER IF NOT EXISTS summaries_ad AFTER DELETE ON summaries BEGIN
    DELETE FROM summaries_fts WHERE rowid = old.rowid;
END;

CREATE TRIGGER IF NOT EXISTS summaries_au AFTER UPDATE ON summaries BEGIN
    UPDATE summaries_fts
        SET content = new.content, summary_id = new.summary_id
        WHERE rowid = old.rowid;
END;
