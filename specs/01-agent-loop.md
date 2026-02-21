# Agent Loop

## Purpose

The agent loop is the core execution engine. It orchestrates the conversation between the user, the LLM, and the tools. Each "turn" consists of sending context to the LLM and either receiving a final response or executing tool calls until the LLM is done.

## Why This Design?

The loop pattern is the standard approach for agentic systems because:

1. **LLMs are stateless** — Each API call is independent; we maintain state
2. **Tool use is iterative** — The LLM may need multiple tool calls to complete a task
3. **Control is explicit** — We decide when to stop, not the LLM

## Behavior

```
fn run_turn(user_message: &str) -> Result<String> {
    let mut messages = build_context(user_message);

    for iteration in 0..MAX_ITERATIONS {
        let response = provider.chat(&messages, &tools)?;

        match response {
            Response::Text(content) => {
                return Ok(content);
            }
            Response::ToolCalls(calls) => {
                messages.push(assistant_message_with_tool_calls(&calls));

                for call in calls {
                    let result = tools.execute(&call)?;
                    messages.push(tool_result_message(call.id, result));
                }
            }
        }
    }

    Err(Error::MaxIterationsReached)
}
```

## Context Building

Each turn starts by assembling the message array:

```
[
    { role: "system", content: <SOUL.md + AGENTS.md + context> },
    { role: "user", content: <message 1> },
    { role: "assistant", content: <response 1> },
    ...
    { role: "user", content: <current message> }
]
```

The system prompt includes:
- Contents of `SOUL.md` (personality)
- Contents of `AGENTS.md` (instructions)
- Current working directory
- Available tools summary

## Constraints

| Constraint | Value | Rationale |
|------------|-------|-----------|
| Max iterations | 20 | Prevent infinite loops, runaway costs |
| Timeout per tool | 60s | Don't hang on slow commands |
| Max tokens | 4096 | Balance cost vs capability |

## Error Handling

| Error | Behavior |
|-------|----------|
| Provider API error | Return error to user, don't retry |
| Tool execution error | Return error text to LLM, let it decide |
| Max iterations reached | Return partial result + warning |
| Parse error | Return error to user |

## State

The agent loop itself is stateless. All persistence is handled by the session module. This makes the loop easy to test and reason about.

## Future Considerations

- **Streaming**: Currently batch-only. Streaming would update the CLI in real-time.
- **Parallel tool calls**: OpenAI API supports multiple tool calls; we execute sequentially for simplicity.
- **Token counting**: Currently no token awareness. May need to truncate history when approaching limits.
