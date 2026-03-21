# Spec 17: Notify

## Status: Not Implemented

This spec describes a planned tool that does not yet exist.

## Motivation

The `notify` tool lets the agent push a message to the user outside the current
request-reply flow. Without it, a heartbeat finding gets logged but doesn't
reach the user. With `notify`, the agent can escalate to the user's phone.

Making notification a tool means the agent decides when something is worth
interrupting the user — no configuration for "forward heartbeat results if
non-trivial."

## Planned Design

### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `message` | String | yes | Content to send |
| `urgency` | String | no | `low` (default) or `high` |

### Urgency

- **`low`** — batched. Accumulated and delivered as a single Telegram message
  at the end of the current turn.
- **`high`** — immediate. Sent as soon as the tool executes.

### Sink

Telegram via `sendMessage` to the configured `chat_id`. Reuses the existing
`TelegramClient`. If Telegram is disabled, the tool is not registered.

### Rate Limiting

Max 5 notifications per turn. Counter lives in the actor, resets per envelope.

### Batching

The actor owns the low-urgency buffer. After the turn completes (success or
error), buffered messages are joined with `\n\n` and sent as a single Telegram
message.

### Error Handling

| Error | Behavior |
|-------|----------|
| Telegram API failure | Error text returned to agent |
| Rate limit exceeded | Error text returned to agent |
| No sink configured | Tool not registered |

### Configuration

No new config keys. Tool availability is derived from `telegram.enabled`.

## Open Questions

1. Should there be a sink abstraction (trait/enum), or is Telegram-only fine
   until a second backend exists?
2. Should batched messages be flushed on turn error, or only on success?
