# Spec 12: Context Window Management

## Motivation

Manage the conversation's token budget so sessions can run indefinitely without
exceeding the model's context window. Automatic summarization compacts the
conversation when the budget is approached.

## Behavior

### Token Estimation

Approximate tokens per message using character count:

```
estimated_tokens = (system_prompt_chars + message_chars) / 4
```

`Message::char_count()` sums the content string length. For `ToolCalls`, it
additionally sums function names and argument strings. This is a rough
heuristic — overestimates for code, underestimates for CJK — but sufficient
for budget decisions. No tokenizer dependency.

### Budget

The effective budget is `max_tokens * budget_percent / 100` (integer
arithmetic, no floating point).

| Config key | Default | Description |
|------------|---------|-------------|
| `context.max_tokens` | 200000 | Model's advertised context window |
| `context.budget_percent` | 80 | Percentage that triggers compaction (1-100) |

### Compaction Trigger

Compaction runs at the top of `run_turn()`, **before** the user message is
pushed:

1. Estimate total tokens (session messages + system prompt)
2. If estimate exceeds budget **and** session has >= 2 messages, compact
3. Otherwise no-op

The compaction call is wrapped in the cancellation token and can be
interrupted.

### Compaction Strategy

When triggered:

1. Format all session messages as `[role] content` text
2. Send to the provider as a separate LLM call (same model, no tools) with
   a summarization prompt requesting preservation of important facts,
   decisions, tool results, and open questions
3. Replace all session messages with a single `Message::System` containing
   the summary

If the provider returns `Response::ToolCalls` (shouldn't happen since no tools
are provided), the `content` field is used as a fallback.

### Activity Event

On successful compaction, emits `Activity::Compaction { before, after }` with
estimated token counts.

### Commands

- `/context` — display estimated token usage, message count, and budget
- `/compact` — force a compaction cycle regardless of budget (still requires
  >= 2 messages). Prints before/after token counts.

Both commands are available from any channel.

`force_compact()` skips the budget check but applies the same >= 2 message
guard as auto-compaction.

## Boundaries

### Owns

- Token estimation (`session_tokens`, `Message::char_count`)
- Budget calculation
- Compaction trigger logic
- Summarization prompt and LLM call
- `/context` and `/compact` command implementations

### Does Not Own

- Session message storage — the session module handles `compact(summary)`
- Provider call execution — delegates to the provider trait
- When to call `compact_if_needed` — the agent loop owns that

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Provider error during summarization | `ProviderError` propagated to caller |
| Session has < 2 messages | Compaction skipped (not an error) |
| Cancellation during compaction | `Error::Cancelled` returned |

## Constraints

- Token estimation is `chars / 4` — no tokenizer library
- Compaction summarizes the entire conversation into one message (no partial
  windowing)
- The summarization uses the same provider and model as the agent — no
  separate compaction model

## Open Questions

None currently.
