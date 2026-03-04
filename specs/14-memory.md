# Long-Term Memory

## Purpose

Give the agent durable recall across sessions and channels. Sessions hold conversational context; memory holds *knowledge* — facts, preferences, decisions, and learnings that outlive any single conversation.

Currently memory is just markdown files searched via `rg`. This spec defines the graduation path: from grep to full-text search to hybrid retrieval with embeddings.

## Why Not Just Bigger Context Windows?

1. **Context is ephemeral** — Compaction (spec 12) summarizes and discards. Important facts that aren't in the summary are gone.
2. **Cross-channel** — A decision made in Telegram should be available during a heartbeat. Sessions are isolated; memory is shared.
3. **Selective recall** — Injecting all memory into every prompt is wasteful and noisy. The agent should retrieve only what's relevant to the current query.
4. **Cost** — Tokens aren't free. Retrieving 5 relevant chunks is cheaper than carrying 50k tokens of "just in case" context.

## Architecture

Three layers, each building on the previous:

```
┌─────────────────────────────────────────────────┐
│                  Agent Loop                     │
│                                                 │
│   system prompt ← SOUL + AGENTS + USER + daily  │
│                   + recalled memory chunks      │
│                                                 │
│   ┌──────────┐   ┌──────────────┐               │
│   │ memory   │   │ memory       │               │
│   │ _store   │   │ _recall      │               │
│   └────┬─────┘   └──────┬───────┘               │
│        │                │                       │
├────────┼────────────────┼───────────────────────┤
│        ▼                ▼                       │
│   ┌──────────────────────────────┐              │
│   │         Memory Store         │              │
│   │                              │              │
│   │  write ──► chunk ──► embed   │              │
│   │  recall ──► retrieve ──► rank│              │
│   └──────────────┬───────────────┘              │
│                  │                              │
│   ┌──────────────▼───────────────┐              │
│   │        SQLite + FTS5         │              │
│   │   documents, chunks, fts     │              │
│   │   (+ embeddings column       │              │
│   │    when provider available)  │              │
│   └──────────────────────────────┘              │
└─────────────────────────────────────────────────┘
```

## Data Model

### Documents

A document is a named piece of memory. Think of it as a file in a virtual filesystem.

```
Document {
    id:         Uuid,
    path:       String,      -- unique, e.g. "MEMORY.md", "daily/2025-03-01.md"
    content:    String,
    category:   Category,
    created_at: Timestamp,
    updated_at: Timestamp,
}
```

### Categories

```
Category = Core | Daily | Conversation
```

| Category | Purpose | Lifetime | Example |
|----------|---------|----------|---------|
| `Core` | Durable facts, preferences, decisions | Indefinite | "User prefers Nix over Docker" |
| `Daily` | Timestamped logs, session summaries | Weeks | Daily log entries, compaction summaries |
| `Conversation` | Notable excerpts from sessions | Days–weeks | Interesting findings during a task |

Categories exist for retrieval scoring, not access control. All categories are searchable.

### Chunks

Documents are split into chunks for granular retrieval. A chunk is the unit of search and ranking.

```
Chunk {
    id:          Uuid,
    document_id: Uuid,
    index:       u32,         -- position within document
    content:     String,
    embedding:   Option<Vec<f32>>,
}
```

### Chunking Strategy

Word-boundary chunking with overlap:

