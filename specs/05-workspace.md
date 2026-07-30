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
│
├── sessions/                # Flat-engine session storage
│   └── <name>.json          # One file per session
│
├── memory/                  # Memory subsystem (spec 21)
│   ├── MEMORY.md            # Index, injected into the system prompt
│   └── topics/              # On-demand topic files
│
├── state/                   # Machine-owned runtime state
│   ├── active_session       # Last active session name
│   ├── lcm.db               # LCM engine store (+ lcm/ payloads)
│   ├── kitaebot.db          # Operational DB: ledgers + cursor docs
│   ├── HISTORY.md           # Duty execution log (spec 24)
│   ├── NOTIFICATIONS.md     # Mirror of sent notifications (spec 17)
│   └── review-checklist.md  # Escape checklist, derived from the ledger (spec 23)
│
└── projects/                # User's working area
```

### Durable and derived state

Backup and restore ([spec 09](09-vm.md)) turns on this split:

| Durable | Derived |
|---------|---------|
| `state/` — the two databases, `state/lcm/payloads/`, the `.md` logs | `projects/`, `reviews/`, `.diffs/` — re-cloned or regenerated |
| `memory/` | build caches (`.cargo`, `.npm`, `.cache`, `.local`) |
| | Nix-provisioned symlinks (`SOUL.md`, `AGENTS.md`, `USER.md`, `config.toml`) |

Durable state measured ~10 MB against ~6 GB of derived, which is what
makes restoring onto a fresh machine cheap. `state/lcm/payloads/` is the
easy one to miss: `large_files` rows reference those blobs, so a backup
without them leaves `lcm_grep` with nothing to search.

### Initialization

`Workspace::init()` resolves the path and delegates to `init_at()`, which
creates the directory tree: workspace root, `sessions/`, `memory/`,
`projects/`, `state/`.

Prompt files (`SOUL.md`, `AGENTS.md`, `USER.md`) and
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

The system prompt is read once at workspace init and cached for the
process lifetime. Prompt files are provisioned from the Nix store, so
changing them requires a rebuild and restart regardless.

### Path Helpers

| Method | Returns |
|--------|---------|
| `path()` | Workspace root |
| `sessions_dir()` | `sessions/` |
| `state_dir()` | `state/` |
| `history_path()` | `state/HISTORY.md` |
| `notifications_path()` | `state/NOTIFICATIONS.md` |
| `state_db_path()` | `state/kitaebot.db` |

## Boundaries

### Owns

- Directory structure creation (`sessions/`, `memory/`, `projects/`, `state/`)
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
