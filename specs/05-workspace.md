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
├── session.json             # Conversation history
│
├── SOUL.md                  # Agent personality (system prompt)
├── AGENTS.md                # Agent instructions
├── USER.md                  # User profile (optional, user-created)
│
├── memory/                  # (Future) Long-term memory
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
| `session.json` | Conversation state | Agent (automatic) |

## Initialization

On startup, `Workspace::init()` creates the directory tree and writes default templates for `SOUL.md` and `AGENTS.md` using `create_new` (O_EXCL) — existing files are never overwritten.

Directories created: workspace root, `memory/`, `projects/`.

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

- **config.toml** — Runtime configuration file for model, tokens, temperature
- **TOOLS.md** — Generated tool documentation included in system prompt
- **HEARTBEAT.md** — Periodic task definitions
- **Git integration** — Auto-commit workspace changes
- **Encryption at rest** — For sensitive data
- **Quota management** — Prevent disk exhaustion
- **Multiple workspaces** — For different projects/personas
