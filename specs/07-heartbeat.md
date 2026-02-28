# Heartbeat

## Purpose

The heartbeat is a periodic awareness check. It allows the agent to proactively review its workspace and surface anything that needs attention — without waiting for user input.

## Heartbeat vs Cron

These are complementary, not interchangeable:

| Aspect | Heartbeat | Cron |
|--------|-----------|------|
| **Timing** | Approximate intervals | Exact schedules |
| **Session** | Persistent (conversational continuity) | Isolated (no context) |
| **Context** | Aware of prior heartbeat history | Standalone |
| **Batching** | Multiple checks per turn | One job per execution |
| **Decision** | Agent decides what needs attention | Executes unconditionally |

Heartbeat is about **awareness within a session**. Cron is about **scheduled independence**.

One heartbeat replaces many small polling tasks. "Check email, review calendar, scan inbox" is one heartbeat turn — cheap and batched — versus three separate cron jobs.

## Architecture

```
┌─────────────────────────────────────────────┐
│            kitaebot run (daemon)            │
│                                             │
│  ┌───────────────────────────────────────┐  │
│  │        tokio::interval (30min)        │  │
│  └───────────────────┬───────────────────┘  │
│                      │                      │
│                      ▼                      │
│  1. Acquire heartbeat.lock — skip if held   │
│  2. Read HEARTBEAT.md — skip if missing     │
│  3. Parse active tasks — skip if none       │
│  4. Load sessions/heartbeat.json            │
│  5. Build prompt from active task lines     │
│  6. Run agent turn (persistent session)     │
│  7. Save session                            │
│  8. Append result to memory/HISTORY.md      │
│  9. Release heartbeat.lock                  │
└─────────────────────────────────────────────┘
```

The heartbeat runs inside the daemon process (`kitaebot run`) as a `tokio::interval` timer, not as an external systemd timer. This simplifies deployment and lets the heartbeat share the daemon's provider and tool instances.

## Persistent Session

The heartbeat has its own persistent session at `sessions/heartbeat.json`. This gives it conversational continuity across runs — the agent can reason about changes over time:

- "I checked the builds an hour ago and they were passing. Now they're failing."
- "I already summarized these inbox files last cycle, skipping."
- "The user asked me to watch this metric — it's been stable for 3 cycles."

Without session persistence, every heartbeat is amnesiac and the agent repeats work or misses trends.

### Session Growth

The heartbeat session will grow unboundedly. This is a known concern, addressed later via summarization/truncation (see [04-session.md](04-session.md)). Don't make the heartbeat stateless to avoid this problem — the value of context outweighs the cost of managing session size.

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

The heartbeat prompt is built from active task lines only (`- [ ]` checkboxes), not the full file content. The agent is instructed:

> Read HEARTBEAT.md if it exists. Follow it strictly. Do not infer or repeat old tasks from prior chats. If nothing needs attention, reply HEARTBEAT_OK.

If the agent responds with only `HEARTBEAT_OK`, nothing is delivered — the heartbeat is silent. If the response contains actionable content, it is appended to `memory/HISTORY.md` with a UTC timestamp.

## Execution Flow

See `src/heartbeat.rs` for the implementation. The core function:

```rust
pub async fn run<P: Provider>(
    workspace: &Workspace, provider: &P, tools: &Tools,
) -> Result<Outcome, Error>
```

Returns `Outcome::Executed(response)` on success or `Outcome::Skipped(reason)` when there is nothing to do.

The full agent response is appended to `memory/HISTORY.md` with a UTC timestamp formatted via Hinnant's `civil_from_days` (no `chrono` dependency).

## Skipping Heartbeat

Heartbeat is skipped (not an error) when:

1. Another heartbeat holds `locks/heartbeat.lock`
2. `HEARTBEAT.md` doesn't exist
3. No active tasks (no unchecked `- [ ]` lines)

Checks run in this order. Skip reason is logged to stderr.

Note: the heartbeat no longer checks for the REPL lock. The REPL and heartbeat use separate sessions and can run concurrently. The lock only prevents two heartbeat turns from overlapping.

## Locking

The heartbeat acquires `locks/heartbeat.lock` for the duration of a turn. This prevents overlapping heartbeat runs (e.g., if a turn takes longer than the interval).

See `src/lock.rs` for the PID-based file lock implementation. RAII guard removes the lock on drop. Stale locks (dead PID) are automatically recovered.

## Logging

Executed heartbeats are logged to `memory/HISTORY.md`. Skip events are printed to stderr but not persisted.

```markdown
[2024-02-21 14:30] Heartbeat: Checked project builds - all passing. No new inbox files.

[2024-02-21 15:30] Heartbeat: Found 3 new files in inbox, summarized and filed.
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

1. **Fixed interval** — 30 minutes, not configurable
2. **No task scheduling** — Just "do these things periodically"
3. **Simple parsing** — Look for `- [ ]` checkboxes
4. **Single execution** — No parallelism
5. **No HEARTBEAT_OK suppression** — All responses logged (suppression added later)

## Future Considerations

- **Configurable interval** — Per-task or global
- **Priority levels** — Some tasks more urgent
- **Conditional tasks** — Only run if condition met
- **Active hours** — Skip heartbeats outside configured time window (e.g., 9am–10pm)
- **Resource limits** — Cap API usage per heartbeat
- **HEARTBEAT_OK suppression** — Don't log or deliver when nothing needs attention
- **Session summarization** — Compact old heartbeat history to prevent unbounded growth
- **Cron jobs** — Separate scheduled task system for exact-time, isolated execution (complement to heartbeat)
