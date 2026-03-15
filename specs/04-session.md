# Session Management

## Purpose

The session module persists conversation history across agent restarts. It maintains the context that makes the agent feel continuous rather than amnesiac.

## Unified Session

All channels share a single session file:

```
session.json      # Unified session for all channels
```

Messages from different channels are tagged with their `ChannelSource` (e.g. `[Telegram]`, `[GitHub PR #42]`, `[Socket]`, `[Heartbeat]`) before being appended. The agent sees the full cross-channel conversation history, giving it continuity across all interfaces.

### Why Unified?

1. **Cross-channel context** — The agent knows about a GitHub PR review when responding on Telegram
2. **No session locking** — The agent actor processes envelopes sequentially; only one turn runs at a time
3. **Simpler persistence** — One file, one load/save path
4. **Natural conversation flow** — The agent's full history is available regardless of which channel is active

### Shared Long-Term Memory

In addition to the unified session, all channels share the workspace:

| Layer | Scope | Example |
|-------|-------|---------|
| `session.json` | Unified | Tagged conversational history from all channels |
| `memory/` | Shared | HISTORY.md, learnings, notes |
| Workspace files | Shared | SOUL.md, HEARTBEAT.md, projects/ |

A learning from a Telegram conversation (written to `memory/`) is visible immediately in the next turn from any channel.

## Data Structure

The session stores `Message` values directly (the same enum used by the agent loop and provider). This avoids a separate `SessionMessage` type and keeps the serialization format aligned with the OpenAI wire format.

Timestamps use a custom `Timestamp(u64)` type — seconds since Unix epoch — to avoid pulling in `chrono` for two fields. Messages do not carry individual timestamps.

## Storage Format

The session file lives at `session.json` under the workspace root. Example:

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

The session module is channel-agnostic. The caller (the agent actor) provides the path. With the unified session, there is only one path: `session.json`.

## File Safety

Writes use atomic rename to prevent corruption: write to `session.json.tmp`, then rename to `session.json`.

## Session Commands

| Command | Action | Scope |
|---------|--------|-------|
| `/new` | Clear session, start fresh | Any channel (clears the unified session) |

## MVP Simplifications

1. **No consolidation** — Messages accumulate unbounded
2. **No windowing** — All messages sent to LLM (until context limit)
3. **No backup** — Single file, atomic write
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
