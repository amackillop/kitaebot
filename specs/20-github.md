# Spec 20: GitHub Channel

## Motivation

The GitHub channel connects the bot to pull requests in two directions:

1. **Feedback on its own PRs**: the bot opens PRs (via GitHub tools or the
   Linear flow) and humans respond with reviews and comments. The channel
   polls for that feedback and turns it into agent turns, so the bot can
   revise its work.
2. **Reviewing others' PRs**: a human requests a review from the bot's
   account and the bot reviews the PR. Review requests are explicit,
   per-PR, auditable in the PR timeline, and self-clearing (GitHub drops
   the pending request once a review is submitted) — no mention parsing
   needed.

Both directions share one poll loop, one identity, one trust list, and one
state file. The GitHub *tools* (PR creation, CI status, the `gh` escape
hatch) are part of the tool registry and stay in spec 03; this spec owns
the channel.

## Behavior

### Poll loop

`tokio::time::interval` with `MissedTickBehavior::Skip`. Each tick runs
both queries:

1. **Own PRs**: `gh search prs --author=@me --state=open`, then per PR
   fetch reviews, comments, and inline diff comments.
2. **Review requests**: `gh search prs --review-requested=@me --state=open`.
3. **Tracked reviewed PRs**: for each PR in the `reviewed`
   map, fetch state, head SHA, and new comments. Closed/merged PRs are
   pruned; a new head SHA triggers an incremental re-review; new trusted
   comments trigger a discussion turn; both in one tick fold into a
   single combined turn.

Items are filtered (see below) and dispatched through the agent handle
with `ChannelSource::GitHub { pr_number, repo, role }`, where `role` is
`author` (feedback on the bot's own PR) or `reviewer` (a review
request, a re-review, a discussion turn on a PR the bot reviewed). The
channel knows which of its three poll passes produced each item, so the
role costs nothing to carry. It selects the review protocol segment
([spec 06](06-system-prompt.md)) and nothing else.

**All GitHub turns route to the repo's work session** (`owner/repo`,
the same key as Linear issue routing). `last_poll` advances only after
a successful poll.

An earlier revision gave reviews their own `review:{nwo}` session, to
keep prior-review context from compacting in-progress work away and to
accumulate repo knowledge across reviews. Both justifications have
since been taken over:

- **Judgment isolation** belongs to the `reviewer` sub-agent
  ([spec 23](23-self-review.md)). The diff and the code reading happen
  in an ephemeral context that is discarded; the root review turn is
  four mechanical calls and holds no diff at all.
- **Knowledge accumulation** belongs to memory ([spec
  21](21-memory.md)). Distillation gathers pending spans across *all*
  sessions with per-session watermarks, so reviewer output reaches
  `memory/topics/` wherever the turn ran — the review session was never
  load-bearing for it. What the reviewer needs back is
  `memory/topics/review-checklist.md` and the findings ledger, both
  session-independent.
- **Prior-review recall** is authoritative on GitHub, not in a
  session: `gh pr view --json reviews` is what was actually published,
  where session history is what the bot believes it said after
  compaction.

What a separate session did still buy was narrative isolation, and the
actor bounds how much that is worth: it consumes envelopes serially and
awaits each turn to completion, so a review dispatch arriving during an
implementation turn queues behind it rather than interleaving. A whole
commit-by-commit implementation is one turn. Interference is therefore
confined to turn boundaries, where nothing is in flight, and feedback
on the bot's own PRs has always landed in the work session anyway.

The residual case is an implementation turn that exhausts
`agent.max_iterations` — root `BudgetPolicy` is `Fail`, so it errors
with work half-done and continuation becomes a second turn. That is a
failure mode to handle at the cap, not a reason to shard sessions.

### Review checkout

Review turns never touch the working checkout under `projects/`. Before
dispatching a review, the channel prepares a dedicated clone at
`reviews/<owner>/<repo>`: clone on first use, then
`git fetch origin <base> pull/{n}/head` and
`git checkout --force --detach <head-sha>`. Force-detaching at the
recorded SHA means leftover state from a previous review turn can never
block the next one, and the checkout matches the SHA recorded in
`reviewed` exactly. The model is told the checkout is read-only.

Both the review-request and tracked passes prepare the checkout this
way. Preparation failure logs a warning and skips the PR for the tick
without writing state, so the next tick retries naturally. For tracked
PRs, push turns retry via the SHA delta; a comment-only turn is lost
once `last_poll` advances — accepted, since prep failures on an
existing clone are transient. The head SHA
must be a 40-char hex string and the base ref must not start with `-`;
both come from the GitHub API, but git would parse an option-shaped
value as a flag.

### Bot identity

