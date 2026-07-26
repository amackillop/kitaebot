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

The reviewer's system prompt sets the stance and the bar. Stance: judge
the artifact against its stated intent; flag anything beyond that intent
as scope creep; matter-of-fact tone, findings anchored to file/line, at
most a paragraph per finding, no code chunks over three lines. The bar
is a finding-eligibility test, adapted per gate — a finding qualifies
only if it:

- was introduced by the change under review; pre-existing problems are
  not findings (commit and series gates; at the plan gate the plan
  itself is the artifact and the test does not apply),
- is discrete and actionable, not a general complaint about the
  codebase,
- does not demand rigor absent from the rest of the codebase,
- does not rest on unstated assumptions about the author's intent, and
- passes the author-would-fix test: the author, made aware, would want
  to fix it.

Speculation that a change "might disrupt" something elsewhere does not
qualify without evidence. The prompt also names the seed categories
(see Findings) and mandates the findings block that ends every
response.

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
reason. The verdict is recorded signal, not mechanism — an `incorrect`
verdict blocks nothing by itself, but a push whose series review said
`incorrect` is a fact the ledger keeps. The gates are **prompted, not enforced**: `git_commit` and
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

The reviewer ends every response with a fenced block holding one JSON
object:

````
```findings
{
  "verdict": "incorrect",
  "confidence": 0.9,
  "explanation": "<1-3 sentences justifying the verdict>",
  "findings": [
    {"category": "duplicate-helper", "severity": "must-fix",
     "confidence": 0.8, "file": "src/x.rs", "line": 42,
     "note": "normalize_path already exists in util.rs"}
  ]
}
```
````

`verdict` is `correct` or `incorrect` — whether the artifact is free of
blocking issues, ignoring nits. It gives the parent a one-bit summary it
can act on without weighing individual findings, and gives the ledger a
per-invocation outcome to correlate against later escapes. An empty `findings` array with a
`correct` verdict is the clean outcome. `confidence` (0.0–1.0, on the
verdict and on each finding) lets the parent weigh a hesitant must-fix
differently from a certain one and gives the ledger a calibration
signal. `severity` is one of `must-fix`, `should-fix`, `nit`.
`category` is free-text, seeded by the reviewer prompt with the initial
taxonomy: `duplicate-helper`, `hallucinated-api`, `unneeded-guard`,
`assertion-free-test`, `swallowed-error`, `comment-noise`,
`scope-creep`, `stringly-typed`, `wrong-approach`, `bad-decomposition`.
Free-text so real categories can emerge from data; consolidation is a
later, informed decision.

The `task` tool, for the `reviewer` type only, parses the block after the
sub-agent returns and records one ledger row per finding plus one review
row for the invocation's verdict — mechanically, no model cooperation
required. The full response text is returned to the parent unchanged
either way. A malformed block or entry logs a warning and skips the
affected rows; it never fails the review.

### The ledger

SQLite at `state/review.db`, following the `state/usage.db` pattern
(spec: per-turn cost tracking). One row per finding:

