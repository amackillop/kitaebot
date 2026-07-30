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
├── context/                 # Context-engine-owned storage (spec 14)
│   └── <name>.json          # One file per session
│
├── memory/                  # Memory subsystem (spec 21)
│   ├── MEMORY.md            # Index, injected into the system prompt
│   └── topics/              # On-demand topic files
│
├── state/                   # Machine-owned operational state
│   ├── kitaebot.db          # Operational DB: ledgers + cursor docs
│   ├── JOURNAL.md           # The bot's work record: topic-tagged, append-only
│   └── review-checklist.md  # Escape checklist, derived from the ledger (spec 23)
│
└── projects/                # User's working area
```

### Durable and derived state

Backup and restore ([spec 09](09-vm.md)) turns on this split:

| Durable | Derived |
|---------|---------|
| `context/` — whatever the engine keeps there | `projects/`, `reviews/`, `.diffs/` — re-cloned or regenerated |
| `state/` — `kitaebot.db`, the `.md` logs | build caches (`.cargo`, `.npm`, `.cache`, `.local`) |
| `memory/` | |
| | Nix-provisioned symlinks (`SOUL.md`, `AGENTS.md`, `USER.md`, `config.toml`) |

Durable state measured ~10 MB against ~6 GB of derived, which is what
makes restoring onto a fresh machine cheap. `context/lcm/payloads/` is
the easy one to miss: `large_files` rows reference those blobs, so a
backup without them leaves `lcm_grep` with nothing to search.

### The journal

`state/JOURNAL.md` is the bot's work record: append-only, one
timestamped entry per event, each tagged `[topic]` so one file stays
greppable per concern. Admission rule: work performed, outcomes,
failures, and messages sent to a human. Routine no-ops — a closed
gate, an idle poll — are mechanics and stay in the tracing log; the
`Reply.routine` flag is how a turn marks its own reply as such, since
only the turn knows whether it did work.

Writers and topics: the actor journals every non-routine unattended
turn outcome under its source (`[duty]`, `[github]`, `[linear]`);
the duty scheduler journals mechanical duties directly; the notifier
mirrors every send as `[notify]` (spec 17); distillation passes land
as `[distill]`. Entries are capped at 4000 bytes.

The journal is what makes the bot's autonomous work recountable — a
future standup duty summarizes it since a cursor (FUTURE.md).

### Backup staging

`kitaebot backup <dir>` (src/backup.rs) stages every piece of durable
state into `<dir>`; `vm-backup`'s script only archives the result.
Selection lives in code because the script version drifted — new
state files were silently missing from backups.

Anti-drift is structural where possible and checked where not:
`state/` and `memory/` are snapshotted wholesale (databases via
`VACUUM INTO`, WAL sidecars skipped as subsumed, everything else
copied), so new files there are covered automatically; `context/` is
staged by the active engine's `ContextEngine::backup`, which has no
default — a new engine cannot compile without answering how it is
backed up; and any workspace-root entry that is neither staged nor in
the `DERIVED` registry is warned about on every backup run. Staging
runs without secrets, network, or the sandbox, and is safe against a
live daemon.

### Initialization

`Workspace::init()` resolves the path and delegates to `init_at()`, which
creates the directory tree: workspace root, `context/`, `memory/`,
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
| `context_dir()` | `context/` — handed whole to the engine (spec 14) |
| `state_dir()` | `state/` |
| `journal_path()` | `state/JOURNAL.md` |
| `state_db_path()` | `state/kitaebot.db` |

## Boundaries

### Owns

- Directory structure creation (`context/`, `memory/`, `projects/`, `state/`)
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
