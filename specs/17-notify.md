# Notify

## Purpose

The `notify` tool lets the agent push a message to the user outside the current request-reply flow. Without it, the agent can only respond to whoever asked — a heartbeat finding gets logged, a GitHub review response goes nowhere visible. With `notify`, the agent can escalate: "I found a failing build during heartbeat, here's what I know" lands on the user's phone.

## Why a Tool?

The agent already decides *what* to do during a turn. Making notification a tool means the agent also decides *when* something is worth interrupting the user. No configuration for "forward heartbeat results if non-trivial" — the agent judges that itself, in context.

A tool also composes naturally with every channel. Heartbeat, GitHub, socket — any turn can notify. No special routing rules per channel.

## Behavior

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `message` | `String` | yes | Content to send to the user |
| `urgency` | `String` | no | `low` (default) or `high` — determines delivery policy |

The tool sends `message` to the configured notification sink (Telegram by default). Returns a confirmation string on success, error text on failure. The agent sees the result and can retry or rephrase.

### Urgency

- **`low`** — batched. Messages are accumulated and delivered as a single combined Telegram message at the end of the current turn. Prevents spam during chatty heartbeat turns. Individual messages are joined with blank lines.
- **`high`** — immediate. Sent as soon as the tool executes. Use for blockers, failures, or anything the user should see right now.

The LLM's system prompt should describe when to use each level. Example guidance: "Use `high` for errors, blockers, or questions you need answered. Use `low` for status updates and informational findings."

## Notification Sink

The sink is the delivery backend. Telegram is the only sink for now. The tool holds a reference to the sink at construction time, injected by the runtime.

### Telegram Sink

Sends via `sendMessage` to the configured `telegram.chat_id`. Reuses the existing `TelegramClient` — no new HTTP client, no new credentials.

If Telegram is disabled (`telegram.enabled = false`), the tool is not registered. The agent cannot call what doesn't exist in its toolbox.

### Future Sinks

The sink could be a trait or an enum. Not worth abstracting until there's a second backend. Candidates:

- **Ntfy** — push notifications without a bot, works with any device
- **Email** — SMTP, good for non-urgent summaries
- **Desktop** — D-Bus notification on the host (via socket channel? via exec?)

## Construction

The runtime builds the `Notify` tool alongside the other tools. It needs a clone of the `TelegramClient` and the `chat_id`. If Telegram is disabled and no other sink is configured, the tool is omitted from the toolbox entirely.

## Rate Limiting

Max 5 notifications per agent turn. After the limit, the tool returns an error ("notification limit reached for this turn"). Prevents runaway loops where the agent decides everything is worth notifying about.

The counter lives in the actor. It resets at the start of each envelope. The actor passes a shared counter (or a flush callback) to the tool at execution time, and flushes batched `low` messages after the turn completes.

## Batching

The actor owns the low-urgency buffer (`Vec<String>`). When the tool executes with `urgency: low`, it appends to this buffer and returns immediately with "queued for delivery". After the turn completes (agent loop returns), the actor joins all buffered messages with `\n\n`, sends a single Telegram message, and clears the buffer. If the turn errors out, buffered notifications are still flushed — the agent wanted them sent.

## Interaction with Channels

| Channel | Typical use |
|---------|-------------|
| Heartbeat | "Build X is failing, needs your attention" |
| GitHub | "I'm stuck on PR #42 review — reviewer asked something I can't answer" |
| Socket | Less useful — user is already connected. But could notify on long tool executions. |
| Telegram | Available but rarely useful — user is already the recipient. Makes sense for deferred follow-ups ("I finished that task you asked about earlier"). |

## System Prompt Guidance

The SOUL.md or AGENTS.md should include instructions like:

> You have a `notify` tool to send push notifications to the user. Use it when:
> - A heartbeat check finds something that needs human attention
> - You're stuck and need input during autonomous work
> - A long-running operation completes or fails
>
> Don't notify for routine findings or status-quo confirmations. If nothing needs attention, stay quiet.

The agent's judgment here is the whole point. Over-notification is worse than under-notification.

## Error Handling

| Error | Behavior |
|-------|----------|
| Telegram API failure | Return error text to agent. Agent can retry or skip. |
| Rate limit exceeded | Return "notification limit reached" to agent. |
| No sink configured | Tool not registered — agent never sees it. |

## Configuration

No new config keys. The tool's availability is derived from existing config:

- `telegram.enabled = true` → Telegram sink available → tool registered
- `telegram.enabled = false` and no other sink → tool omitted

Future: a `notify.sink` config key if multiple backends exist.

## Simplifications

1. **Telegram only** — one sink, no abstraction
2. **No delivery confirmation** — fire and hope. Telegram's API is reliable enough.
3. **No user reply path** — notification is one-way. If the user wants to respond, they message normally on Telegram. The unified session means the agent will see it in context.
4. **No scheduling** — no "notify me at 5pm". The heartbeat already handles periodic checks.
5. **No per-channel sink routing** — all notifications go to the same place regardless of which channel triggered them