- **Chunk size**: 400 words (smaller than IronClaw's 800 — our documents are shorter)
- **Overlap**: 15% (60 words)
- **Step size**: 340 words
- **Minimum chunk size**: 30 words (trailing runts merge with previous chunk)

Short documents (< chunk size) produce a single chunk. No chunking overhead for typical memory entries.

## Storage Backend

### SQLite with FTS5

Single file: `memory/memory.db`. No external services. Backups via `cp`.

```sql
CREATE TABLE documents (
    id         TEXT PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,
    content    TEXT NOT NULL,
    category   TEXT NOT NULL DEFAULT 'core',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE chunks (
    id          TEXT PRIMARY KEY,
    document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    content     TEXT NOT NULL,
    embedding   BLOB,                -- NULL until embeddings are enabled
    UNIQUE(document_id, chunk_index)
);

-- FTS5 virtual table for full-text search
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    content,
    content=chunks,
    content_rowid=rowid
);

-- Keep FTS index in sync
CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
END;
CREATE TRIGGER chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
    INSERT INTO chunks_fts(rowid, content) VALUES (new.rowid, new.content);
END;
```

Pragmas:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
```

WAL mode allows concurrent reads during writes — important since the agent might search memory while a heartbeat is writing to it.

### Why SQLite Over Markdown + grep?

1. **FTS5** — Proper ranking (BM25) instead of substring matching. Handles stemming, prefix queries.
2. **Atomic writes** — No partial-write corruption risk. No tmp+rename dance per file.
3. **Single file** — `memory.db` replaces the `memory/*.md` directory. Still trivially backupable.
4. **Schema evolution** — Migrations are straightforward. Adding the embedding column is an ALTER TABLE, not a file format change.
5. **Concurrent access** — WAL mode handles daemon + REPL accessing memory simultaneously.

### Migration from Markdown

On first run with the new memory system:

1. Read each `memory/*.md` file
2. Create a document with `path` = filename, `category` = infer from filename (`daily-*.md` → Daily, `HISTORY.md` → Daily, others → Core)
3. Chunk and index
4. Leave original files in place (read-only archive)

This is a one-time operation. After migration, the agent uses SQLite exclusively.

## Retrieval

### Phase 1: FTS Only (MVP)

Query the FTS5 index, rank by BM25:

```sql
SELECT c.id, c.content, c.document_id, rank
FROM chunks_fts
JOIN chunks c ON chunks_fts.rowid = c.rowid
WHERE chunks_fts MATCH ?
ORDER BY rank
LIMIT ?;
```

BM25 is built into FTS5 — no custom scoring needed. This alone is a massive improvement over `rg`.

### Phase 2: Hybrid (FTS + Vector)

When an embedding provider is configured, retrieval combines two signals via Reciprocal Rank Fusion (RRF).

#### Embedding Provider

Trait-based, provider configured in `config.toml`:

```toml
[memory.embeddings]
provider = "openai"            # "openai" | "ollama" | "none"
model = "text-embedding-3-small"
dimensions = 1536
```

Supported providers:

| Provider | Model | Dimensions | Notes |
|----------|-------|------------|-------|
| OpenAI | text-embedding-3-small | 1536 | Recommended default |
| Ollama | nomic-embed-text | 768 | Local, no API cost |
| None | — | — | FTS only, embeddings disabled |

The `EmbeddingProvider` trait:

```
trait EmbeddingProvider: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn dimensions(&self) -> usize;
}
```

Batch embedding is intentionally omitted from the trait. Reindexing can call `embed` in a loop. Premature batching optimization adds complexity for a workload that runs once per document write.

#### Vector Search

Cosine similarity, brute-force scan over the embedding column:

```sql
SELECT id, content, document_id, cosine_similarity(embedding, ?) AS score
FROM chunks
WHERE embedding IS NOT NULL
ORDER BY score DESC
LIMIT ?;
```

`cosine_similarity` is a Rust UDF registered on the SQLite connection. No ANN index — the corpus will be small enough (thousands of chunks, not millions) that exact search is fast and correct.

#### Reciprocal Rank Fusion

Combine FTS and vector results:

```
score(chunk) = α / (k + fts_rank) + (1 - α) / (k + vector_rank)
```

Where:
- `k = 60` (standard RRF constant, dampens top-rank dominance)
- `α = 0.5` (equal weight to FTS and vector — tune later if needed)
- Chunks appearing in only one result set get score from that set only
- Normalize final scores to [0, 1]
- Apply minimum relevance threshold (0.3) — don't inject garbage

#### Time Decay

Boost recent memories:

```
decay(chunk) = exp(-λ * age_days)
```

Where `λ = ln(2) / 7` (7-day half-life). A week-old memory scores 50% of a fresh one. A month-old memory scores ~6%.

Final score: `rrf_score * (0.7 + 0.3 * decay)`. The 0.7 floor prevents old but highly relevant memories from vanishing entirely.

#### Category Boost

Core memories get a +0.2 additive boost to their final score. Durable facts should outrank ephemeral conversation excerpts when relevance is similar.

## Tools

Two tools exposed to the agent:

### `memory_store`

```json
{
    "name": "memory_store",
    "description": "Store information in long-term memory for recall across sessions.",
    "parameters": {
        "path": "string — document path (e.g. 'MEMORY.md', 'notes/project-x.md')",
        "content": "string — content to store (replaces existing document at path)",
        "category": "string — 'core' | 'daily' | 'conversation' (default: 'core')"
    }
}
```

On store:
1. Upsert document at `path`
2. Re-chunk the content
3. Generate embeddings for each chunk (async, best-effort — store succeeds even if embedding fails)
4. Update FTS index (synchronous, via triggers)

### `memory_recall`

```json
{
    "name": "memory_recall",
    "description": "Search long-term memory for relevant information.",
    "parameters": {
        "query": "string — natural language search query",
        "limit": "integer — max results (default: 5, max: 20)"
    }
}
```

Returns ranked chunks with metadata:

```json
[
    {
        "content": "User prefers Nix over Docker for deployment...",
        "path": "MEMORY.md",
        "score": 0.82,
        "category": "core"
    }
]
```

### Duplicate Detection

Before storing, compute cosine similarity between the new content's embedding and existing chunks at the same path. If any chunk exceeds 0.95 similarity, skip the write and return a message indicating the memory already exists. Prevents the agent from storing the same fact repeatedly.

Only active when embeddings are enabled. With FTS-only, duplicates are the agent's problem (system prompt should discourage it).

## Context Injection

The system prompt (spec 06) gains a new section: recent memory context. Built during `system_prompt()`:

1. Load SOUL.md, AGENTS.md, USER.md (unchanged)
2. Load today + yesterday daily logs from memory store (replaces filesystem read)
3. Append a `## Memory` section with instructions:

```markdown
## Memory

You have long-term memory that persists across conversations. Use the
`memory_store` tool to save important facts, decisions, and learnings.
Use `memory_recall` to search for prior context before asking the user
to repeat themselves.

Memory categories:
- **core**: Durable facts and preferences (default)
- **daily**: Timestamped logs and session summaries
- **conversation**: Notable excerpts worth remembering
```

The agent retrieves memories on-demand via `memory_recall`. No automatic pre-fetching into the system prompt beyond daily logs — the agent decides what to recall based on the user's query.

## Compaction Integration

When context compaction (spec 12) fires:

1. Summarize the conversation (existing behavior)
2. **New**: Store the summary in memory via `memory_store` with `category = "daily"` and `path = "daily/{date}.md"`
3. Replace session messages with summary (existing behavior)

This closes the loop: compacted context becomes searchable long-term memory. Nothing is truly lost — it's just moved from the session to the memory store.

## Configuration

```toml
[memory]
# Chunking
chunk_size = 400          # words per chunk
overlap_percent = 15      # overlap between chunks

# Retrieval
min_relevance = 0.3       # discard results below this score
default_limit = 5         # default results per recall
rrf_k = 60               # RRF smoothing constant
decay_halflife_days = 7   # time decay half-life
category_boost_core = 0.2 # additive boost for Core category

[memory.embeddings]
provider = "none"         # "openai" | "ollama" | "none"
model = ""
dimensions = 0
```

All fields have defaults. Memory works out of the box with FTS-only (`provider = "none"`). Embeddings are opt-in.

## REPL Commands

| Command | Action |
|---------|--------|
| `/memory <query>` | Search memory, display results with scores |
| `/memory-stats` | Document count, chunk count, index size, embedding coverage |

## MVP Scope

Phase 1 — what gets built first:

- [x] SQLite storage with FTS5
- [x] `memory_store` and `memory_recall` tools
- [x] Chunking (word-boundary with overlap)
- [x] BM25 ranking
- [x] Migration from `memory/*.md` files
- [x] Compaction writes to memory store
- [x] `/memory` REPL command

Phase 2 — after MVP is stable:

- [ ] Embedding provider trait + OpenAI implementation
- [ ] Hybrid retrieval (RRF)
- [ ] Time decay scoring
- [ ] Category boost
- [ ] Duplicate detection
- [ ] Ollama embedding provider

## Assumptions

1. Corpus stays small (< 10k chunks). Brute-force cosine similarity is fine; no ANN index needed.
2. SQLite FTS5 BM25 is good enough for keyword retrieval. No need for custom BM25 tuning.
3. The agent will learn to use `memory_store` and `memory_recall` effectively via system prompt instructions — no forced automatic memorization.
4. OpenRouter (the LLM provider) and the embedding provider are independent services. Embedding failures don't block agent operation.
5. Single-user system. No multi-tenancy, no user_id scoping on documents.

## Unresolved Questions

1. **Automatic vs agent-driven storage** — Should compaction summaries be the only automatic writes, or should the agent also auto-store after certain events (e.g., user corrections, explicit "remember this")? Current design: agent-driven via tools + compaction auto-write.
2. **Memory pruning** — Old daily logs accumulate indefinitely. Should there be a TTL or archival strategy? Or is disk cheap enough to not care?
3. **Embedding model migration** — If the embedding model changes, all embeddings need regeneration. Should we store the model name per-chunk and reindex lazily, or nuke and rebuild?
4. **Memory in group chats** — IronClaw excludes MEMORY.md from group chat context. Is this relevant for kitaebot (currently single-user)?
5. **Read tool** — Should there be a `memory_read` tool for reading a full document by path, or is `memory_recall` (search) sufficient? IronClaw has both.
