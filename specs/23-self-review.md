# Spec 23: Self-Review

## Motivation

The goal is PRs that need minimal human feedback. Every finding a human
reviewer makes costs a round-trip measured in hours; the same finding
caught locally costs one sub-agent call. Three observations drive the
design:

1. **Same-context self-review is weak.** The current workflow (AGENTS.md
   step 7) has the agent review its own staged diff in the same context
   that wrote it. The model is anchored on its own reasoning trace and
   grades its own homework generously. A fresh context reviewing a packed
   diff is structurally more skeptical — the convergent pattern across
   grok-build, Claude Code, and opencode review flows.

2. **Review before publication keeps history clean.** Findings fixed in
   the staged diff or by amend never existed as far as git is concerned.
   GitHub-mediated self-review would append review-fix commits and pollute
   history; local review produces the corrected commit directly.

3. **Atomic commits make review tractable.** A small diff plus a commit
   message stating intent turns review into a crisp question — "does this
   diff do what its message claims, and nothing else?" — instead of the
   mushy "is this branch good?". The workflow already produces the right
   input; this spec adds the checkpoints.

A findings ledger makes the whole loop measurable: which mistake
categories recur, and which findings still escape to human reviewers.
Reviewer prompts get tuned from data, not guesses.

## Behavior

### The `reviewer` agent type

A third built-in `task` agent type alongside `explore` and `worker`
([spec 19](19-sub-agents.md)).

| Property | Value |
|----------|-------|
| Max iterations | `sub_agents.max_iterations` (default: 30) |
| Tools | `file_read`, `glob_search`, `grep`, `web_fetch`, `web_search` |
| LCM tools | **none** |
| Model | `provider.model_overrides.reviewer`, falls back to `provider.model` |
| Cannot | Write files, execute commands, spawn sub-agents, use git/GitHub tools |

Two deliberate exclusions:

- **No LCM tools.** `lcm_grep`/`lcm_expand` retrieve the parent's
  compacted history. The reviewer's value is independence from the
  parent's narrative; giving it the parent's reasoning would rebuild the
  anchoring this spec exists to remove.
- **No exec.** The parent packs the diff into the prompt (it already has
  exec for `git diff`/`git show`). The reviewer reads surrounding files
  for context via `file_read`/`grep`. This keeps the reviewer read-only by
  construction, not by convention.

`web_fetch`/`web_search` stay: verifying an API against real
documentation directly targets the hallucinated-API slop category, and
egress is already allowlisted (spec 18).

**Durable memory is in; the parent's narrative is out.** The independence
this type exists for is insulation from the parent's reasoning about
*this change*, not amnesia about the codebase — a reviewer that does not
know the repo's conventions or the bot's recurring mistakes can only
catch generic slop. The role prompt instructs the reviewer to read
`memory/MEMORY.md`, the worked repo's topic file, and the repo's own
`AGENTS.md`/`CLAUDE.md` before judging. Memory files are ordinary
workspace files reachable via `file_read`, so spec 21's sub-agent
injection exclusion stands untouched. The contamination edge — a
remembered decision about the current task pre-anchoring a plan review —
is already excluded by memory discipline: in-progress task state is
session state, never memory (AGENTS.md, Memory section).

The reviewer's system prompt sets an adversarial stance (find what is
wrong, do not affirm), names the seed categories (see Findings), demands
findings anchored to file/line, and mandates the findings block that ends
every response. It explicitly instructs: judge the diff against the
stated intent; flag anything beyond the stated intent as scope creep.

A distinct type rather than a packed `explore` prompt because: the
adversarial role belongs at system-prompt level where it cannot be
diluted by prompt packing, and the model override gives blind-spot
diversity — a different model reviewing is worth more than the same
weights re-reading.

Naming: the `reviewer` agent type is unrelated to GitHub review sessions
(`review:{nwo}`, spec 20). Those review *other people's* PRs on GitHub;
this reviews the bot's own work locally, pre-push.

