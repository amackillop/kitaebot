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
"run it now".

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
the turn runs on the repo's work session; anything the prompt wants
to raise goes through the proposal contract below, same caps. This
stays inside the trust model because the operator authors the prompt
and the schedule in config — it is operator-defined work on a timer,
not model-created work.

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
  first if absent, so the commit gate never meets a cold store. Repos
  warm sequentially: two cold builds would contend for the same cores.
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

- **Shepherd** (from spec 21's deferred list): gate is a `gh` query
  over the bot's own open PRs — unanswered review threads, PRs
  whose CI is red, branches with no PR after N days. Action is an
  LLM turn on the owning work session to push each item forward
  (reply, fix, or propose closing).
- **CI triage**: gate is a `gh` query for failing default-branch
  workflows on trusted repos. Action analyzes the failure and files
  a Linear issue (see proposal contract) with the analysis attached.
- **Checklist reconciliation** (from spec 23's deferred list): gate
  is a ledger query — external findings dispositioned `fixed` (true
  escapes) not yet reflected in `state/review-checklist.md`.
  Action is a memory-only LLM turn folding them in. No proposal
  needed: memory writes have no outward effect.

**The proposal contract.** Discovery output that implies new work
becomes a tracker ticket, body carrying the evidence (query results,
links) and the bot's analysis. For GitHub-tracked repos the write path
exists: `github_issue_create` files the issue unassigned, a human
triages by assigning it to the bot, and the issues channel (spec 10)
picks it up like any other ticket. The bot never executes work it
proposed without that transition; proposal and authorization are
separated by an existing human gate. Which repos file proposals to
Linear instead of GitHub — and the Linear write path itself — is
undesigned; it needs an explicit per-repo mapping in config, not a
guess.

**Spend and volume caps, mechanical:**

- At most N open bot-proposed issues per repo (default 3). The gate
  counts before the action runs; a full triage column stops proposal,
  and the surplus goes to the journal, not Linear.
- Duplicate suppression: a proposal keyed to the same evidence (same
  failing workflow, same PR) as an existing open bot-proposed issue
  is not re-filed.
- Per-day duty-turn budget (token count from the usage ledger); duties
  whose budget is spent skip with a logged reason.

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

Assumes: system clock is sane (NTP); Linear workspace has a triage
state the bot's token may create issues into; `git.repositories` is the
complete scope statement for discovery and prompt duties.

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
- **Linear unreachable during proposal**: the action logs the
  proposal to the journal and the duty retries next period; no queue,
  no retry loop inside the turn.
- **Proposal flood attempt** (a broken gate matching everything): the
  per-repo cap bounds damage to N issues; the duplicate key bounds
  repeat noise.

## Constraints

- Model judgment never schedules recurring work, authorizes work, or
  spends money: schedules are config, gates are code, caps are code,
  and outward work needs the human Linear transition. Commitments are
  the sole, deliberately-contained exception (phase 3): one-shot,
  visible at creation, listed, capped, cancellable.
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
- **Triage state discovery**: hardcode the Linear state name in
  config, or resolve it via the MCP at startup? Config is simpler and
  fails louder.
- **Proposal-target abstraction**: the contract is proposal +
  human transition + pickup, and Linear is one implementation of it;
  GitHub Issues is the obvious second (repos with `gh` access but no
  Linear team). Decide at phase 2 design whether a small trait
  (propose, count open, find by dedup key) is worth defining up
  front — the sizing question is whether the human-transition
  detection generalizes, since the Linear channel owns it today.
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
