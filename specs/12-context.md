# Context Window Management

## Purpose

Manage the conversation's token budget so sessions can run indefinitely without exceeding the model's context window. Automatic summarization compacts the conversation when the budget is approached.

## Why This Design?

1. **Sessions grow unboundedly** — Without management, long conversations exceed the context window and the provider returns an error (or silently truncates)
2. **Simple heuristic over tokenizer** — `chars / 4` is close enough for English text and avoids a tokenizer dependency
3. **Summarization over truncation** — Compressing old messages preserves key facts and decisions; hard truncation loses context
4. **Whole-conversation compaction** — Summarize everything into one system message rather than batching. Simpler, and real-world testing will reveal if recent messages need preserving

## Token Estimation

Approximate tokens per message using character count:

```
estimated_tokens = chars / 4
```

Counts message content strings and, for assistant messages, tool call function names + arguments. This is a rough heuristic — it overestimates for code (more ASCII) and underestimates for CJK text, but it's sufficient for budget decisions. No tokenizer dependency.

## Context Budget

Configurable via `config.toml`:

```toml
[context]
max_tokens = 200000    # Model context window
budget_percent = 80    # Compact at 80% usage
```

The effective budget is `max_tokens * budget_percent / 100`. The remaining headroom is reserved for the system prompt and the model's response.

| Field | Default | Purpose |
|-------|---------|---------|
| `max_tokens` | 200000 | Model's advertised context window |
| `budget_percent` | 80 | Percentage of window that triggers compaction (1–100) |

Integer percentage avoids floating-point lint issues with `f32` ratios.

## Compaction Strategy

When the estimated token count exceeds the budget:

1. Summarize the entire conversation via a separate LLM call (same provider, no tools)
2. Replace all messages with a single `Message::System` containing the summary
3. Proceed with the turn

The summarization prompt asks the model to preserve important facts, decisions, tool results, and open questions.

## Trigger

Compaction runs at the top of `run_turn()`, before the user message is pushed:

1. Estimate total tokens (session messages + system prompt)
2. If estimate exceeds budget and session has >= 2 messages, compact
3. Otherwise no-op

This is a single integration point that covers all entry paths (REPL, heartbeat, Telegram).

## REPL Commands

- `/context` — Display estimated token usage, message count, and budget. Pure computation, no LLM call.
- `/compact` — Force a compaction cycle regardless of budget. Prints before/after token counts.

## Future Work

### Tool output pruning

Before resorting to an LLM summarization call, walk backwards through tool results and erase output from old tool calls. Protect the most recent N turns and a configurable token threshold of tool output. This is cheap (no LLM call) and effective since tool output is typically the bulk of token usage. Only fall through to full summarization if pruning doesn't reclaim enough.

Reference: opencode `prune()` — protects 40k tokens of recent tool output, requires >20k reclaimable before acting.

### Structured summary template

Replace the freeform summarization prompt with a structured template:

- **Goal**: What the user is trying to accomplish
- **Instructions**: Important user directives still in effect
- **Discoveries**: Notable things learned during the session
- **Accomplished**: What's done, in progress, and remaining
- **Relevant files**: Structured list of files read/edited/created

### Message replay after compaction

After auto-compaction, replay the user's last message so the agent can continue working seamlessly rather than requiring the user to re-state their request.

### Keep recent messages

If full-conversation summarization loses too much conversational continuity, add a `keep_last` parameter to preserve the N most recent messages unsummarized.

### Separate compaction model

Allow configuring a different (cheaper/faster) model for summarization calls, since the summary doesn't need the same capabilities as the main agent.

### Proper tokenizer

Use `tiktoken` or a model-specific tokenizer for accurate counts when precision matters.
