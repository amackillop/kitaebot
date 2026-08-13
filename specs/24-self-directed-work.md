# Spec 24: Self-Directed Work

## Motivation

Every unit of work the bot performs today is human-initiated: a
Telegram message, a Linear ticket a human wrote, a PR that requests
review. The bot has execution (channels, tools), verification (spec
23 gates), and durable knowledge (spec 21 memory), but no work
discovery — nothing ever asks "what should exist that doesn't?"
Standing problems the bot could find mechanically (its own stale PRs,
failing CI on trusted repos, review-ledger escapes not yet folded
into the checklist) sit unnoticed until a human trips over them.

The blocking defect is the scheduler. Spec 07's heartbeat is a fixed
`tokio::interval` anchored to daemon start: it cannot express "daily
at 03:00", and any restart resets the phase — a daily cadence on an
interval timer drifts with every deploy.

The heartbeat's task list is a second defect, not a feature to
migrate. `HEARTBEAT.md` checkboxes are an artifact of the project's
openclaw-era origins, conflating two things a scheduler must keep
apart: recurring operator tasks (which want per-task schedules,
gates, and cursors — none of which a checkbox line carries) and
deferred one-shots (which want due times, ownership, and completion
state — spec 21's never-built "structured commitments"). This spec
replaces the file with both proper forms and retires it.

Three phases: the duty scheduler; discovery duties that propose work
for human approval; commitments that let deferred work survive
context loss and restarts.

## Behavior

### Phase 1: the duty scheduler

A **duty** is a named unit of scheduled work: a schedule, a cheap
mechanical **gate** deciding whether there is anything to do, and an
**action** (an LLM turn or a mechanical function) that runs only when
the gate opens. Gates are code, not model judgment: a `gh` query, a
token count, a file stat, a cursor comparison.

Schedules, per duty, config-declared:

- `every = "<duration>"` — interval semantics, e.g. `"1h"`.
- `daily = "HH:MM"` — wall-clock UTC.

Scheduling rules:

- **Persisted phase.** Each duty's `last_run` timestamp is persisted
  as the `duties` document in the state database (spec 05). A duty is
  due when `now >= next_due(last_run, schedule)`. Restarts change
  nothing: cadence derives from persisted state, not process start.
- **Anacron catch-up.** A duty overdue at startup (the daemon was
  down across its due time) fires once, then `last_run = now`.
  Missing three periods yields one catch-up run, never three.
- **Serialization.** Duty turns enter the same actor queue as every
  other turn; two duties due simultaneously run in sequence, in
  declaration order. The scheduler never preempts an in-flight turn.
- **Startup grace.** No duty fires until channels are up; duties then
  fire in declaration order.

Distillation migrates onto the scheduler (`every = "1h"` default; its
token gate stays — a closed-gate tick costs two file stats) and the
global heartbeat interval disappears.

**HEARTBEAT.md is retired.** The task-review turn, the checkbox
parser, and the deployed prompt-file symlink go with it. Recurring
operator tasks become prompt duties (below); deferred one-shots
become commitments (phase 3). Duty outcomes land in `JOURNAL.md`
(spec 05) under the `[duty]` topic: dispatch duties via the actor's
unattended-outcome journaling, mechanical duties from the scheduler
itself. Routine no-ops (a closed gate) stay out — they are tracing,
not work. The `/heartbeat` command becomes `/duties`: run
every duty whose gate is open, ignoring schedules — the operator's
"run it now". `/duty <name>` runs one. Both route from the actor to
the scheduler over a trigger channel and execute on the scheduler's
own path: same gates, same journaling, and `last_run` advances, so a
manual run defers the next scheduled tick rather than duplicating
it. The actor validates names against the duty list and replies
"queued" immediately — a triggered run can take as long as any turn,
and the chat socket must not block on it. Duty-sourced commands are
not forwarded: the scheduler dispatches `/duty distill` through the
actor, and forwarding it back would loop.

**Operator-defined prompt duties.** Recurring watch-tasks the
operator authors in config: a name, a schedule, a prompt, and an
optional mechanical gate. The canonical example is a security watch:

```toml
[[duties.prompt]]
name = "open-money-security-watch"
daily = "06:00"
repo = "CumuloGlobal/open-money"
gate = "new-commits"
prompt = "Review the commits since the cursor for security issues..."
```

The `new-commits` gate keeps a per-duty cursor (last-reviewed SHA) in
the same `state/` file as `last_run`: a cheap fetch-and-compare
decides whether a turn runs, and each run sees only the delta — an
idle repo costs two git commands. Without a gate the prompt runs
unconditionally on schedule. The repo must be listed in `git.repositories`;
the turn runs on the repo's work session; the scheduler prepares a fresh
checkout (clone, fetch, detach at origin/HEAD, clean, devshell) before
the turn starts, same `execution_checkout::prepare` flow the issue
channel uses, so the agent finds a working base and injected conventions.
A clone failure falls back to a manual-clone note rather than leaving the
turn to improvise. Anything the prompt wants to raise goes through the
proposal contract below, same caps. This stays inside the trust model
because the operator authors the prompt and the schedule in config — it
is operator-defined work on a timer, not model-created work.

**Built-in vs operator-defined — the line.** Code owns contracts,
config owns intent. A duty is built-in when its gate or mechanical
scaffolding (dedup keys, state queries) couples to internal schemas —
the review ledger, PR ownership, the memory layout — because that
half must version in lockstep with the code it reads, and because a
gate expressed as an LLM turn would cost tokens to discover there is
nothing to do. A duty is operator-defined when it is pure domain
intent: run this judgment against this external delta on this
schedule. Equivalent test: if its breakage should be a repo bug
caught by the test suite, it is built-in; if it should be a config
error the operator owns, it is a prompt duty. Built-ins expose their
judgment-free knobs (schedule, enabled, cap values) in config; their
logic does not move there.

### Self-maintenance duties

Recurring mechanical work on the bot's own machine rather than on a
repository. No LLM turn: the outcome is a log line, not a reply.

- **Warm** — build each configured repo's checks, cloning the checkout
  first if absent, so the commit gate never meets a cold store. The
  duty is gated per-repo on new commits: each tick probes the remote
  HEAD via `ls-remote` and warms only repos whose HEAD moved past a
  per-repo cursor (`warm/<nwo>` in the state DB), or that have no
  cursor (enrollment) or no checkout. The cursor advances only on a
  successful warm, so a failed warm retries next tick. Repos warm
  sequentially: two cold builds would contend for the same cores.
  Contract and consumer: [spec 03](03-tools.md#build-warm).
- **Workspace hygiene** (not built) — remove finished checkouts and
  the `.direnv` gcroots pinning their devShell closures. Review
  checkouts became worktrees of the working clone — no object store,
  no devshell — so what remains to clean is `projects/` checkouts,
  and "finished" is undefinable there until sessions bind to
  checkouts. Deferred until that binding exists.

Scheduled rather than hooked to boot or clone: the scheduler's anacron
behaviour covers boot without a separate path, and covers what neither
hook can — a garbage collection that ran since. The two duties are
coupled: nothing roots what a warm builds
([spec 03](03-tools.md#open-questions)), so hygiene keeps the store
small enough that collection stays rare, and a scheduled warm turns an
eviction into one background rebuild instead of a blocked commit.

Mechanical duties are a `Duty` action kind run inside the scheduler
loop itself — no actor, no turn. The scheduler serializes duties, so a
cold warm delays whatever is due behind it; the warm duty is declared
last so dispatch duties due at the same tick go first.

### Phase 2: discovery duties

Duties whose action *generates* work rather than performing it.
Scope is the `git.repositories` trust list only, for all of them.
Gates query GitHub through the REST client (`clients/github.rs`),
never a CLI.

**The proposal contract.** Discovery output that implies new work
becomes a tracker ticket, body carrying the evidence and the bot's
analysis. Routing is an explicit per-repo config mapping:
`git.repositories."owner/repo".proposals = "github"` — a string enum
so a Linear write path can join later; a repo without the field gets
discovery observation but never filings. For GitHub the write path is
`github_issue_create`: the issue files unassigned, a human triages by
assigning it to the bot, and the issues channel (spec 10) picks it up
like any other ticket. The bot never executes work it proposed
without that transition; proposal and authorization are separated by
an existing human gate.

**Self-analysis** (built). Mines the bot's own problem record for
evidence of defects in kitaebot itself — the issue #7 smoke test
showed four real defects visible in the operational record before any
human looked. Sources are the error tee (`state/errors/`, spec 05)
and the journal filtered to `[notify]` entries: the alert mirror
already is the problems journal, so successful-run prose never enters
the delta. Panics reach the tee through a hook installed with the
subscriber — without it a crash was the one failure the duty could
not see (stderr and journald only, unreadable by design), and with
`Restart=on-failure` a crash loop stacks one entry per attempt for
the next successful boot's run to read. The gate is `(file, byte offset)` cursors plus a low token
threshold (`min_delta_tokens`, default 200) — the delta is
incident-shaped, not volume-shaped like distillation's. First contact
primes at end-of-sources; a failed turn re-reads the same delta next
period; a below-threshold delta accumulates. The turn's contract:
investigate, ground the symptom in the bot's own checkout, then file
at most one issue or reply that nothing is actionable. The boundary
with distillation: distill turns experience into knowledge (memory);
self-analysis turns anomalies into tickets. Quality defects hiding in
*successful* outputs are out of scope. Config:
`[duties.self_analysis]` with a schedule, the target `repo` (must be
trusted and proposal-enabled), and `min_delta_tokens`. `/duties` reaches it like every duty, via the
scheduler's trigger channel.

**What belongs in the error tee.** The tee is not a severity filter
that happens to be readable. It is this duty's evidence set, and
`LevelFilter::WARN` is only the mechanism that populates it. Two
consequences, both of which have already been got wrong once:

*Below WARN is not quieter, it is gone.* The duty's other source is
`state/JOURNAL.md`, the bot's own topic-tagged record — not journald,
which the daemon cannot read back at all. That unreadability is the
entire reason the tee exists. So choosing `debug!` over `error!` at a
call site is not a presentation decision, it is a decision to withhold
that event from self-analysis permanently. Log level here is data
retention.

*Select on "could this indicate a fixable defect", not on "is the
daemon at fault".* The two come apart precisely where the interesting
defects live. A model repeatedly calling a tool that does not exist is
not a daemon fault, and it is exactly the evidence that a prompt
advertises a stale tool name. A reviewer failing to read a file that
was never created is not a daemon fault either, and it is how the
missing file got found. Faultless-but-recurring is the signal, so
"the daemon behaved correctly" is not grounds for exclusion. What *is*
grounds: outcomes that can never indicate a defect however often they
repeat, such as a search command exiting non-zero because it matched
nothing.

*Entry size is a correctness concern, not tidiness.* The tee has no
per-entry cap and the duty truncates the whole errors section at
`SECTION_MAX_BYTES`. One oversized entry — a failed command logged
with its entire stdout and stderr — therefore evicts every other
incident in that window. Log a bounded, structured summary naming the
operation, its inputs, and the outcome; the full payload belongs in
the tool result the model reads, which is a separate path.

**Planned next, same contract:**

- **CI triage**: gate is a REST query for new failing default-branch
  workflow runs (run-id cursor per repo). Action analyzes the failure
  and files a proposal with an evidence key
  (`ci:{nwo}:{workflow}`) embedded, giving mechanical dedup the
  self-analysis duty cannot have.
- **Shepherd** (from spec 21's deferred list): the bot's own open
  PRs — unanswered review threads, red CI, branches with no PR after
  N days. Acts on already-authorized surface (its own PRs), so mostly
  no proposals; the session-binding question (per-repo vs combined
  turns) is decided at its implementation.
- **Checklist reconciliation** (from spec 23's deferred list): ledger
  query for unreconciled escapes; memory-only turn, no proposal
  needed.

**Spend and volume caps, mechanical:**

- At most 3 open bot-proposed issues per repo, counted by author
  before dispatch (the bot is a normal account, so its filings are
  author-searchable — no marker label). At the cap the duty skips and
  journals the skip; triage frees the cap.
- Duplicate suppression: the open bot-authored issues are injected
  into the dispatch prompt (in-context dedup); duties with natural
  evidence keys (CI triage) additionally embed them for mechanical
  suppression.
- One filing per run, stated in the turn contract: a daily duty files
  at most ~7 a week against a cap of 3, so triage binds quickly.
- No per-day token budget yet, deliberately: the schedule is the cost
  ceiling (one turn per period, bounded by `max_iterations`), and the
  usage ledger already records what a future cap would need. Add the
  budget when duty count or cadence makes the schedule an insufficient
  bound — with data, not in advance.

### Phase 3: commitments

Subsumes spec 21 phase 3 ("structured commitments"), which was filed
under memory but is work scheduling; this spec owns it.

A **commitment** is a deferred one-shot: `{id, text, due, session,
created_by, status}` persisted in `state/`. Created two ways, both
conversational and both visible in the reply that creates them:

- The operator asks: "tomorrow, do X" / "remind me Friday".
- The bot promises: "I'll re-check CI after the deploy" becomes a
  record, not a hope that the context survives.

A `commitment-due` duty (interval, e.g. `every = "10m"`) is the
mechanical due-gate: it queries for `due <= now AND status = open`
and dispatches one turn per due commitment on its owning session,
carrying the commitment text. The turn completes, reschedules (with
a stated reason), or cancels the commitment; every transition is
recorded and appears in `/commitments`.

Commitments are model-created, which the constraints below otherwise
forbid — the containment is different, not absent: creation is
visible in-conversation at creation time (never silent), `/commitments`
lists all open ones, the operator can cancel any, open commitments
are capped (default 10), and execution is an ordinary turn through
every existing gate. A commitment authorizes a *reminder to act*,
not the act: outward effects still pass review gates and, for new
work, the proposal contract.

## Boundaries

Owns: the duty scheduler, duty definitions, prompt duties,
commitments, schedule and cursor state in `state/`, the proposal
contract. Replaces spec 07 (heartbeat) entirely; subsumes spec 21
phase 3. Journal logging is unchanged in format.

Does not own: turn execution (agent actor), Linear transport (MCP /
linear channel), review machinery (spec 23), memory writes (spec 21).
Duty and commitment actions are ordinary turns; every existing gate
(review, egress, deny-list) applies unchanged.

Assumes: system clock is sane (NTP); `git.repositories` is the
complete scope statement for discovery and prompt duties, and its
`proposals` fields are the complete routing statement for filings.

## Failure Modes

- **Duty turn fails**: logged, `last_run` still advances (retry next
  period, not tight-loop). Unattended failures alert via the spec 17
  notifier, same as any unattended turn.
- **Commitment turn fails**: the commitment stays open; the due-gate
  re-fires it next tick. A commitment that fails N consecutive times
  (default 3) is marked stuck and alerted, not retried forever.
- **Clock jumps backward**: duties become not-due; no double-fire,
  because due-ness derives from `last_run`.
- **State file lost**: duties all become due and anacron catch-up
  runs each once; commitments are lost — acceptable, they are
  reminders, and the loss is visible in `/commitments`. Cursor loss
  re-reviews one delta.
- **Tracker unreachable during proposal**: the turn reports the
  failure; the cursor did not advance, so the duty re-reads the same
  delta next period. No queue, no retry loop inside the turn.
- **Symptom probe fails** (unreadable journal or error files): logged,
  duty retries next period; the cursor stays put.
- **Warm ls-remote fails**: the repo is skipped with cursor untouched;
  it retries next tick.
- **Warm command fails**: cursor does not advance, so the repo retries
  next tick.
- **Proposal flood attempt** (a broken gate matching everything): the
  per-repo cap bounds damage to 3 open issues, and one filing per run
  bounds the rate; the injected open list bounds repeat noise.

## Constraints

- Model judgment never schedules recurring work, authorizes work, or
  spends money: schedules are config, gates are code, caps are code,
  and outward work needs the human tracker transition (assignment, for
  GitHub). Commitments are the sole, deliberately-contained exception
  (phase 3): one-shot, visible at creation, listed, capped,
  cancellable.
- Discovery and prompt duties read and propose against
  trusted repos (`git.repositories`) only.
- Duty turns run through the normal actor; no parallel execution, no
  second agent loop.
- Schedule, cursor, and commitment state live in the state database
  (spec 05): opaque documents for load-and-save state, real tables
  (versioned migrations) for anything queried.
- SQL never leaves the module that owns the schema. Duty gates that
  read a ledger consume a method on it (`unreconciled_escapes()`,
  `spent_since()`), never inline queries — a gate wanting raw table
  access is a gate asking for an API that does not exist yet.

## Open Questions

- **Shepherd session binding**: shepherd items belong to per-repo
  work sessions; does one shepherd run dispatch one turn per repo
  with items, or one combined turn? Per-repo matches session-context
  locality; combined is cheaper. Decide at implementation.
- **The Linear write path**: `proposals = "linear"` needs issue
  creation, a triage state (hardcode the state name in config — it
  fails louder than MCP discovery), and a human-transition signal the
  Linear channel can detect. Whether that grows a small trait
  (propose, count open) or stays a second match arm is decided when a
  Linear-tracked repo actually wants proposals; GitHub shipped as
  plain functions on purpose.
- **Gate vocabulary for prompt duties**: `new-commits` is the only
  gate v1 needs; new-issues, new-releases, or an RSS cursor follow
  the same cursor shape when a real watch-task wants them. A
  min-commit-count threshold was considered and rejected (2026-07-25):
  the schedule is the cost ceiling and the gate only subtracts, so a
  fast repo degrades to the ungated per-period cost by design;
  batching would delay review of exactly the commit that matters, and
  counting needs a heavier probe than ls-remote. If churn noise ever
  shows up in usage data, the answer is a path/author filter on the
  delta, not a count.
- **Commitment creation surface**: a dedicated tool the model calls,
  or prompted convention plus distillation catching missed promises?
  A tool is mechanical and auditable; lean tool, decide at phase 3
  design.
