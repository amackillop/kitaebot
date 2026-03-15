# Heartbeat

## Purpose

The heartbeat is a periodic awareness check. It allows the agent to proactively review its workspace and surface anything that needs attention — without waiting for user input.

## Heartbeat vs Cron

These are complementary, not interchangeable:

| Aspect | Heartbeat | Cron |
|--------|-----------|------|
| **Timing** | Approximate intervals | Exact schedules |
| **Session** | Unified (conversational continuity) | Isolated (no context) |
| **Context** | Aware of full conversation history | Standalone |
| **Batching** | Multiple checks per turn | One job per execution |
| **Decision** | Agent decides what needs attention | Executes unconditionally |

Heartbeat is about **awareness within a session**. Cron is about **scheduled independence**.

One heartbeat replaces many small polling tasks. "Check email, review calendar, scan inbox" is one heartbeat turn — cheap and batched — versus three separate cron jobs.

## Architecture

```
┌──────────────────────────────────────────────┐
│            kitaebot run (daemon)             │
│                                              │
│  ┌────────────────────────────────────────┐  │
│  │     tokio::interval (configurable)     │  │
│  └──────────────────┬─────────────────────┘  │
│                     │                        │
│                     ▼                        │
│  1. Send "/heartbeat" through AgentHandle    │
│  2. Actor classifies as slash command        │
│  3. /heartbeat handler:                      │
│     a. prepare() — read HEARTBEAT.md         │
│     b. Skip if no file or no active tasks    │
│     c. Run agent turn with heartbeat prompt  │
│     d. finish() — append to HISTORY.md       │
│  4. Reply logged, errors retried next tick   │
└──────────────────────────────────────────────┘
```

The heartbeat is a thin timer loop (`poll_loop`) that sends `/heartbeat` through the `AgentHandle` on each tick. The actual logic lives in the `/heartbeat` slash command handler, which calls `heartbeat::prepare()` and `heartbeat::finish()`.

Because all messages go through the actor sequentially, there is no need for lock files. The actor naturally serializes heartbeat turns with messages from other channels.

## Unified Session

The heartbeat shares the unified session with all other channels. Messages are tagged with `[Heartbeat]` so the agent can distinguish heartbeat context from user messages. This gives the heartbeat full conversational continuity — it sees prior user conversations, GitHub reviews, and its own previous heartbeat results.

## HEARTBEAT.md Format

Tasks are recurring — they run every heartbeat cycle, not once. Checkboxes
act as an enable/disable toggle: `- [ ]` is enabled, `- [x]` is disabled.
The user (or agent) toggles checkboxes to activate or deactivate tasks
without deleting them. This avoids re-adding tasks with slightly different
wording.

```markdown
# Heartbeat Tasks

Tasks below are checked every 30 minutes.

## Tasks

- [ ] Check if any project builds are failing
- [ ] Summarize any new files in projects/inbox
- [x] Review memory and clean up stale entries
```

## Prompt and Response

`heartbeat::prepare()` reads HEARTBEAT.md, extracts active task lines (`- [ ]` checkboxes), and builds a prompt. If there are no active tasks or no HEARTBEAT.md file, it returns `Prepared::Skipped`.

The `/heartbeat` command handler runs an agent turn with the prepared prompt. If the response contains actionable content, `heartbeat::finish()` appends it to `memory/HISTORY.md` with a UTC timestamp.

## Execution Flow

See `src/heartbeat.rs`. Three public entry points:

- **`prepare`** — reads HEARTBEAT.md, returns either a ready prompt or a skip reason
- **`finish`** — appends a timestamped response to `memory/HISTORY.md`
- **`poll_loop`** — ticks on a configurable interval, sends `/heartbeat` through the handle, logs errors and retries next tick

## Skipping Heartbeat

Heartbeat is skipped (not an error) when:

1. `HEARTBEAT.md` doesn't exist
2. No active tasks (no unchecked `- [ ]` lines)

Skip reason is logged to stderr.

## Logging

Executed heartbeats are logged to `memory/HISTORY.md`. Skip events are printed to stderr but not persisted.

```markdown
[2024-02-21T14:30:00Z] Heartbeat: Checked project builds - all passing. No new inbox files.

[2024-02-21T15:30:00Z] Heartbeat: Found 3 new files in inbox, summarized and filed.
```

## Task Management

The agent can modify `HEARTBEAT.md` using the `exec` tool:

**Add a task:**
```bash
echo "- [ ] New periodic task" >> HEARTBEAT.md
```

**Disable a task:**
```bash
sed -i 's/- \[ \] specific task/- [x] specific task/' HEARTBEAT.md
```

**Enable a task:**
```bash
sed -i 's/- \[x\] specific task/- [ ] specific task/' HEARTBEAT.md
```

**Remove a task permanently:**
```bash
sed -i '/specific task/d' HEARTBEAT.md
```

## Configuration

| Key | Default | Purpose |
|-----|---------|---------|
| `heartbeat.interval_secs` | `1800` | Seconds between heartbeat ticks |

## Future Considerations

- **Priority levels** — Some tasks more urgent
- **Conditional tasks** — Only run if condition met
- **Active hours** — Skip heartbeats outside configured time window (e.g., 9am–10pm)
- **Resource limits** — Cap API usage per heartbeat
- **Session summarization** — Compact old history to prevent unbounded growth
- **Cron jobs** — Separate scheduled task system for exact-time, isolated execution (complement to heartbeat)
