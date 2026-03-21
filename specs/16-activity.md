# Spec 16: Activity Events

## Motivation

Structured side-channel events emitted during an agent turn so channels can
display progress (tool calls, compaction, cancellation) without altering the
dispatch contract.

## Behavior

### Event Type

```rust
pub enum Activity {
    Cancelled,
    Compaction { before: usize, after: usize },
    MaxIterations,
    ToolEnd { tool: String, error: Option<String> },
    ToolStart { tool: String },
}
```

| Variant | When | Fields |
|---------|------|--------|
| `Cancelled` | Cancellation token fires | — |
| `Compaction` | After `compact_if_needed` returns `Ok(true)` | `before`/`after`: estimated token counts |
| `MaxIterations` | Loop exhausts `max_iterations` | — |
| `ToolEnd` | After each tool result | `tool`: name, `error`: `Some(msg)` if failed/blocked |
| `ToolStart` | Before `join_all` for each pending call | `tool`: name |

### Display

Human-readable via `Display` impl:

```
Turn cancelled
Compacting context: 150432 -> 2841 tokens
Running tool: exec
Tool finished: exec
Tool failed: file_read (Permission denied)
Max iterations reached
```

### Serialization

Tagged JSON via `#[serde(tag = "kind", rename_all = "snake_case")]` for NDJSON
transport over the socket channel.

### Emission

```rust
pub fn emit(tx: Option<&mpsc::Sender<Activity>>, event: Activity) {
    if let Some(tx) = tx {
        let _ = tx.try_send(event);
    }
}
```

`try_send` is non-blocking. If the channel is full, events are silently
dropped. Events are informational, not transactional.

### Threading

The activity sender is threaded through:

- `AgentHandle::send_message` — accepts `Option<mpsc::Sender<Activity>>`
  (owned, for `'static` bound on the envelope)
- `Envelope` — stores the owned sender
- Actor — converts to `Option<&mpsc::Sender<Activity>>` (borrowed reference)
- `process_message` and `run_turn` — receive and emit via borrowed reference

Channels that want events create an `mpsc::channel(64)` per message and pass
the sender. Channels that don't care pass `None`.

### Consumption

Channels use `tokio::select! { biased }` to drain events before checking
dispatch completion, followed by a `try_recv` drain loop for any remaining
buffered events:

```rust
loop {
    tokio::select! {
        biased;
        Some(event) = rx.recv() => { /* display if verbose */ }
        result = &mut reply => { break; }
    }
}
while let Ok(event) = rx.try_recv() { /* display */ }
```

### `/verbose` Toggle

Channel-local UI state, intercepted before dispatch:

- **Socket**: toggled per connection in `parse_line`. Resets on disconnect.
- **Telegram**: toggled per polling session in `handle_message`. Resets on
  daemon restart.
- **Heartbeat**: no toggle (passes `None` for activity sender).

Response: `"Verbose: on"` / `"Verbose: off"` — sent directly, not through the
agent.

## Boundaries

### Owns

- `Activity` enum definition and `Display` implementation
- `emit()` free function
- Channel buffer size convention (64)

### Does Not Own

- When events are emitted — the agent loop decides
- Whether events are displayed — each channel's `/verbose` toggle decides
- Event persistence — `tracing` handles durable logs

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Channel full (backpressure) | Event silently dropped |
| No sender (`None`) | No-op |

## Constraints

- Buffer size: 64 events per channel per message
- Non-blocking emission — never blocks the agent loop
- No event filtering — all-or-nothing verbose flag
- No event persistence — ephemeral, for live display only

## Open Questions

None currently.