Resolved on startup via `gh api user`. All reviews/comments authored by
this login are skipped to prevent self-reply loops; PRs authored by this
login are excluded from the review-request path (GitHub rejects
self-reviews anyway).

### Access control

The bot owner (`github.owner`) is always trusted. Additional users can be
granted access via `github.trusted_users`, and bot apps (e.g. code-review
bots) via `github.trusted_bots`. All are case-insensitive; a trailing
`[bot]` suffix on a login is stripped before matching the bot list, since
the REST API appends it and GraphQL does not. Untrusted items are logged
and skipped.

- **Feedback path**: trust is checked on the review/comment author.
- **Review-request path**: trust is checked on the **PR author** — the
  search result does not carry who requested the review, and the author
  is whose code runs through the bot's context.

### Feedback on own PRs

For each of the bot's open PRs, fetch reviews and comments
(`gh pr view --json reviews,comments`) and inline diff comments
(`gh api repos/{nwo}/pulls/{n}/comments`). Skip the bot's own items,
items older than `last_poll`, and untrusted authors. Message formats:

- Review: `Review on PR #5 "Title" (owner/repo) by @alice: APPROVED\n\nBody`
- Comment: `Comment on PR #5 "Title" (owner/repo) by @carol:\n\nBody`
- Diff comment: `Inline comment on PR #5 "Title" (owner/repo) by @dave at src/main.rs:42:\n\nBody`

### Review requests

Each PR from the review-request query is a review candidate, filtered:

| Check | Behavior |
|-------|----------|
| PR authored by the bot | Skip |
| PR author untrusted | Log warning, skip |
| Already dispatched for this head SHA | Skip (see State) |

The dispatched message carries PR number, title, repo, author, and
body, plus mechanically fetched context and an instruction block. The
context — base branch, changed-file list with add/del counts, and full
commit messages (headline and body) — comes from the same `gh pr view`
call that resolves the head SHA, so it costs no extra request and
matches the SHA recorded in `reviewed` exactly: the review cannot race
a push and judge commits it was not dispatched for. Commit messages
are required reading for every review (they carry the rationale the
code is checked against), so the harness supplies them instead of
prompting the model to fetch them. The diff is deliberately NOT
packed: the root produces it on demand and hands it to the reviewer by
reference (see the protocol below), so it never has to sit in a
message at all.

The review protocol itself is static choreography and lives in a role
segment ([spec 06](06-system-prompt.md)), appended to every turn
dispatched with `role: reviewer`; the dispatch message carries only the
per-turn facts above. The protocol:

**The root review turn orchestrates; the `reviewer` sub-agent
([spec 23](23-self-review.md)) judges.** Two reasons, recorded here
because both outlast the implementation. First, model strength: the
reviewer role carries its own model override, and an external PR
review — read by a human teammate — is the most outward-facing
judgment the bot produces; it should not run on a weaker model than
the bot's internal self-checks. Second, containment: PR-head content
is untrusted input, and the reviewer sub-agent is read-only by
construction — no exec, no git, no GitHub tools — so the judgment
pass over untrusted code happens in the most tool-restricted context
available, rather than in a root turn holding outward-facing tools.

- The PR head is already checked out at `reviews/<owner>/<repo>`,
  detached at the recorded SHA with the base branch fetched (see
  Review checkout). Read-only: git only to read; never
  `gh pr checkout`. The working checkout under `projects/` is not
  involved.
- The root produces the diff by redirecting
  `git diff origin/<base>...HEAD` to a file under `reviews/.diffs/`,
  and packs its **path** into the reviewer dispatch together with the
  PR's stated intent (title, body, commit messages), the checkout
  root, and review metadata `{repo, gate: "pr", git_ref: <head SHA>}`
  so the verdict lands in the findings ledger. The reviewer cannot
  produce diffs itself (no git), which is why the root produces it.
  By reference rather than by value because a packed diff sits in the
  root's working set twice — once as the exec result, once in the
  `task` call the root writes — and that second copy is an assistant
  message, which the context engine does not externalize at any size
  ([spec 14](14-context-engine.md)). By reference the root holds none
  of the diff, the reviewer reads all of it instead of a head/tail
  excerpt, and PR size stops bounding the review. The root does not
  read the diff: it is not the judge, and a diff in its context is one
  the reviewer's verdict has to compete with.
