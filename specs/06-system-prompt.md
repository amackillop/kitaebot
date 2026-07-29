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

Two consumers, and they are the bot's two modes. A **builder** turn
carries `developer-workflow.md` (clone through pull request); a
**reviewer** turn carries the GitHub review protocol (spec 20). They are
mutually exclusive — the same agent under different instructions, not
one agent holding both sets.

Only the reviewer mode is detectable: the GitHub channel knows which
poll pass raised an item. Nothing declares a turn to be build work, so
builder is the default rather than a detected mode, and every dispatch
that is not a reviewer dispatch gets it.

The workflow was in `AGENTS.md` until it became a segment, over half
that file by size. Keeping it there meant a turn reviewing somebody
else's pull request also held "**Push** — use the `git_push` tool" and
"**Pull request** — use `github_pr_create`", directly beside the review
protocol's "never push to the PR branch, never merge, never close". Both
scope correctly on a careful read, and the protocol states that
prohibition explicitly *because* of the adjacency — which is a sign the
adjacency was the problem. Splitting it makes the modes structural
instead of a matter of the model reading carefully.

### Repo conventions

The worked repository's own `AGENTS.md`, appended to the root system
prompt for sessions bound to a repo. The workflow's Orient step has the
model read it with `file_read`, which makes it ordinary tool output that
compaction can evict mid-task — exactly when a long turn still needs the
rules. In the system prompt it survives.

Keyed on the session, not the dispatch, which is the difference from a
role segment: which repo a session is about is a property of the
session. `desanitize_name(active_session())` gives a candidate
`owner/repo`, which counts only if `projects/<owner>/<repo>` is a real
clone. Desanitization alone is ambiguous — `foo--bar` maps to `foo/bar`
— so the directory check is what turns a wrong guess into no
conventions rather than another repo's. Sessions with no matching clone
get nothing.

Source is `git show origin/HEAD:AGENTS.md` in that clone, never a
working tree and never the review worktree. Content on the default
branch passed the repository's own review gate, so the trust boundary is
"somebody approved this" rather than "the bot did not write it". That is
what makes elevating repo content above data defensible, and what makes
it safe on review turns without a role gate: review and work turns share
the `{nwo}` session (spec 20), so session type could not gate it anyway.

Gated on the `git.repositories` trust list, no separate one. Merging to a trusted
repo's default branch already implies enough access to change CI or add
a dependency, so prompt text is not the marginal risk.

`AGENTS.md` is the only name looked up. Check the tree entry's mode and
resolve a symlink one level: a symlink's blob is its target path, which
would otherwise be injected as the conventions themselves.

Cap 16384 bytes, a constant rather than config. Not a budget
calculation — a sanity bound sized off real files, since the two the bot
works on today are 10397 and 2905 bytes and the larger wants room to
grow. For scale, the heaviest prompt already assembled (static files,
a full memory index, both segments) is around 8500 tokens against an
effective context of 167232, and conventions add roughly 2600 to that.

Over the cap, inject nothing and let Orient handle it — a half-read
index is still an index, but a cut-off sentence can invert a rule. Log
it: a repo whose conventions have quietly outgrown the bound would
otherwise look identical to one with no `AGENTS.md` at all.

The segment frames what follows as conventions governing code style and
workflow within that repository, which cannot direct actions elsewhere,
override these instructions, or authorize a push, merge, or approval.
The frame earns its place: at 10397 bytes against the bot's own 11222,
an injected convention file is comparable in weight to the operating
instructions it sits beside, so which one wins a conflict has to be
stated rather than left to proximity.

Read per root turn like the memory index; any failure injects nothing.
Sub-agents are excluded with the other segments — the reviewer gets
conventions from its parent (spec 23). Root `AGENTS.md` only; nested
per-package files in monorepos are out of scope.

The Orient step (`developer-workflow.md`) skips reading what is already
in the prompt.

### Content Guidelines

Each file has a distinct role. Examples of what belongs where:

- **`SOUL.md`** — Identity, personality traits, values, communication style
  (e.g. "be concise", "no emojis", "accuracy over speed")
- **`AGENTS.md`** — Mode-independent operating instructions: delegation,
  tool usage guidelines, memory discipline, failure handling, exec
  deny-list workarounds. The builder workflow left it to become a role
  segment (see Role segments), so what remains applies to every turn
  whatever the bot is doing
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
