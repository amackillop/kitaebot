# Spec 04: Session

## Motivation

The session persists conversation history across turns and restarts. All
channels share a single unified session, giving the agent cross-channel
continuity — it knows about a GitHub PR review when responding on Telegram.

## Behavior

### Unified Model

All channels write to one session file. Messages are tagged with their
`ChannelSource` (e.g. `[Telegram]`, `[GitHub PR #42]`, `[Socket]`,
`[Heartbeat]`) by the actor before appending. The agent sees the full
cross-channel history regardless of which channel is active.

In addition to the session, all channels share the workspace:

| Layer | Scope | Description |
|-------|-------|-------------|
| `sessions/session.json` | Unified | Tagged conversational history |
| `memory/` | Shared | HISTORY.md, learnings, notes |
| Workspace files | Shared | SOUL.md, AGENTS.md, projects/ |

### Data Structure

The session stores `Message` enum values directly:

```
Session {
    messages: Vec<Message>,
    created_at: Timestamp,    // seconds since Unix epoch
    updated_at: Timestamp,
}
```

`Timestamp` is a newtype over `u64`. Messages do not carry individual
timestamps.

The `Message` enum:

| Variant | Fields | Description |
|---------|--------|-------------|
| `User` | `content` | User input |
| `Assistant` | `content` | LLM text response |
| `ToolCalls` | `content`, `calls: Vec<ToolCall>` | LLM requesting tool invocations |
| `Tool` | `call_id`, `content` | Tool execution result |
| `System` | `content` | System-injected message (compaction summary, policy directive) |

On disk, messages are serialized using serde's default externally-tagged enum
format (e.g. `{"User": {"content": "..."}}`). This is **not** the OpenAI wire
format — wire conversion happens in the provider module.

### Operations

| Method | Behavior |
|--------|----------|
| `new()` | Create empty session with current timestamp |
| `load(path)` | Load from disk. Create new if file doesn't exist. Return `SessionError::Parse` if corrupt. |
| `save(path)` | Update `updated_at`, then atomic write (tmp + rename) |
| `add_message(msg)` | Append message, update `updated_at` |
| `messages()` | Return full message slice |
| `clear()` | Wipe messages, preserve `created_at`, update `updated_at` |
| `compact(summary)` | Replace all messages with a single summary message |
| `len()` | Message count |

### Token Estimation

Each `Message` exposes a `char_count()` method. For `ToolCalls`, this sums the
content length plus all function names and argument strings. Token count is
estimated as `total_chars / 4` (crude English approximation). This feeds into
the context budget system (see [spec 12](12-context.md)).

### File Safety

Writes use atomic rename: write to `session.json.tmp`, then rename to
`session.json`. A crash during write leaves the original file intact.

## Boundaries

### Owns

- Message storage and retrieval
- Atomic persistence to disk
- Timestamps (`created_at`, `updated_at`)
- The `compact()` operation (replacing messages with a summary)

### Does Not Own

- Deciding when to compact — the context module triggers that
- Generating the summary — the context module calls the provider
- Channel tagging — the actor prepends `[ChannelSource]` before adding
- Session file path — the workspace module defines it (`sessions/session.json`)
- System prompt assembly — prepended per provider call, never stored

### Interactions

- The **actor** calls `load()` before and `save()` after each envelope.
  Save happens unconditionally, even on turn failure.
- The **context module** calls `compact(summary)` when the token budget is
  exceeded.
- The **`/new` command** calls `clear()` then `save()`.
- The **`/compact` command** calls `force_compact()` which delegates to the
  context module.

## Failure Modes

| Failure | Error | Behavior |
|---------|-------|----------|
| File doesn't exist | — | Return new empty session |
| Corrupt JSON | `SessionError::Parse` | Error propagated to caller |
| Filesystem read error | `SessionError::Io` | Error propagated to caller |
| Serialization failure | `SessionError::Serialize` | Error propagated to caller |
| Crash during write | — | Atomic rename protects the original file |

## Constraints

- Session path: `<workspace>/sessions/session.json`
- No per-message timestamps
- No encryption (workspace is private)
- No version field (unknown fields are silently ignored by serde for forward
  compatibility)

## Open Questions

None currently.