### Review gates

Three gates in the developer workflow (AGENTS.md), all dispatched by the
root agent as ordinary `task` calls with `agent_type: "reviewer"`. All
run **before** the action they guard, so fixes land in the artifact that
ships.

**1. Plan review** — after composing the implementation plan, before the
plan is published or acted on. The workflow gains an explicit plan step
for non-trivial work: state the approach and the commit-by-commit
decomposition. When work arrives via a Linear ticket, the plan is posted
to the ticket for human sign-off — the review runs **before that post**,
so the human only ever sees the polished plan, same as commit review
running before `git_commit`. For direct requests with no ticket, it runs
before implementation starts. The parent packs: the task statement
(ticket/issue/request), the plan, and relevant repo conventions. The
reviewer challenges: does the approach solve the stated task; is the
decomposition right; does it reinvent something the repo already has;
does the design make invalid states representable; is there a simpler
alternative. This is the cheapest gate and catches the most expensive
class of mistake — a wrong approach found at commit 5 costs a branch,
found at the plan it costs one call.

**2. Commit review** — after staging, before every `git_commit`. The
parent packs: the `git diff --cached` output, the proposed commit
message, and the paths touched. The reviewer answers: does the diff do
what the message claims and nothing else; correctness bugs; the slop
categories. Findings are fixed in the staged diff before committing —
history never contains the mistake and no amend is needed.

**3. Series review** — before `git_push` of a branch that will become a
PR. The parent packs: the commit list (subjects) and the full branch diff
against the base. The reviewer checks what per-commit review cannot see:
does the sum solve the task; dead ends left behind (commit 4 quietly
reverting half of commit 2); naming/convention drift across the series;
commit boundaries that stopped making sense. If the branch diff exceeds
the packable size, the parent packs the commit list plus per-commit stats
and the reviewer requests specific files via `file_read`.

The parent handles findings like human review feedback: only `must-fix`
findings oblige action; `should-fix` is the parent's judgment; `nit` is
recorded and freely ignored. The parent may dispute any finding with a
reason. The gates are **prompted, not enforced**: `git_commit` and
`git_push` do not verify a review happened. A mechanical block on an
LLM-mediated step invites the halt-loop failure mode the deny-list
already demonstrated; the ledger measures skipped gates instead, and
enforcement gets reconsidered only if the data shows prompting is not
enough.

### Convergence

Review loops are the dual failure mode: an agent reviewer will always
find *something*, and a parent that re-reviews its fixes never ships.
Two rules bound every gate:

- **Single pass per artifact.** Each gate fires exactly once; the parent
  fixes must-fix findings and proceeds without re-dispatching a review of
  the fixed version. Justified architecturally, not optimistically: every
  gate sits in front of a human gate (plan → ticket sign-off, commits and
  series → PR review). Self-review exists to cheaply strip the obvious
  before a human looks, not to prove correctness — pass two reviews the
  reviewer's own suggestions, which is where the loop lives. One
  exception: a plan-review verdict of wrong-approach yields a genuinely
  new plan, which gets one review. Capped at one redesign round; after
  that, proceed and let the human sign-off arbitrate.
- **Clean is a first-class outcome.** The reviewer's role prompt
  explicitly licenses the empty findings block: a review that finds
  nothing is a valid, expected result, and findings must never be
  manufactured to justify the invocation. Severity discipline is part of
  the same instruction — must-fix is reserved for defects, not taste.

The ledger polices both directions: findings-per-commit rate, nit share,
and dispute rate expose an over-flagging reviewer just as the escapes
stream exposes an under-catching one. Prompt tuning works from that
data.

### Findings contract

The reviewer ends every response with a fenced block:

````
```findings
{"category": "duplicate-helper", "severity": "must-fix", "file": "src/x.rs", "line": 42, "note": "normalize_path already exists in util.rs"}
```
````

