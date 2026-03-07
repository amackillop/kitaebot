# Workspace

## Purpose

The workspace is the agent's home directory. It contains configuration, state, and user files. All agent operations are confined to this directory.

## Why Workspace Isolation?

1. **Security** — Agent can't access system files or other users' data
2. **Predictability** — Agent always knows where it is
3. **Portability** — Workspace can be backed up, moved, or reset
4. **Simplicity** — No complex permission system needed

## Location

Resolved via fallback chain:

1. `KITAEBOT_WORKSPACE` environment variable
2. `$XDG_DATA_HOME/kitaebot`
3. `~/.local/share/kitaebot`

## Directory Structure

```
~/.local/share/kitaebot/     (or KITAEBOT_WORKSPACE)
├── config.toml              # Runtime configuration
│
├── SOUL.md                  # Agent personality (system prompt)
├── AGENTS.md                # Agent instructions
├── USER.md                  # User profile (optional, user-created)
├── HEARTBEAT.md             # Periodic task definitions
│
├── sessions/                # Per-channel conversation history
│   ├── telegram.json
│   ├── socket.json
│   └── heartbeat.json
│
├── locks/                   # PID lock files
│   └── heartbeat.lock
│
├── memory/                  # Shared long-term memory
│   ├── HISTORY.md           # Heartbeat execution log
│   └── daily-*.md           # Auto-created daily logs
│
└── projects/                # User's working area
    └── ...
```

## File Purposes

### Prompt Files

Loaded into the system prompt (concatenated in order):

| File | Purpose | Who Edits |
|------|---------|-----------|
| `SOUL.md` | Personality, values, style | User |
| `AGENTS.md` | Instructions for the agent | User |
| `USER.md` | User profile, preferences | User (optional) |

### State Files

| File | Purpose | Who Edits |
|------|---------|-----------|
| `sessions/*.json` | Per-channel conversation state | Agent (automatic) |
| `locks/*.lock` | Mutual exclusion per channel | Agent (automatic) |
| `memory/HISTORY.md` | Heartbeat execution log | Agent (automatic) |
| `memory/daily-*.md` | Daily logs (timestamped entries) | Agent (automatic) |
| `HEARTBEAT.md` | Periodic task definitions | User or agent |
| `config.toml` | Runtime configuration | User |

### Shared Memory

The `memory/` directory is shared across all channels. Any channel can read or write files here. This is the mechanism for cross-channel knowledge transfer:

- Heartbeat writes execution logs to `memory/HISTORY.md`
- Telegram conversations can write learnings to `memory/`
- Socket sessions can read memory to debug agent behavior

### Memory Search

See [spec 14 (Memory)](14-memory.md) for the full retrieval system. Storage lives in `memory/memory.db` (SQLite with FTS5). The original `memory/*.md` files are migrated on first run and retained as a read-only archive.

### Daily Logs

Auto-created daily log files provide temporal awareness across channels.

- **File:** `memory/daily-YYYY-MM-DD.md` — created on first write each day
- Any channel can append to today's log via the exec tool
- Daily logs are append-only markdown with timestamped entries
- Lives in `memory/` alongside `HISTORY.md` — no new directory needed

The agent's system prompt includes the last 2 days of daily logs (today + yesterday). This gives the agent a sense of "what happened recently" without loading full session history from all channels.

## Initialization

On startup, `Workspace::init()` creates the directory tree and writes default templates for `SOUL.md` and `AGENTS.md` using `create_new` (O_EXCL) — existing files are never overwritten.

Directories created: workspace root, `sessions/`, `locks/`, `memory/`, `projects/`.

## Isolation Enforcement

### Path Traversal

The `exec` tool rejects commands containing `../` to prevent escaping the workspace.

### Shell Restrictions

The `exec` tool always runs with `cwd` = workspace root.

### VM Sandbox

The VM provides the real security boundary:
- Separate filesystem namespace
- Limited network access
- Resource constraints

## Backup & Recovery

Workspace is self-contained. To backup:

```bash
tar -czf kitaebot-backup.tar.gz ~/.local/share/kitaebot/
```

## Future Considerations

- **Git integration** — Auto-commit workspace changes
- **Encryption at rest** — For sensitive data
- **Quota management** — Prevent disk exhaustion
- **Multiple workspaces** — For different projects/personas