- The reviewer returns prose findings and the findings block; the
  root translates. Verdict `correct` → `APPROVE`; `incorrect` →
  `COMMENT` with each finding as a resolvable inline thread. The root
  owns anchoring (paths and lines from the findings), suggestion
  blocks where a finding carries a concrete replacement (consent and
  authorship stay with the author; the no-push invariant holds), and
  submission via `github_pr_review_submit` — `body`, `event`
  (`APPROVE` or `COMMENT` only; `REQUEST_CHANGES` is unrepresentable,
  blocking judgments stay with humans), `comments`. Each `comments`
  entry carries a single right-side `line`, so a replacement spanning
  more lines than can be anchored becomes prose with file:line
  references. On anchoring failure the finding moves into `body` with
  a file:line reference and the review is resubmitted; since the root
  anchors without having read the diff, a repeatedly rejected
  submission reads hunk headers (`--unified=0`) for the touched line
  ranges — anchoring data, not the hunk bodies. A formal review is
  required — submitting it clears the pending request and stops
  re-triggering.
- A failed reviewer call is a skipped review ([spec
  23](23-self-review.md)): the root judges the diff itself and says so
  in the review body. Restated here because the review-gates segment
  carrying that disclosure norm is gated on `review.enabled` while
  this protocol is not, so the obligation would otherwise vanish
  exactly when a human is reading the result.
- Comment discipline is the reviewer prompt's own (spec 23): suspect
  or needs-change only, no praise threads, no manufactured findings.
  The root does not add findings of its own — one judge per review.
- Never push to the PR branch, never merge, never close.

### Re-reviews on push

Once reviewed, a PR stays tracked until it closes — no explicit
re-request needed. A new head SHA triggers an incremental re-review,
scoped to the delta in the context of the prior review. The dispatched
message carries the previously reviewed SHA and instructs the model to:

- Produce the incremental diff, not the whole PR: the channel has
  already prepared the review checkout (same as the initial review —
  new head detached, read-only), so `git diff {prev}...HEAD` in it, to
  the same `reviews/.diffs/` path convention. Three dots: after a force
  push `{prev}` is no longer an ancestor, and diffing from the merge
  base degrades better than diffing against a diverged tip. Falls back
  to the full `gh pr diff` when that fails anyway.
- Recall the prior review from `gh pr view --json reviews`, which is
  what was actually published. The work session may still carry it, but
  the API is the source of truth and the session is a cache of it.
- Dispatch the reviewer with the delta diff, the prior review's
  substance, and the question the initial review does not ask: does
  the delta address that feedback adequately and without introducing
  new problems? A full re-review of untouched code is explicitly not
  wanted. Same metadata shape, `git_ref` the new head SHA.
- Translate and submit as in the initial review: `correct` →
  `APPROVE` (the feedback is addressed), `incorrect` → `COMMENT`
  naming the remaining gaps as inline threads.

The `reviewed` entry updates to the new SHA on dispatch, so each push
gets at most one incremental turn.

### Review thread follow-ups

Humans can push back on the bot's review comments, and the bot holds up
its end of the discussion. For each tracked PR, new comments since
`last_poll` — PR-level comments and inline diff comment replies — from
trusted users are dispatched with an instruction to engage on the
merits: agree, state what that concedes about the original comment, or
disagree and explain why, with specifics. Replies go to the same thread
(inline replies via `github_pr_diff_reply`, PR comments via a normal
comment). Going quiet is not an option; neither is reflexively
defending a bad take.

When a commenter asks the bot to implement the fix, the answer is a
suggestion block in the inline thread, never a push — same mechanism as
the proactive suggestions in reviews. Fixes that do not fit the
commented lines are spelled out with file:line references instead.
Pushing to the PR branch directly remains future work (needs
`headRefName`, fork handling, and a self-review guard).

The bot responds only to human comments, never to its own, so threads
terminate when the human stops replying.

Follow-up turns are also where the bot's own published findings get
dispositioned ([spec 23](23-self-review.md)), not at submission time.
A `pr`-gate finding stays pending until its author answers it: `fixed`
when they take the change, `disputed` with their reason when they
contest it, `no-action` when it is dropped without objection. A dispute
is recorded whether or not the bot concedes — that a human argued at
all is the signal, and which way it went belongs in the note. This is
the only place the ledger observes a human disputing a finding; the
self-gates can only ever record the bot disputing itself, which is the
weakest calibration signal available. `pending` on a `pr` finding
therefore means awaiting the author, not lapsed discipline. Finding ids
come from the review turn that published them, in the work session's
history and recoverable with `lcm_grep`; the ledger holds them
regardless, keyed by repo and `git_ref`.

### Same-tick push and comments

A push and new comments can land in the same tick, and their true order
is not knowable: comment timestamps are server-side, but push time is
not exposed (commit dates are author-controlled; GraphQL's
`Commit.pushedDate` is deprecated and returns null). So no ordering is
attempted — both fold into a single turn whose message carries the
incremental diff and the new comments together. A comment may already be
answered by the push it raced; one turn lets the model say so instead of
replying to stale code.

