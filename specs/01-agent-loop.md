# Agent Loop

## Purpose

The agent loop is the core execution engine. It orchestrates the conversation between the user, the LLM, and the tools. Each "turn" consists of sending context to the LLM and either receiving a final response or executing tool calls until the LLM is done.

## Why This Design?

The loop pattern is the standard approach for agentic systems because:

1. **LLMs are stateless** — Each API call is independent; we maintain state
2. **Tool use is iterative** — The LLM may need multiple tool calls to complete a task
3. **Control is explicit** — We decide when to stop, not the LLM

## Behavior

1. Push user message onto the session
2. Prepend system prompt to session messages (system prompt is not stored in the session)
3. Send to provider
4. If `Response::Text` — store assistant message, return text
5. If `Response::ToolCalls` — store assistant message, execute all tool calls in parallel, store results, loop

The system prompt is prepended per provider call but not persisted in the session. The prompt is fetched fresh on every turn via `workspace.system_prompt()`, so edits to SOUL.md take effect on the next message without restarting or running `/new`.

## Context Building

Each turn starts by assembling the message array:

```
[
    { role: "system", content: <SOUL.md + AGENTS.md + USER.md> },
    { role: "user", content: <message 1> },
    { role: "assistant", content: <response 1> },
    ...
    { role: "user", content: <current message> }
]
```

The system prompt is built by concatenating:
- Contents of `SOUL.md` (personality)
- Contents of `AGENTS.md` (instructions)
- Contents of `USER.md` (user profile, optional)

## Constraints

All values are configurable via `config.toml` (see `src/config.rs`). Defaults shown below.

| Constraint | Default | Config key | Rationale |
|------------|---------|------------|-----------|
| Max iterations | 100 | `agent.max_iterations` | Prevent infinite loops, runaway costs |
| Timeout per tool | 60s | `tools.exec.timeout_secs` | Don't hang on slow commands |
| Max tokens | 4096 | `provider.max_tokens` | Balance cost vs capability |

## Repetition Detection

The loop tracks consecutive identical tool call sets to detect stuck loops. A fingerprint is computed for each iteration's calls — the list of `(tool_name, parsed_args)` pairs, compared as `serde_json::Value` so JSON key reordering doesn't defeat detection.

If the same fingerprint appears `REPEAT_LIMIT` times consecutively, execution is skipped and an error message is injected into the session as each call's tool result. This gives the LLM a chance to self-correct. If it keeps looping, the error repeats until `max_iterations` terminates the turn.

| Repeat count | Behavior |
|---|---|
| 1–2 | Execute normally (LLM may legitimately retry once) |
| 3+ | Skip execution, inject error message, `continue` loop |

The counter resets when the call signature changes, so interleaved different calls (A, A, B, B, A, A) don't trigger false positives.

## Error Handling

| Error | Behavior |
|-------|----------|
| Provider API error | Return error to user, don't retry |
| Tool execution error | Return error text to LLM, let it decide |
| Max iterations reached | Return `Error::MaxIterationsReached` to caller |
| Parse error | Return error to user |

## Actor Pattern

The agent loop runs inside an actor. The `Agent` struct owns the session, provider, tools, and config. It processes one `Envelope` at a time in a sequential `while let Some(envelope) = rx.recv().await` loop. This eliminates session locking entirely.

### AgentHandle

`AgentHandle` is a cloneable wrapper around an `mpsc::Sender<Envelope>`. Each channel holds one clone. Callers never construct envelopes directly — they call `send_message()` with a `ChannelSource`, input text, optional activity sender, and cancellation token, then await the reply over a oneshot channel.

### ChannelSource

Messages are tagged with their origin (Heartbeat, GitHub PR, Socket, Telegram) before entering the unified session. The actor prepends `[ChannelSource]` to each message so the agent (and humans reviewing logs) can tell where input came from.

### Input Classification

The actor classifies each envelope's input text:
- Text starting with `/` must match a known slash command or it's an error
- Everything else is a free-text message routed through the agent loop

## State

The agent loop functions (`process_message`, `run_turn`) are stateless — they take session, provider, and tools by reference. The actor owns the session path and handles load/save around each envelope. This makes the loop easy to test in isolation.

## Activity Events

`run_turn` and `process_message` accept an optional `mpsc::Sender<Activity>` for emitting structured events during execution. See [spec 16](16-activity.md) for the full design.

Events are emitted at these points in the loop:

1. **Compaction** — after `compact_if_needed` returns `Ok(true)`, emit `Activity::Compaction` with before/after token estimates
2. **Tool start** — before `join_all`, emit `Activity::ToolStart` for each pending call
3. **Tool end** — after each tool result, emit `Activity::ToolEnd` with tool name and error if failed/blocked
4. **Max iterations** — at loop exhaustion, emit `Activity::MaxIterations`

When repetition detection skips execution, no `ToolStart`/`ToolEnd` events are emitted for that iteration.

Callers that don't need events pass `None`. No behavior change when the sender is absent.

## Future Considerations

- **Streaming**: Currently batch-only. Streaming would update the CLI in real-time.