One JSON object per line; an empty block means clean. `severity` is one
of `must-fix`, `should-fix`, `nit`. `category` is free-text, seeded by
the reviewer prompt with the initial taxonomy: `duplicate-helper`,
`hallucinated-api`, `unneeded-guard`, `assertion-free-test`,
`swallowed-error`, `comment-noise`, `scope-creep`, `stringly-typed`,
`wrong-approach`, `bad-decomposition`. Free-text so real categories can
emerge from data; consolidation is a later, informed decision.

The `task` tool, for the `reviewer` type only, parses the block after the
sub-agent returns and records one ledger row per line — mechanically, no
model cooperation required. The full response text is returned to the
parent unchanged either way. A malformed block or line logs a warning and
skips the row; it never fails the review.

### The ledger

SQLite at `state/review.db`, following the `state/usage.db` pattern
(spec: per-turn cost tracking). One row per finding:

| Column | Meaning |
|--------|---------|
| `ts` | Timestamp |
| `repo` | `owner/repo` |
| `gate` | `plan` \| `commit` \| `series` \| `external` |
| `git_ref` | SHA for commit/series, branch for plan, PR number for external |
| `source` | `self` \| `human` \| `bot` |
| `category` | Free-text category |
| `severity` | `must-fix` \| `should-fix` \| `nit` (self only) |
| `file`, `line` | Location, nullable |
| `note` | The finding text |

Two write paths:

- **Self findings**: mechanical, from the findings-block parse above.
- **External findings**: a `review_log` tool. When the root processes PR
  review feedback (AGENTS.md step 11), it logs each inline comment with a
  category before acting on it. Human corrections to a plan posted on a
  Linear ticket are logged the same way (`gate = plan`, `source = human`).
  Model-driven and prompted — acceptable, because external findings arrive
  in an LLM turn anyway and there is no mechanical categorizer.

External rows are the escapes stream: findings that survived all three
self-review gates, and the per-category self-vs-external delta is the
quality metric for the whole loop. Human and review-bot escapes both
count. A bot escape is not a lesser event — a bot comment on a PR
triggers a fix commit, a re-review round, and another poll cycle,
exactly the post-publication churn the gates exist to minimize — so
recurring bot-flagged categories belong on the checklist as much as
human-flagged ones. Both sources have false-positive rates; the
`source` column keeps the streams separable so a category one source
keeps raising and the parent keeps disputing is discounted rather than
learned. Recurring escape categories
feed back to the reviewer through a `review-checklist` memory topic the
bot maintains from ledger data — the role prompt is compiled in
(`include_str!`) and static, so the data-derived checklist lives in
memory, which the reviewer reads at every gate.

Reader ships in the same commit as the writer (bin-only crate dead-code
constraint): a `/findings` command and `just findings` recipe reporting
counts by category, source, gate, and repo over a time window, mirroring
`/usage`.

### Workflow integration

AGENTS.md Developer Workflow changes (prose, in the implementation
commits):

- New step after Orient/Context: **Plan** — for non-trivial work, write
  the approach and commit-by-commit decomposition, then dispatch plan
  review and incorporate findings *before* posting the plan to the ticket
  for human sign-off (Linear-sourced work) or implementing (direct
  requests).
- Step 7 (same-context self-review) is **replaced** by the commit-review
  gate. Two self-reviews per commit is cost without benefit, and the
  same-context one is the weaker of the two.
- Push step gains the series-review gate.
- Review-feedback step gains: log each inline comment via `review_log`
  before acting on it.

### Configuration

```toml
[review]
enabled = true
```

| Config key | Default | Description |
|------------|---------|-------------|
| `review.enabled` | `true` | Master switch: registers `review_log`, opens the ledger, and includes the review gates in the workflow prompt |
| `provider.model_overrides.reviewer` | unset | Reviewer model; falls back to `provider.model` |

When disabled, the workflow prompt omits the gates entirely — a prompt
that references an unavailable mechanism is worse than none.

