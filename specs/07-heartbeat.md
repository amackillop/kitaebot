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
│              systemd timer                   │
│         (every 30 minutes)                   │
└─────────────────────┬───────────────────────┘
                      │
                      ▼
┌─────────────────────────────────────────────┐
│           kitaebot heartbeat                 │
│                                              │
│  1. Read HEARTBEAT.md                        │
│  2. If tasks exist, send to agent            │
│  3. Agent processes and responds             │
│  4. Log result to HISTORY.md                 │
└─────────────────────────────────────────────┘
```

## HEARTBEAT.md Format

```markdown
# Heartbeat Tasks

Tasks below are checked every 30 minutes.

## Active Tasks

- [ ] Check if any project builds are failing
- [ ] Summarize any new files in projects/inbox
- [ ] Review memory and clean up stale entries

## Completed

<!-- Agent moves completed tasks here -->
```

## Execution Flow

```rust
async fn run_heartbeat(workspace: &Path) -> Result<()> {
    let heartbeat = fs::read_to_string(workspace.join("HEARTBEAT.md"))?;

    // Check if there are any tasks
    if !has_active_tasks(&heartbeat) {
        log::debug!("No heartbeat tasks, skipping");
        return Ok(());
    }

    // Build prompt for agent
    let prompt = format!(
        "This is a heartbeat check. Review the following tasks and handle any that need attention:\n\n{}",
        heartbeat
    );

    // Run agent with heartbeat context
    let response = agent.process_message(&prompt, "heartbeat").await?;

    // Log to history
    append_to_history(workspace, &format!(
        "[{}] Heartbeat: {}\n",
        Utc::now().format("%Y-%m-%d %H:%M"),
        summarize(&response)
    ))?;

    Ok(())
}
```

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

**Remove a task:**
```bash
sed -i '/specific task/d' HEARTBEAT.md
```

**Mark complete:**
The agent moves tasks to the Completed section.

## MVP Simplifications

For MVP:

1. **Fixed interval** — 30 minutes, not configurable
2. **No task scheduling** — Just "do these things periodically"
3. **Simple parsing** — Look for `- [ ]` checkboxes
4. **Single execution** — No parallelism

## Skipping Heartbeat

Heartbeat is skipped when:

1. `HEARTBEAT.md` doesn't exist
2. No active tasks (all completed or only headers)
3. Agent is currently in a user session
4. Previous heartbeat still running

## Logging

All heartbeat activity is logged to `memory/HISTORY.md`:

```markdown
[2024-02-21 14:30] Heartbeat: Checked project builds - all passing. No new inbox files.

[2024-02-21 15:00] Heartbeat: Skipped - no active tasks.

[2024-02-21 15:30] Heartbeat: Found 3 new files in inbox, summarized and filed.
```

## Future Considerations

- **Configurable interval** — Per-task or global
- **Priority levels** — Some tasks more urgent
- **Conditional tasks** — Only run if condition met
- **External triggers** — Webhooks to trigger heartbeat
- **Resource limits** — Cap API usage per heartbeat
