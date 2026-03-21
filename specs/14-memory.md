# Spec 14: Long-Term Memory

## Motivation

Give the agent durable recall across sessions and channels. Sessions hold
conversational context; memory holds knowledge — facts, preferences, decisions,
and learnings that outlive any single conversation.

## Status: Not Implemented

This spec describes a planned system that does not yet exist. The current
"memory" is a plain `memory/` directory with two files:

- `memory/HISTORY.md` — heartbeat responses appended as timestamped entries
- `memory/github_poll_state.json` — GitHub channel poll cursor

There is no SQLite, no FTS5, no `memory_store` or `memory_recall` tools, no
chunking, no embeddings, no memory injection into the system prompt. The agent
can search `memory/` via the `grep` tool if it chooses to.

## Planned Design

The graduation path from flat files to structured retrieval:

### Phase 1: SQLite + FTS5

- Single-file storage (`memory/memory.db`)
- Documents (named pieces of memory) split into chunks for granular retrieval
- Categories: Core (durable facts), Daily (logs), Conversation (excerpts)
- FTS5 full-text search with BM25 ranking
- Two tools: `memory_store` (write) and `memory_recall` (search)
- Migration from existing `memory/*.md` files on first run
- Compaction integration: summaries written to memory store as Daily documents

### Phase 2: Hybrid Retrieval

- Embedding provider trait (OpenAI, Ollama, or none)
- Vector search via cosine similarity (brute-force, no ANN index)
- Reciprocal Rank Fusion combining FTS and vector results
- Time decay scoring (7-day half-life)
- Category boost (Core memories score higher)
- Duplicate detection via embedding similarity

## Open Questions

1. Automatic vs. agent-driven storage — should compaction summaries be the
   only automatic writes?
2. Memory pruning — TTL or archival strategy for old daily logs?
3. Embedding model migration — reindex lazily or nuke and rebuild?
