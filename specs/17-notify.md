# Spec 17: Notify

## Motivation

The `notify` tool lets the agent push a message to the user outside the current
request-reply flow. Without it, a heartbeat finding gets logged but doesn't
reach the user, and a blocked Linear work item can't ping the user's phone.

Making notification a tool means the agent decides when something is worth
interrupting the user — no configuration for "forward heartbeat results if
non-trivial." The notification is the attention tap; the substance belongs in
the channel reply (e.g. the Linear issue comment).

## Behavior

### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `message` | String | yes | Content to send |
| `urgency` | String | no | `low` (default) or `high` |

### Urgency

- **`low`** — batched. Accumulated and delivered as a single Telegram message
  after the turn completes.
- **`high`** — immediate. Sent as soon as the tool executes.

### Sink

Telegram via `sendMessage` to the configured `telegram.chat_id`, reusing
`TelegramClient`. If Telegram is disabled, the tool is not registered.

Sends are plain text (no parse mode) and single-attempt — they bypass the
Telegram channel's retry and HTML-escape layer deliberately. Notification
delivery is best-effort; the agent sees the error text and can decide whether
to retry. There is no sink abstraction — Telegram is the only backend until a
second one exists.

Every outgoing text (immediate sends and the drained batch) is truncated at
4000 bytes with the standard truncation marker, keeping it under Telegram's
4096-character message cap.

### Rate Limiting

Max 5 notify calls per turn, both urgencies counted. A failed send still
consumes a slot — this stops the agent from burning the Telegram API with
retries. Exceeding the limit returns error text to the agent.

### Architecture

The tool is registered once at startup, but the batch buffer and rate counter
are per-turn. A `Notifier` owns the `TelegramClient`, the chat id, and a
mutex-guarded state (attempt counter + batch). It is shared as `Arc<Notifier>`
between:

- `NotifyTool`, in the tool registry — `execute` records the call against the
  state (pure transition: send-now / buffered / rate-limited) and performs the
  immediate send for `high`;
- the actor, which resets the state before each turn and flushes the batch
  after the turn completes.

The state transitions are pure and synchronous; the lock is never held across
an await. Tool calls within a turn run in parallel, so the mutex is load-bearing.

`ToolCtx` (spec 03) stays generic — notify state is not threaded through the
per-turn tool context.

### Batching

The actor flushes after every turn — success, error, and cancellation alike
(commands never run tools, so no flush there). Buffered messages are joined
with `\n\n` and sent as one Telegram message. A flush failure is logged and
dropped; the turn is already over and there is no one to hand the error to.

### Harness alerts on failed unattended turns

The model is not the only notification source: a turn can fail in a way
the model cannot report — the policy gate halts it (spec 01), or the
provider errors out. On attended channels (Socket, Telegram) a human
reads the reply, so nothing extra is needed. On unattended channels
(Heartbeat, GitHub, Linear) the reply goes nowhere and the failure would
vanish into the logs — the actor sends an immediate alert via the shared
`Notifier` instead: source plus a one-line failure description
(`GitHub PR #141: turn halted by policy gate: ...`). The 4000-byte send
cap applies as usual.

Alerts are harness-initiated, so they bypass the per-turn rate counter:
the actor sends at most one per turn, and the counter exists to stop the
*model* from hammering the API. Delivery is best-effort, like the batch
flush. With Telegram disabled there is no `Notifier` and no alert — same
as the tool.

### Model disclosure on unattended turns

The harness alert covers only failure. A turn that *succeeds* by working
around a tooling defect discloses nothing: the reply goes unread and the
defect survives. AGENTS.md therefore directs the model to `notify` when
it hits or routes around a malfunction of its own tooling on an
unattended turn. Prompted, not enforced — the harness cannot tell a
workaround from ordinary work.

### Scope

Root agent only. Sub-agent tool sets (spec 19) are explicit allowlists that
do not include `notify` — children report to their parent, not to the user's
phone.

## Failure Modes

| Error | Behavior |
|-------|----------|
| Telegram API failure (immediate send) | `ExecutionFailed` with error text returned to agent; the attempt still counts |
| Telegram API failure (batch flush) | Logged, dropped |
| Rate limit exceeded | `ExecutionFailed` error text returned to agent |
| No sink configured | Tool not registered |

## Constraints

No new config keys. Tool availability is derived from `telegram.enabled`.
Note: with Telegram disabled, listing `notify` in `tools.disabled` is a
config error (unknown tool) — same behavior as any other conditionally
registered tool.
