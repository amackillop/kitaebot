# Activity Events

## Purpose

Structured side-channel events emitted during an agent turn so channels can display what the agent is doing (tool calls, compaction, iteration limits) without altering the dispatch contract.

## Why This Design?

1. **Observability** — Tool calls and compaction happen silently today. Channels have no way to show progress to the user during a long turn.
2. **Side-channel, not return value** — `dispatch` still returns `Result<String, String>`. Activity events flow through an `mpsc` channel as fire-and-forget side effects. No coupling between event production and consumption.
3. **Optional** — The sender is `Option<&mpsc::Sender<Activity>>`. Callers that don't care (heartbeat, tests) pass `None`. No overhead, no panics.
4. **Channel-local verbosity** — Whether to display events is a UI decision, not an agent decision. Each channel manages its own `/verbose` toggle.

## Event Type

```rust
pub enum Activity {
    Compaction { before: usize, after: usize },
    MaxIterations,
    ToolEnd { tool: String, error: Option<String> },
    ToolStart { tool: String },
}
```

Variants in alphabetical order per codebase convention.

| Variant | When | Fields |
|---------|------|--------|
| `Compaction` | After `compact_if_needed` returns `Ok(true)` | `before`/`after`: estimated token counts |
| `MaxIterations` | Loop exhausts `max_iterations` | — |
| `ToolEnd` | After each tool result | `tool`: name, `error`: `Some(msg)` if failed/blocked |
| `ToolStart` | Before `join_all` for each pending call | `tool`: name only (args in journalctl) |

### Display

Human-readable via `Display` impl:

```
Compacting context: 150432 -> 2841 tokens
Running tool: exec
Tool finished: exec
Tool failed: file_read (Permission denied)
Max iterations reached
```

### Serialization

`Serialize` derive for NDJSON transport over the socket channel.

## Emission

A free function handles the send:

```rust
pub fn emit(tx: Option<&mpsc::Sender<Activity>>, event: Activity) {
    if let Some(tx) = tx {
        let _ = tx.try_send(event);
    }
}
```

`try_send` is non-blocking. If the channel is full (backpressure from a slow consumer), events are silently dropped. This is acceptable — events are informational, not transactional.

## Threading

The `activity` parameter is threaded as `Option<&mpsc::Sender<Activity>>`:

- `agent::run_turn` — receives and emits events
- `agent::process_message` — receives and forwards to `run_turn`
- `AgentHandle::send_message` — accepts `activity_tx` and forwards to the actor via `Envelope`

Channels that want events create an `mpsc::channel(64)` per message and pass the sender. Channels that don't care pass `None`.

## Consumption

Channels that opt into activity events use `tokio::select! { biased }`:

```rust
let (tx, mut rx) = mpsc::channel(64);
let result = tokio::spawn(dispatch(..., Some(&tx)));

loop {
    tokio::select! {
        biased;
        Some(event) = rx.recv() => { /* display if verbose */ }
        result = &mut result => { break; }
    }
}
// drain remaining buffered events
while let Ok(event) = rx.try_recv() { /* display */ }
```

`biased` ensures events drain before checking dispatch completion, so no events are lost between the last emit and the join.

## `/verbose` Toggle

Channel-local UI state, not a slash command:

- **Socket**: toggled per connection, intercepted in `handle_line` before dispatch. Resets on reconnect.
- **Telegram**: toggled per polling session, intercepted in `handle_message`. Resets on restart.
- **Heartbeat**: no toggle (no interactive user).

Response: `"Verbose: on"` / `"Verbose: off"` — sent directly, not through the agent.

## Channel Buffer Size

64 events. A single tool-heavy turn with 20 parallel calls produces ~40 events (start + end per call). 64 gives comfortable headroom without memory concerns.

## Simplifications

1. **No event filtering** — All-or-nothing verbose flag. No per-event-type filtering.
2. **No event persistence** — Events are ephemeral. `tracing` handles durable logs.
3. **No timestamps on events** — `tracing` spans cover timing.
4. **No streaming** — Events are discrete, not a byte stream.
