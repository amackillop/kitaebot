# Session Management

## Purpose

The session module persists conversation history across agent restarts. It maintains the context that makes the agent feel continuous rather than amnesiac.

## Why Persistence Matters

Without persistence:
- Agent forgets everything on restart
- User must re-explain context every time
- No sense of ongoing relationship

With persistence:
- Conversations continue naturally
- Agent builds understanding over time
- User can reference past interactions

## Data Structure

The session stores `Message` values directly (the same enum used by the agent loop and provider). This avoids a separate `SessionMessage` type and keeps the serialization format aligned with the OpenAI wire format.

Timestamps use a custom `Timestamp(u64)` type — seconds since Unix epoch — to avoid pulling in `chrono` for two fields. Messages do not carry individual timestamps.

## Storage Format

Session is stored as `session.json` in the workspace. Example:

```json
{
    "messages": [
        {
            "role": "user",
            "content": "What files are in my workspace?"
        },
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "call_123", "type": "function", "function": {"name": "exec", "arguments": "{\"command\": \"ls\"}"}}
            ]
        },
        {
            "role": "tool",
            "tool_call_id": "call_123",
            "content": "$ ls\nSOUL.md\nsession.json\nprojects/\n\nExit code: 0"
        },
        {
            "role": "assistant",
            "content": "Your workspace contains: SOUL.md, session.json, projects/"
        }
    ],
    "created_at": 1708516800,
    "updated_at": 1708516802
}
```

## Operations

- **`Session::new()`** — Create empty session with current timestamp
- **`Session::load(path)`** — Load from disk; create new if file doesn't exist; return parse error if corrupt
- **`Session::save(path)`** — Atomic write (tmp + rename) to prevent corruption
- **`Session::add_message(msg)`** — Append message, update `updated_at`
- **`Session::messages()`** — Return full message slice (no windowing)
- **`Session::clear()`** — Wipe messages, preserve `created_at`

## File Safety

Writes use atomic rename to prevent corruption: write to `session.json.tmp`, then rename to `session.json`.

## Session Commands

| Command | Action |
|---------|--------|
| `/new` | Clear session, start fresh |

## MVP Simplifications

1. **No consolidation** — Messages accumulate unbounded
2. **No windowing** — All messages sent to LLM (until context limit)
3. **No backup** — Single file, atomic write
4. **No encryption** — Plain JSON (workspace is private anyway)
5. **No per-message timestamps** — Only session-level `created_at`/`updated_at`

## Future Considerations

### Memory Consolidation

When sessions grow large, we'll need to consolidate:

1. Take oldest N messages
2. Ask LLM to summarize key facts
3. Write to `MEMORY.md`
4. Append summary to `HISTORY.md`
5. Remove old messages from session

### Token Counting

Currently no token awareness. Eventually:

1. Estimate tokens per message
2. Track cumulative context size
3. Trigger consolidation before hitting limit

### Multiple Sessions

Current design is single-session. Could extend to:

- Named sessions (`kitaebot --session project-x`)
- Auto-switching based on context
- Session templates
