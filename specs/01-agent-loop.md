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

The system prompt is prepended per provider call but not persisted in the session. Edits to SOUL.md take effect on the next `/new` session since the prompt is cached at startup.

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
| Max iterations | 20 | `agent.max_iterations` | Prevent infinite loops, runaway costs |
| Timeout per tool | 60s | `tools.exec.timeout_secs` | Don't hang on slow commands |
| Max tokens | 4096 | `provider.max_tokens` | Balance cost vs capability |

## Error Handling

| Error | Behavior |
|-------|----------|
| Provider API error | Return error to user, don't retry |
| Tool execution error | Return error text to LLM, let it decide |
| Max iterations reached | Return `Error::MaxIterationsReached` to caller |
| Parse error | Return error to user |

## State

The agent loop itself is stateless. All persistence is handled by the session module. This makes the loop easy to test and reason about.

## Future Considerations

- **Streaming**: Currently batch-only. Streaming would update the CLI in real-time.
- **Token counting**: Currently no token awareness. May need to truncate history when approaching limits.