### State persistence

`state/github_poll_state.json` via atomic write (tmp + rename):

```json
{"last_poll": "2026-07-05T12:00:00Z", "reviewed": {"owner/repo#42": "<head-sha>"}}
```

- Missing or corrupt state defaults to `last_poll = now` (avoids
  replaying entire PR histories) and an empty `reviewed` map (worst case
  one duplicate full review, visible and harmless; tracked-PR follow-up
  threads are forgotten until the PR is re-requested).
- `reviewed` maps each tracked PR to the last head SHA dispatched for
  review. Dispatch records the SHA before the turn runs; a PR
  reappearing with the same SHA is skipped — this covers the failure
  loop where the model replies without submitting a formal review, which
  would otherwise re-trigger every tick.
- A new head SHA on a tracked PR dispatches an incremental re-review
  (see above).
- Entries are pruned when the PR is closed or merged. Unlike Linear's
  announced set, absence from the review-requested search is not a
  prune signal — submitting a review clears the pending request, and the
  PR must stay tracked for re-reviews and follow-ups.

### Configuration

| Config key | Default | Description |
|------------|---------|-------------|
| `github.enabled` | `false` | Enable the GitHub channel |
| `github.poll_interval_secs` | `300` | Seconds between poll cycles |
| `github.owner` | — | Bot owner's GitHub username (required when enabled) |
| `github.trusted_users` | `[]` | Additional trusted GitHub usernames |
| `github.trusted_bots` | `[]` | Bot app logins whose PR feedback to act on |

There is no separate flag for the review-request trigger: the trust
check on the PR author already gates who can put code in front of the
bot, and GitHub only lets repo collaborators request reviews in the
first place. Requires the `github-token` secret.

**Activity events**: not forwarded (passes `None` for activity sender).

## Boundaries

### Owns

- The poll passes (own PRs, review requests, tracked reviewed PRs) and
  their filtering
- Review checkout preparation under `reviews/`
- Bot identity resolution and self-reply prevention
- Message formatting for reviews, comments, diff comments, review
  requests, and follow-up discussions
- The review, re-review, and discussion instruction blocks
- Poll state persistence (`last_poll`, `reviewed`)

### Does Not Own

- Agent turns — `AgentHandle::send_message`, as everywhere
- Session routing — the actor routes on the repo hint (spec 14)
- GitHub tools (`github_pr_create`, `github_gh`, ...) — spec 03; the
  model drives `gh` through the exec tool during review turns, there is
  no dedicated review tool
- Input classification (message vs command) — the actor
- Merging or branch mutation — explicitly out of scope

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Bot login resolution fails | Log error, park forever (no polling) |
| PR list/fetch fails | Log error, retry next tick without advancing `last_poll` |
| Individual message send fails | Log error, continue with remaining items |
| Agent turn fails (review) | Logged and alerted via the notifier (spec 17). SHA already recorded, so no retry storm; the next push or a human re-request retries. |
| Model never submits a formal review | Pending request stays, but the SHA guard prevents re-dispatch. Visible as a stale request on the PR. |
| Review checkout prep fails (clone/fetch/detach) | Log warning, skip the PR this tick without recording state; retried next tick |
| Head SHA / tracked-PR fetch fails | Skip the PR this tick |
| Incremental compare fetch fails | The model falls back to the full diff |
| State file corrupt | Defaults: `last_poll = now`, empty `reviewed` map |

No channel failure crashes the daemon; a disabled or failed channel
resolves to `std::future::pending()` and parks forever.

## Constraints

- Review only: no pushing, merging, closing, or label mutation
- Review verdicts are `COMMENT` or `APPROVE` — never `REQUEST_CHANGES`;
  blocking judgments stay with humans
- The bot never resolves review threads, including its own addressed
  comments after an `--approve` — resolution belongs to the author
- One review turn per (PR, head SHA); discussion turns are additionally
  bounded by the human replying
- The PR diff is untrusted input even from trusted authors (vendored
  code, generated files). It enters the context through the exec tool
  and gets the standard `<tool_output>` framing (spec 11); the
  instruction block reminds the model that diff content is data, not
  instructions.
- No draft-PR filtering in v1: request a review on a draft, get a review
- Text only, no message queuing — shared channel constraints (spec 10)

## Open Questions

None currently. Resolved during drafting:

- Thread resolution stays with the author, even after an `--approve`.
- Tracked-PR polling cost: pruning on close is sufficient; no size cap.
- Same-tick push + comment: one combined turn (ordering is not
  observable anyway — see Same-tick push and comments).
