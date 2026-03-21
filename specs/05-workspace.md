# Spec 05: Workspace

## Motivation

The workspace is the agent's home directory — configuration, state, prompt
files, and user projects all live here. All agent operations are confined to
this directory (enforced by Landlock, see [spec 15](15-sandbox.md)).

## Behavior

### Location

Resolved via fallback chain:

1. `KITAEBOT_WORKSPACE` environment variable
2. `$XDG_DATA_HOME/kitaebot`
3. `~/.local/share/kitaebot`

### Directory Structure

```
<workspace>/
├── config.toml              # Runtime configuration (Nix-provisioned)
│
├── SOUL.md                  # Agent personality (Nix-provisioned)
├── AGENTS.md                # Agent instructions (Nix-provisioned)
├── USER.md                  # User profile (Nix-provisioned, optional)
├── HEARTBEAT.md             # Periodic task definitions (Nix-provisioned)
│
├── sessions/                # Session storage
│   └── session.json         # Unified session (all channels)
│
├── memory/                  # Shared long-term memory
│   ├── HISTORY.md           # Heartbeat execution log
│   └── github_poll_state.json  # GitHub channel poll cursor
│
└── projects/                # User's working area
```

### Initialization

`Workspace::init()` resolves the path and delegates to `init_at()`, which
creates the directory tree: workspace root, `sessions/`, `memory/`, `projects/`.

Prompt files (`SOUL.md`, `AGENTS.md`, `USER.md`, `HEARTBEAT.md`) and
`config.toml` are **not** created by the Rust binary. They are provisioned
externally by the NixOS module via `systemd.tmpfiles.rules` as symlinks into
the Nix store. This keeps content management declarative and outside the
binary's responsibility.

Workspace init failure is fatal — the process exits.

### System Prompt Assembly

`system_prompt()` concatenates files in order:

1. `SOUL.md` — personality, values, style
2. `AGENTS.md` — instructions for the agent
3. `USER.md` — user profile, preferences

Files are separated by a single `\n`. Missing files produce a `warn` log but
are not fatal — the function returns whatever it could read, possibly empty.

The system prompt is assembled fresh on every provider call (not cached). Edits
to prompt files take effect on the next turn without restart.

### Path Helpers

| Method | Returns |
|--------|---------|
| `path()` | Workspace root |
| `session_path()` | `sessions/session.json` |
| `heartbeat_path()` | `HEARTBEAT.md` |
| `history_path()` | `memory/HISTORY.md` |
| `github_poll_state_path()` | `memory/github_poll_state.json` |

## Boundaries

### Owns

- Directory structure creation (`sessions/`, `memory/`, `projects/`)
- Path resolution (env var / XDG fallback)
- System prompt assembly (concatenation of prompt files)
- Path helpers for well-known files

### Does Not Own

- Prompt file content — provisioned by NixOS
- Config file content — provisioned by NixOS
- Session persistence — the session module handles that
- Filesystem confinement — Landlock handles that
- File content written by the agent — tools handle that

### Interactions

- **Landlock sandbox** receives the workspace path and grants full access
  within it. All other filesystem access is restricted.
- **Agent actor** calls `system_prompt()` on each turn and `session_path()`
  for session load/save.
- **Heartbeat** uses `heartbeat_path()` and `history_path()`.
- **GitHub channel** uses `github_poll_state_path()` for poll cursor
  persistence.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Workspace path unresolvable (no env var, no HOME) | Fatal exit |
| Directory creation fails | Fatal exit (`WorkspaceError::Init`) |
| Prompt file missing | Warn log, prompt assembled from remaining files |
| Prompt file read error | Warn log, file skipped |

## Constraints

- Workspace must exist before the agent starts (init is synchronous at
  startup)
- Prompt files are expected to be provisioned externally — the binary creates
  no files, only directories
- `system_prompt()` never fails — it degrades gracefully to an empty string

## Open Questions

None currently.
