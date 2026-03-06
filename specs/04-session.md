# Session Management

## Purpose

The session module persists conversation history across agent restarts. It maintains the context that makes the agent feel continuous rather than amnesiac.

## Per-Channel Sessions

Each channel has its own session file under `sessions/`:

```
sessions/
├── telegram.json      # Telegram channel
├── socket.json        # Unix socket channel
├── heartbeat.json     # Periodic awareness checks
└── repl.json          # Local debug/testing
```

Sessions are isolated — a Telegram conversation and a REPL session carry independent history. The agent sees different conversational context depending on which channel it's serving.

### Why Separate Sessions?

1. **Different interaction contexts** — A Telegram conversation with the user is a different context than a heartbeat awareness check
2. **No cross-contamination** — Debug REPL messages don't appear in Telegram context
3. **Independent lifecycles** — Clearing the REPL session doesn't wipe Telegram history
4. **Concurrent access** — Different channels can run turns simultaneously without session conflicts

### Shared Long-Term Memory

While sessions are isolated, all channels share the workspace:

| Layer | Scope | Example |
|-------|-------|---------|
| `sessions/<channel>.json` | Per-channel | Conversational history |
| `memory/` | Shared | HISTORY.md, learnings, notes |
| Workspace files | Shared | SOUL.md, HEARTBEAT.md, projects/ |

A learning from a Telegram conversation (written to `memory/`) is visible during the next heartbeat or REPL session. The agent's knowledge persists across channels even though conversations don't.

## Data Structure

The session stores `Message` values directly (the same enum used by the agent loop and provider). This avoids a separate `SessionMessage` type and keeps the serialization format aligned with the OpenAI wire format.

Timestamps use a custom `Timestamp(u64)` type — seconds since Unix epoch — to avoid pulling in `chrono` for two fields. Messages do not carry individual timestamps.

## Storage Format

Session files live in `sessions/` under the workspace root. Example (`sessions/telegram.json`):

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

The session module is channel-agnostic. It doesn't know which channel it serves — the caller provides the path (`sessions/telegram.json`, `sessions/repl.json`, etc).

## File Safety

Writes use atomic rename to prevent corruption: write to `sessions/<channel>.json.tmp`, then rename to `sessions/<channel>.json`.

## Session Commands

| Command | Action | Scope |
|---------|--------|-------|
| `/new` | Clear session, start fresh | REPL and socket |

## MVP Simplifications

1. **No consolidation** — Messages accumulate unbounded
2. **No windowing** — All messages sent to LLM (until context limit)
3. **No backup** — Single file per channel, atomic write
4. **No encryption** — Plain JSON (workspace is private anyway)
5. **No per-message timestamps** — Only session-level `created_at`/`updated_at`

## Future Considerations

### Memory Consolidation

When sessions grow large (particularly heartbeat), we'll need to consolidate:

1. Take oldest N messages
2. Ask LLM to summarize key facts
3. Write summary to `memory/`
4. Remove old messages from session

### Token Counting

Currently no token awareness. Eventually:

1. Estimate tokens per message
2. Track cumulative context size
3. Trigger consolidation before hitting limit

### Session Metadata

May want to store channel-specific metadata alongside the message history (e.g., Telegram chat_id, last update_id offset). Keep the session format extensible — additional top-level fields are ignored by older code.
