# Spec 01: Agent Loop

## Motivation

The agent loop is the core execution engine. It orchestrates the conversation
between the user, the LLM, and the tools. Each "turn" sends context to the LLM
and either receives a final text response or executes tool calls in a loop until
the LLM produces one.

## Behavior

### Turn Lifecycle

A turn proceeds in this order:

1. Compact the session if the token budget is exceeded (see [spec 12](12-context.md))
2. Push the user message onto the session
3. Enter the tool loop (up to `max_iterations`):
   a. Prepend the system prompt to the session messages (not persisted)
   b. Call the provider
   c. If `Response::Text` — store assistant message, return text, exit loop
   d. If `Response::ToolCalls` — store assistant message, run safety gates,
      execute calls in parallel, record results, continue loop

Compaction runs **before** the user message is added, so the current input is
never summarized away.

The system prompt is assembled fresh on every provider call by concatenating
`SOUL.md`, `AGENTS.md`, and `USER.md` (optional). Edits to these files take
effect on the next call without restart.

### Tool Result Recording

Every successful tool output is passed through the safety layer (see
[spec 11](11-safety.md)). Clean outputs are wrapped in XML tags:

```
<tool_output name="tool_name">
output
</tool_output>
```

If the safety layer detects a leaked secret, the output is replaced with an
error message and never enters the session.

### Repetition Detection

The loop fingerprints each iteration's tool calls as a set of `(name, args)`
pairs (compared as `serde_json::Value` so key order is irrelevant).

| Consecutive identical count | Behavior |
|-----------------------------|----------|
| 1-2 | Execute normally |
| 3+  | Skip execution, inject error as each call's result |

The counter resets when the fingerprint changes.

### Policy Violation Gate

When a tool returns `ToolError::Blocked`, a per-turn strike counter increments.

| Strike | Behavior |
|--------|----------|
| 1 | Inject a system message directing the LLM to stop attempting the blocked operation. Continue the turn. |
| 2 | Halt the turn immediately. Return a synthetic response listing the blocked operations and their guidance. |

The strike counter resets per turn.

### Cancellation

The turn accepts a cancellation token. Cancellation is checked:

- Before compaction
- Around the provider call
- Around tool execution (`join_all`)
- At the top of each loop iteration

When cancelled, the turn emits `Activity::Cancelled` and returns
`Error::Cancelled`. Partial session state from the current turn is still saved.

### Activity Events

The turn accepts an optional activity sender. Events are emitted at:

1. **Compaction** — after successful compaction, with before/after token counts
2. **Tool start** — before execution, one per call
3. **Tool end** — after execution, with error if failed/blocked
4. **Max iterations** — when the loop is exhausted
5. **Cancelled** — when the cancellation token fires

When repetition detection skips execution, no tool events are emitted for that
iteration. Events use non-blocking `try_send`; they are silently dropped if the
channel is full.

## Boundaries

### Owns

- The tool loop: iteration, repetition detection, policy gate, cancellation
- Turn-level orchestration: compaction trigger, context assembly, provider call
- Tool result recording and safety checking

### Does Not Own

- Session persistence — the actor handles load/save around each envelope
- System prompt content — sourced from the workspace
- Tool execution — delegated to the tool registry
- Context compaction logic — delegated to the context module
- Safety/leak detection — delegated to the safety module

### Actor

The loop runs inside an actor that processes envelopes sequentially. The actor
owns the session path and handles load/save.

**Session save semantics**: the session is saved after every envelope,
regardless of whether the turn succeeded or failed. This means partial state
(e.g., tool calls executed before a provider error) is persisted. If the save
itself fails, the save error takes precedence and the turn result is lost.

**Input classification**: the actor delegates to `Input::parse()`. Text
starting with `/` must match a known slash command or an error is returned.
Everything else is a free-text message routed through `run_turn`.

Known commands: `/compact`, `/context`, `/heartbeat`, `/new`, `/stats`.

Commands handle their own session load/save independently from the message
path. This means commands like `/compact` and `/new` load and save the session
directly, while `/heartbeat` delegates to `process_message` which does its own
load/save.

### AgentHandle

A cloneable `mpsc::Sender<Envelope>` wrapper. Channels call
`send_message(source, input, activity_tx, cancel)` and await a `Reply` over a
oneshot channel. `Reply` carries a `content: String` and a `preformatted: bool`
hint for display formatting.

If the actor has shut down, `send_message` returns a synthetic error string.

### ChannelSource

Messages are tagged with their origin before entering the session:

- `Heartbeat`
- `GitHub { pr_number: u32 }`
- `Socket`
- `Telegram`

The actor prepends `[ChannelSource]` to each user message.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Provider API error | Return error to caller. Session (including partial state) is saved. |
| Tool execution error | Error text returned to LLM as tool result. Turn continues. |
| Tool blocked (policy) | Strike counter incremented. At 2 strikes, turn halts with guidance message. |
| Safety violation | Tool output replaced with error. Original output never stored. Turn continues. |
| Max iterations | Return `Error::MaxIterationsReached`. Session saved. |
| Cancellation | Return `Error::Cancelled`. Session saved. |
| Session save failure | Save error propagated to caller. Turn result is lost. |

## Constraints

All values configurable via `config.toml`:

| Constraint | Default | Config key |
|------------|---------|------------|
| Max iterations per turn | 100 | `agent.max_iterations` |
| Exec tool timeout | 60s | `tools.exec.timeout_secs` |
| Provider response max tokens | 4096 | `provider.max_tokens` |
| Context window budget | 200,000 tokens at 80% | `context.max_tokens`, `context.budget_percent` |

Note: `provider.max_tokens` caps the LLM's response length. `context.max_tokens`
caps the conversation window and triggers compaction. These are independent.

## Open Questions

None currently.