## Boundaries

### Owns

- The `reviewer` agent type: system prompt, tool allowlist
- The findings-block contract and its parser
- The ledger: schema, writer, `/findings` + `just findings` reader
- The `review_log` tool
- The review-gate additions to the workflow prompt

### Does Not Own

- Sub-agent machinery — `task` tool, `EphemeralSession`, allowlist
  filtering (spec 19)
- Model overrides (spec 02)
- GitHub feedback ingestion — comments arrive via spec 20's existing flow;
  this spec only adds logging them
- `git_commit` / `git_push` (spec 03) — deliberately untouched; gates are
  advisory

### Interactions

- **Sub-agents (spec 19)**: `reviewer` is a third prebuilt type in the
  same framework; the `task` tool gains the type and, for it, the
  post-return findings parse. Spec 19's "no recursive spawning" and
  allowlist-validation tests extend to the new type.
- **Provider (spec 02)**: one new entry in `model_overrides`, same
  fallback semantics as the existing five roles.
- **GitHub channel (spec 20)**: untouched mechanically. The `review:{nwo}`
  session continues reviewing others' PRs; own-PR human feedback continues
  arriving on the work session, now logged via `review_log`.
- **Cost tracking**: reviewer sub-agent turns are billed inside the
  parent's `run_turn` row like any other sub-agent — no ledger coupling.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Reviewer sub-agent errors (overflow, max_iterations, provider) | Tool error text to parent; parent proceeds on its own judgment. A failed review is a skipped review, not a blocked turn. |
| Findings block missing or malformed | Warning log, no ledger rows; full response text still delivered to parent. |
| Ledger write fails | Warning log, review continues. The ledger is telemetry; it never blocks work. |
| Reviewer hallucinates a finding | Parent disputes with a reason, as with human feedback. Cost is bounded by the advisory design — no gate blocks on a false positive. |
| Branch diff too large to pack (series gate) | Parent degrades to commit list + stats; reviewer pulls files itself. |
| `review_log` called with bad arguments | `ToolError::InvalidArguments`, parent retries or skips. |

## Constraints

- The reviewer is read-only and context-isolated by construction: no
  exec, no writes, no LCM tools, no git/GitHub tools. Independence from
  the parent's narrative is the design point, not an optimization.
- Gates are advisory. No tool blocks on review status.
- Single pass per artifact: no re-review of fixes; at most one plan
  redesign round. The human gates downstream are the backstop.
- Self-finding ledger writes are mechanical (harness parse); only
  external findings go through a model-invoked tool.
- One reviewer model, static config; no per-call model selection (same
  rationale as spec 19: the model never controls spend).
- The findings block is a lenient text convention, not a structured-output
  protocol; no provider-level structured output machinery.

## Open Questions

- **Disposition tracking**: should the parent's fix/dispute decision be
  recorded per finding (a `review_log` update call)? V1 records findings
  only, but the dispute-rate discounting above needs dispositions to
  work — until they exist, noisy external categories are filtered by
  hand, not data. Likely the first post-v1 addition.
- **Category consolidation**: when does free-text collapse into an enum,
  and does the reviewer prompt's seed list get regenerated from ledger
  data mechanically or by hand?
- **Checklist maintenance cadence**: v1 relies on prompted memory
  discipline at feedback-processing time to keep the `review-checklist`
  topic current. If that proves lax, the tightening is a heartbeat duty
  that queries the ledger and reconciles the checklist — same shape as
  distillation (spec 21).
- **Ledger file**: own `state/review.db` vs a table in `usage.db`.
  Separate file assumed here; one-file ops simplicity may argue otherwise.
- **Per-gate model strength**: plan review arguably deserves the
  strongest model while commit review could run cheaper. Single override
  in v1; split only if the ledger shows plan-review misses.
- **Series-gate size threshold**: at what packed-diff size does the
  parent degrade to list+stats? Needs a real number from usage, not a
  guess.
