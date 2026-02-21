# Workspace

## Purpose

The workspace is the agent's home directory. It contains configuration, state, and user files. All agent operations are confined to this directory.

## Why Workspace Isolation?

1. **Security** — Agent can't access system files or other users' data
2. **Predictability** — Agent always knows where it is
3. **Portability** — Workspace can be backed up, moved, or reset
4. **Simplicity** — No complex permission system needed

## Directory Structure

```
/var/lib/kitaebot/
├── config.toml          # Agent configuration
├── session.json         # Conversation history
│
├── SOUL.md              # Agent personality (system prompt)
├── USER.md              # User profile
├── AGENTS.md            # Agent instructions
├── TOOLS.md             # Tool documentation (generated)
├── HEARTBEAT.md         # Periodic tasks
│
├── memory/
│   ├── MEMORY.md        # Long-term facts
│   └── HISTORY.md       # Event log
│
└── projects/            # User's working area
    └── ...
```

## File Purposes

### Core Files

| File | Purpose | Who Edits |
|------|---------|-----------|
| `config.toml` | Runtime configuration | User |
| `session.json` | Conversation state | Agent (automatic) |

### Prompt Files

These are loaded into the system prompt:

| File | Purpose | Who Edits |
|------|---------|-----------|
| `SOUL.md` | Personality, values, style | User |
| `USER.md` | User profile, preferences | User |
| `AGENTS.md` | Instructions for the agent | User |
| `TOOLS.md` | Tool documentation | Generated |

### Memory Files

| File | Purpose | Who Edits |
|------|---------|-----------|
| `memory/MEMORY.md` | Consolidated long-term facts | Agent |
| `memory/HISTORY.md` | Append-only event log | Agent |
| `HEARTBEAT.md` | Periodic tasks | Agent |

### User Files

The `projects/` directory is for user work. The agent can create, read, and modify files here.

## Bundled Templates

The VM includes template versions of workspace files. On first boot:

1. Check if workspace exists
2. If not, copy templates to `/var/lib/kitaebot/`
3. Set appropriate permissions

Templates come from `workspace/` in the source repo (same as nanobot).

## Isolation Enforcement

### Path Validation

All file operations validate paths:

```rust
fn validate_path(workspace: &Path, requested: &Path) -> Result<PathBuf> {
    let canonical = requested.canonicalize()?;

    if !canonical.starts_with(workspace) {
        return Err(Error::PathTraversal);
    }

    Ok(canonical)
}
```

### Shell Restrictions

The `exec` tool always runs with:
- `cwd` = workspace
- No access to parent directories
- PATH restricted to safe commands

### Nix Sandbox

The VM provides additional isolation:
- Separate filesystem namespace
- Limited network access
- Resource constraints

## Permissions

```
/var/lib/kitaebot/           drwxr-xr-x  kitaebot:kitaebot
├── config.toml              -rw-------  (contains API key)
├── session.json             -rw-r--r--
├── SOUL.md                  -rw-r--r--
├── memory/                  drwxr-xr-x
│   └── ...                  -rw-r--r--
└── projects/                drwxr-xr-x
    └── ...                  -rw-r--r--
```

## Backup & Recovery

Workspace is self-contained. To backup:

```bash
tar -czf kitaebot-backup.tar.gz /var/lib/kitaebot/
```

To restore:

```bash
tar -xzf kitaebot-backup.tar.gz -C /
```

## Reset

To start fresh:

```bash
rm -rf /var/lib/kitaebot/*
systemctl restart kitaebot
# Templates will be re-copied
```

## Future Considerations

- **Git integration** — Auto-commit workspace changes
- **Encryption at rest** — For sensitive data
- **Quota management** — Prevent disk exhaustion
- **Multiple workspaces** — For different projects/personas
