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

The system prompt is read from disk **once at startup** and cached for
the process lifetime. Prompt files are provisioned from the Nix store,
so changing them requires a rebuild and restart regardless.

The one dynamic exception is the memory index (`memory/MEMORY.md`),
appended after the static files and re-read each turn because the
agent writes it at runtime — see [spec 21](21-memory.md).

The prompt is prepended as a `Message::System` to every provider call but
**never stored in the session**. This keeps the session clean and allows prompt
changes without invalidating history.

### Role segments

Compiled prompt segments appended to the root system prompt when the
turn is of a matching kind. The key is the **dispatch**, not the
session: the channel that raised the turn declares what the bot is
being asked to be, and that declaration rides on the envelope.

Keyed this way after an earlier revision keyed it on the session name
(`review:{nwo}` sessions took the review segment). Two problems. It
depended on a naming convention surviving `sanitize_name`, which
needed a test to pin behaviour nothing else relied on; and it assumed
one role per conversation, which stopped being true when review turns
moved onto the repo's work session (spec 20). A session is where
history accumulates. A role is a property of the turn.

The mechanism mirrors the review-gates segment (spec 23): a compiled
`include_str!` const, appended in `process_message_metered`, gated on
a condition — there config, here the dispatched role. Segments carry
static choreography that would otherwise ride in every dispatch User
message and accumulate in the session until compaction; as
system-prompt segments they are paid once per request and never
compacted away. Dispatch messages shrink to per-turn facts.

First and only consumer: the GitHub review protocol (spec 20), on
dispatches where the bot is the reviewer and not the author.
Deliberately not consumed: injecting the worked repo's own
`AGENTS.md`/`CLAUDE.md` at system-prompt level — repo content is
data, and elevating it above data is a prompt-injection surface with
its own gating decision (FUTURE.md, System Prompt).

### Content Guidelines

Each file has a distinct role. Examples of what belongs where:

- **`SOUL.md`** — Identity, personality traits, values, communication style
  (e.g. "be concise", "no emojis", "accuracy over speed")
- **`AGENTS.md`** — Operational instructions: tool usage guidelines, developer
  workflow (clone, branch, implement, validate, commit, PR), commit message
  standards, failure handling, exec deny-list workarounds
- **`USER.md`** — User-specific context: name, timezone, preferences,
  project conventions

(`HEARTBEAT.md` was provisioned alongside the prompt files until the
duty scheduler retired it — see [spec 24](24-self-directed-work.md).)

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
