# Spec 07: Heartbeat

## Motivation

The heartbeat is a periodic awareness check. It lets the agent proactively
review its workspace and surface anything that needs attention — without
waiting for user input.

## Behavior

### Timer

A `tokio::time::interval` fires on a configurable cadence (default 30 minutes).
Missed ticks are skipped, not burst. On each tick, the literal string
`"/heartbeat"` is sent through the `AgentHandle` with
`ChannelSource::Heartbeat`.

The first tick fires immediately on daemon startup.

### Execution Flow

1. Actor receives the envelope, classifies `"/heartbeat"` as
   `Input::Command(SlashCommand::Heartbeat)`.
2. **Prepare**: `heartbeat::prepare()` reads `HEARTBEAT.md` and extracts
   active task lines (`- [ ]` checkboxes). Returns `Prepared::Ready(prompt)`
   or `Prepared::Skipped(reason)`.
3. **Execute**: On ready, the command handler calls `agent::process_message()`
   directly (not through the handle — that would deadlock since we're already
   inside the actor). The heartbeat turn runs in the unified session with full
   conversational context.
4. **Finish**: On success, `heartbeat::finish()` appends the response to
   `memory/HISTORY.md` with a UTC timestamp. A finish write failure is logged
   but does not fail the heartbeat.

Because `/heartbeat` routes through the `Command` dispatch branch (not the
`Message` branch), the heartbeat prompt enters the session **without** a
`[Heartbeat]` channel prefix. Source tagging only applies to free-text
messages.

### HEARTBEAT.md Format

Tasks are recurring — they run every heartbeat cycle, not once. Checkboxes act
as an enable/disable toggle: `- [ ]` is enabled, `- [x]` is disabled. The
agent or user toggles checkboxes to activate or deactivate tasks without
deleting them.

```markdown
# Heartbeat Tasks

- [ ] Check if any project builds are failing
- [ ] Summarize any new files in projects/inbox
- [x] Review memory and clean up stale entries
```

### Prompt Construction

Active tasks are extracted and injected into a prompt:

```
This is a heartbeat check. Review the following tasks and handle any
that need attention:

- [ ] Check if any project builds are failing
- [ ] Summarize any new files in projects/inbox
```

### History Logging

Responses are appended to `memory/HISTORY.md`:

```
[2024-02-21T14:30:00Z] Heartbeat: Checked project builds - all passing.

[2024-02-21T15:30:00Z] Heartbeat: Found 3 new files in inbox, summarized.
```

Every successful response is logged — there is no "actionable content" filter.

### Skipping

Heartbeat is skipped (not an error) when:

1. `HEARTBEAT.md` doesn't exist
2. No active tasks (no unchecked `- [ ]` lines)

Skip events are logged at `info` level via the reply path. They are not
persisted to HISTORY.md.

## Boundaries

### Owns

- Timer loop (`poll_loop`) — interval tick, send through handle, log errors
- Task parsing — extract `- [ ]` lines from HEARTBEAT.md
- Prompt construction — build the heartbeat prompt from active tasks
- History logging — append timestamped responses to HISTORY.md

### Does Not Own

- Agent turn execution — delegates to `agent::process_message()`
- Session persistence — the session module handles that
- HEARTBEAT.md content — provisioned by NixOS, edited by user or agent
- Activity events — heartbeat passes `None` for the activity sender

### Interactions

- **Daemon** spawns the `poll_loop` as a concurrent task alongside other
  channels.
- **Actor** processes heartbeat envelopes sequentially with all other channels.
  No lock files needed.
- **Workspace** provides `heartbeat_path()` and `history_path()`.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| HEARTBEAT.md missing | Skip, log, retry next tick |
| HEARTBEAT.md unreadable | `HeartbeatError::ReadTasks`, logged, retry next tick |
| No active tasks | Skip, log, retry next tick |
| Agent turn error | Logged via `tracing::error!`, retry next tick |
| HISTORY.md write failure | `HeartbeatError::WriteHistory`, logged, response still returned |

The heartbeat loop never crashes. All errors are logged and retried on the
next tick.

## Constraints

| Config key | Default | Description |
|------------|---------|-------------|
| `heartbeat.interval_secs` | 1800 | Seconds between ticks (must be > 0) |

There is no `enabled` flag for heartbeat. It naturally no-ops when
HEARTBEAT.md has no active tasks.

## Open Questions

None currently.
