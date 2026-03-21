# Spec 06: System Prompt

## Motivation

The system prompt gives the agent its identity, instructions, and user context.
It is injected into every provider call, shaping all responses across all
channels.

## Behavior

### Prompt Files

Three files are concatenated (in order) to form the system prompt:

| File | Purpose | Required |
|------|---------|----------|
| `SOUL.md` | Personality, values, communication style | No (warned if missing) |
| `AGENTS.md` | Operational instructions, workflow, tool usage guidelines | No (warned if missing) |
| `USER.md` | User profile, preferences | No (warned if missing) |

Files are separated by a single `\n`. Missing files produce a `tracing::warn`
log but do not cause failure — the prompt is assembled from whatever files
exist.

### Assembly and Injection

The system prompt is rebuilt from disk on **every incoming message** (not
cached). Edits to any prompt file take effect on the next turn without restart
or `/new`.

The prompt is prepended as a `Message::System` to every provider call but
**never stored in the session**. This keeps the session clean and allows prompt
changes without invalidating history.

### Content Guidelines

Each file has a distinct role. Examples of what belongs where:

- **`SOUL.md`** — Identity, personality traits, values, communication style
  (e.g. "be concise", "no emojis", "accuracy over speed")
- **`AGENTS.md`** — Operational instructions: tool usage guidelines, developer
  workflow (clone, branch, implement, validate, commit, PR), commit message
  standards, failure handling, exec deny-list workarounds
- **`USER.md`** — User-specific context: name, timezone, preferences,
  project conventions

`HEARTBEAT.md` is provisioned alongside the prompt files but is **not** part of
the system prompt — it is used separately by the heartbeat channel (see
[spec 07](07-heartbeat.md)).

### Provisioning

Prompt files are **not** created by the Rust binary. They are provisioned by
the NixOS module via `systemd.tmpfiles.rules` as symlinks from a configurable
`promptsDir` into the workspace. This keeps content management declarative.

## Boundaries

### Owns

- The separation of concerns: personality (SOUL) vs. instructions (AGENTS) vs.
  user context (USER)
- Default content for each file
- The contract that exactly these three files, in this order, form the prompt

### Does Not Own

- Prompt assembly — the workspace module handles concatenation
- Prompt injection — the agent loop handles prepending to provider calls
- File provisioning — NixOS handles symlink creation

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Prompt file missing | Warn log, file skipped, prompt assembled from rest |
| Prompt file unreadable | Warn log, file skipped |
| All prompt files missing | Empty system prompt (agent still functions) |

## Constraints

- Prompt files should be kept short to conserve tokens in the context window
- The concatenation order (SOUL, AGENTS, USER) is fixed
- File encoding must be UTF-8

## Open Questions

None currently.
