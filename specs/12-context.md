# Context Window Management

## Purpose

Manage the conversation's token budget so sessions can run indefinitely without exceeding the model's context window. Automatic summarization compacts old messages when the budget is approached.

## Why This Design?

1. **Sessions grow unboundedly** — Without management, long conversations exceed the context window and the provider returns an error (or silently truncates)
2. **Simple heuristic over tokenizer** — `len_chars / 4` is close enough for English text and avoids a tokenizer dependency
3. **Summarization over truncation** — Compressing old messages preserves key facts and decisions; hard truncation loses context

## Token Estimation

Approximate tokens per message using character count:

```
estimated_tokens = message_chars / 4
```

Track cumulative estimate per session. This is a rough heuristic — it overestimates for code (more ASCII) and underestimates for CJK text, but it's sufficient for budget decisions. No tokenizer dependency.

## Context Budget

Configurable per model via `config.toml`:

```toml
[context]
max_tokens = 128000        # Model context window
budget_ratio = 0.8         # Use 80% for history
summarize_batch = 20       # Messages to summarize per compaction
```

The effective budget is `max_tokens * budget_ratio`. The remaining 20% is reserved for the system prompt and the model's response.

| Field | Default | Purpose |
|-------|---------|---------|
| `max_tokens` | 128000 | Model's advertised context window |
| `budget_ratio` | 0.8 | Fraction of window available for history |
| `summarize_batch` | 20 | Number of oldest messages to compact per cycle |

`max_tokens` and `budget_ratio` are model-dependent. No hardcoded assumptions about model capabilities.

## Windowing Strategy

When the estimated token count exceeds the budget:

1. Take the oldest `summarize_batch` messages from the conversation
2. Ask the LLM to compress them into key facts and decisions
3. Replace the original messages with a single `Message::System` summary
4. The summary becomes the new "floor" of the conversation

The summary is prepended after the system prompt, before remaining conversation messages.

## Summarization

The summarization request is a separate LLM call using the same provider. The prompt asks the model to extract:

- Key decisions made
- Facts established
- Current state of any ongoing tasks
- Unresolved questions

The result is a concise `Message::System` that preserves the conversation's essential context in a fraction of the tokens.

## Trigger

Check before each `run_turn()`:

1. Estimate total tokens across all messages in the session
2. If estimate exceeds `max_tokens * budget_ratio`, trigger compaction
3. Summarize the oldest `summarize_batch` messages
4. Replace them with the summary
5. Proceed with the turn

This ensures the conversation never exceeds the budget when the next turn begins.

## Future Considerations

- **Proper tokenizer** — Use `tiktoken` or model-specific tokenizer for accurate counts when precision matters
- **Sliding window with pinned messages** — Allow important messages to be "pinned" so they survive compaction
- **Multi-stage summarization** — Summarize summaries when even the compacted history grows too large
- **Per-channel budgets** — Different channels may warrant different context strategies
