# Heartbeat

## Purpose

The heartbeat is a periodic wake-up mechanism. It allows the agent to perform background tasks without user interaction — checking things, running maintenance, or acting on scheduled items.

## Why Heartbeat?

Unlike cron (which schedules specific commands), heartbeat is agent-driven:

1. **Flexible** — Agent decides what to do based on `HEARTBEAT.md`
2. **Intelligent** — Can skip if nothing needs doing
3. **Conversational** — Tasks are natural language, not shell scripts
4. **Self-managing** — Agent can add/remove its own tasks

## Architecture

```
┌─────────────────────────────────────────────┐
│              systemd timer                  │
│         (every 30 minutes)                  │
└─────────────────────┬───────────────────────┘
                      │
                      ▼
┌─────────────────────────────────────────────┐
│           kitaebot heartbeat                │
│                                             │
│  1. Check repl.lock — skip if held          │
│  2. Acquire heartbeat.lock — skip if held   │
│  3. Read HEARTBEAT.md — skip if missing     │
│  4. Parse active tasks — skip if none       │
│  5. Build prompt from active task lines     │
│  6. Run one agent turn (ephemeral session)  │
│  7. Append result to memory/HISTORY.md      │
└─────────────────────────────────────────────┘
```

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

## Execution Flow

See `src/heartbeat.rs` for the implementation. The core function:

```rust
pub async fn run<P: Provider>(
    workspace: &Workspace, provider: &P, tools: &Tools,
) -> Result<Outcome, Error>
```

Returns `Outcome::Executed(response)` on success or `Outcome::Skipped(reason)` when there is nothing to do. Uses an ephemeral `Session::new()` — heartbeat turns are not persisted.

The prompt is built from active task lines only (`- [ ]` checkboxes), not the full file content. The full agent response is appended to `memory/HISTORY.md` with a UTC timestamp formatted via Hinnant's `civil_from_days` (no `chrono` dependency).

## Systemd Integration

```ini
# /etc/systemd/system/kitaebot-heartbeat.timer
[Unit]
Description=Kitaebot heartbeat timer

[Timer]
OnBootSec=5min
OnUnitActiveSec=30min
Persistent=true

[Install]
WantedBy=timers.target
```

```ini
# /etc/systemd/system/kitaebot-heartbeat.service
[Unit]
Description=Kitaebot heartbeat

[Service]
Type=oneshot
ExecStart=/usr/bin/kitaebot heartbeat
User=kitaebot
WorkingDirectory=/var/lib/kitaebot
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

## MVP Simplifications

For MVP:

1. **Fixed interval** — 30 minutes, not configurable
2. **No task scheduling** — Just "do these things periodically"
3. **Simple parsing** — Look for `- [ ]` checkboxes
4. **Single execution** — No parallelism

## Skipping Heartbeat

Heartbeat is skipped (not an error) when:

1. A user session holds `repl.lock`
2. Another heartbeat holds `heartbeat.lock`
3. `HEARTBEAT.md` doesn't exist
4. No active tasks (no unchecked `- [ ]` lines)

Checks run in this order. Skip reason is printed to stderr.

## Locking

Mutual exclusion uses PID-based lock files (see `src/lock.rs`):

- **`repl.lock`** — Held by user sessions. Heartbeat checks but does not acquire.
- **`heartbeat.lock`** — Acquired for the duration of the heartbeat turn. RAII guard removes on drop.

Stale locks (dead PID) are automatically recovered. `create_new` provides atomic acquisition. This is defense-in-depth — systemd's `Type=oneshot` prevents overlapping runs.

## Logging

Executed heartbeats are logged to `memory/HISTORY.md`. Skip events are printed to stderr but not persisted.

```markdown
[2024-02-21 14:30] Heartbeat: Checked project builds - all passing. No new inbox files.

[2024-02-21 15:30] Heartbeat: Found 3 new files in inbox, summarized and filed.
```

## Future Considerations

- **Configurable interval** — Per-task or global
- **Priority levels** — Some tasks more urgent
- **Conditional tasks** — Only run if condition met
- **External triggers** — Webhooks to trigger heartbeat
- **Resource limits** — Cap API usage per heartbeat
- **Debug logging of skips** — Structured logging for skip events (currently silent)
- **History truncation** — Cap or summarize long agent responses before appending to HISTORY.md