| Column | Meaning |
|--------|---------|
| `ts` | Timestamp |
| `repo` | `owner/repo` |
| `gate` | `plan` \| `commit` \| `series` \| `external` \| `pr` (bot reviews of others' PRs, spec 20) |
| `git_ref` | SHA for commit/series/pr, branch for plan, PR number for external |
| `source` | `self` \| `human` \| `bot` |
| `category` | Free-text category |
| `severity` | `must-fix` \| `should-fix` \| `nit` (self only) |
| `confidence` | 0.0–1.0, nullable (self only) |
| `file`, `line` | Location, nullable |
| `note` | The finding text |
| `disposition` | `fixed` \| `disputed` \| `no-action`, nullable (see Disposition tracking) |
| `disposition_note` | Reason, required for disputes, nullable |
| `disposed_at` | Timestamp of the disposition, nullable |

A second table, `reviews`, records one row per gate invocation: `ts`,
`repo`, `gate`, `git_ref`, `verdict`, `confidence`. It answers what
finding rows cannot: whether a gate ran at all — a pushed series with no
series-review row is a skipped gate, which is how "the ledger measures
skipped gates" is actually mechanized — and how verdicts correlate with
eventual escapes.

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

### Disposition tracking

Findings record what a reviewer said; dispositions record what the
parent did about it. Without them, dispute-rate discounting — the
mechanism that separates a noisy source from a good one — works by
hand-filtering, not data. Dispositions are the parent's per-finding
decision, written after acting on it.

**Vocabulary**: `fixed` | `disputed` | `no-action`. Enum at the tool
boundary, free text in storage — same rationale as `category` and
`severity`: a novel value must not invalidate the row carrying it.
Every value is a factual outcome, not an attitude: `no-action` covers
both the freely-ignored nit and the answered question — uncontested,
no code change warranted. Deliberately not `ignored` (an attitude
word; a parent that just answered a question won't self-describe as
ignoring it and misfiles the row as `disputed`, inflating the human
dispute rate) and not a fourth `answered` value (taxonomy no planned
query distinguishes). A dispute requires a `disposition_note`; the
note is what makes a dispute auditable rather than a shrug.

**Who acts, and when**: at the self gates the parent both receives the
finding and acts on it, so it dispositions immediately and `pending`
measures its own discipline. At the `pr` gate (spec 20) the finding is
published to someone else's PR, so the actor is its author and the
disposition waits for their reply on a later follow-up turn. `pending`
there means awaiting the author. Queries that read `pending` as laxity
must therefore exclude the `pr` gate or separate it out; the dispute
rate needs no such care, and a human disputing a published finding is
the strongest calibration signal the ledger receives.

**Identity**: a disposition needs a finding to point at, and row ids
never left the database in v1. Both write paths now surface them:
`record_review` returns the inserted ids and the task tool appends a
mechanical trailer to the reviewer text it hands the parent
(`[ledger: finding ids 12, 13]`; nothing appended for a clean review
or a disabled ledger), and `review_log` replies `Recorded finding #N.`
instead of `Recorded.`. Ids reach the parent as ordinary tool output —
no side channel, and the parent quotes them back verbatim.

**Write path**: a root-only `review_disposition` tool
(`finding_id`, `disposition`, `note`) that can only annotate existing
rows — it creates nothing, so it needs no equivalent of `review_log`'s
`source = 'self'` forgery guard. An unknown id is a tool error, not a
silent no-op: a hallucinated id must be visible to the model that
produced it. Model-reported by necessity — only the parent knows
whether it fixed or disputed — which is the same trust level as
`review_log`.

**Reading**: `/findings` gains a dispositions-by-source section:
total, fixed, disputed, no-action, pending (`disposition IS NULL`) per
source. Dispute rate per source is the query this whole mechanism
exists to answer; pending rate is the free byproduct that measures
disposition discipline itself.

**Enforcement**: prompted, not enforced, same doctrine as the gates.
The review-gates segment instructs a `review_disposition` call after
acting on each finding. Findings left pending forever are the failure
mode; the pending column makes it measurable, and a mechanical nag
(heartbeat duty) is built only if the data shows prompting is not
enough.

**Migration**: `state/review.db` predates these columns, so `open()`
adds them via guarded `ALTER TABLE` (checked against
`pragma_table_info`) — `CREATE TABLE IF NOT EXISTS` never alters an
existing table.

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
  guess. The `pr` gate (spec 20) dissolved its version of this question
  rather than answering it: the parent redirects the diff to a file and
  packs the path, so no diff text crosses either context and there is
  no size to threshold. The same move should work at the commit and
  series gates, with one wrinkle — those diffs are taken in a *working*
  checkout under `projects/`, so the diff file has to land outside the
  repo it describes or it dirties the tree it is about to be committed
  from. Until that is settled the self gates still pack by value, which
  makes the two halves of the pipeline inconsistent.
