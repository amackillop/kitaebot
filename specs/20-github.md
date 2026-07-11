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
with `ChannelSource::GitHub { pr_number, repo }`. The session key splits
by direction: feedback on the bot's own PRs routes to the repo's work
session (`owner/repo`, the same key as Linear issue routing), while
review and re-review/discussion turns route to `review:owner/repo`.
Reviewing and building the same repo are different conversations —
prior-review context lives in the review session, in-progress work in
the work session, and neither compacts the other away. `last_poll`
advances only after a successful poll.

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
granted access via `github.trusted_users`. Both are case-insensitive.
Untrusted items are logged and skipped.

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
packed: which files to read in full is a judgment call, and packing
all diffs just moves the size problem into the User message.

The instruction block:

- The PR head is already checked out at `reviews/<owner>/<repo>`,
  detached at the recorded SHA with the base branch fetched (see Review
  checkout). Treat it as read-only: git only to read (diff, log, show);
  no branch switching, edits, or stashing, and never `gh pr checkout`
  (`github_gh` blocks it anyway, [spec 03](03-tools.md)). The working
  checkout under `projects/` is not involved.
- Read the diff per file: the changed-file list and commit messages
  are already in the message, so go straight to
  `git diff origin/<base>...HEAD -- <path>` in the review checkout for
  each file worth reading. The full `gh pr diff` output typically
  exceeds the tool-output threshold ([spec 14](14-context-engine.md))
  and comes back as an excerpt the root cannot expand; per-file diffs
  stay readable. Commit messages carry the rationale for the change —
  the why, the trade-offs, the alternatives rejected. They inform the
  review, and the review checks that the code actually does what they
  say.
- The prompt states the externalization contract: oversized tool
  output becomes a `<file>` reference with a head/tail excerpt, the
  full text remains searchable via `lcm_grep`, and re-running the
  command with different flags to shrink it is a waste of a turn.
- Context beyond the diff (usage of changed code, existing behavior,
  test coverage) goes through the `task` tool (explore, [spec
  19](19-sub-agents.md)) against files in the review checkout, with
  specific questions and file:line evidence required in the answer. Direct file
  reads are reserved for judging a hunk whose surrounding code the diff
  does not show. This keeps cross-file tracing out of the root context;
  findings still land there via the sub-agent's summary.
- Review for correctness, security, and design. Be specific: file and
  line references, not vibes.
- Comments are for what is suspect or needs to change. No praise
  comments — inline threads exist to be resolved, and "nice" resolves
  nothing. A truly remarkable observation gets one line in the review
  body.
- Findings with a concrete better version carry a GitHub suggestion
  block with the replacement lines: the author commits it with one
  click, so consent and authorship stay with them and the bot's no-push
  invariant holds. This covers mechanical fixes (typo, off-by-one,
  wrong constant) and cleaner shapes for the commented lines alike;
  findings that need discussion rather than replacement lines get
  prose.
- Submit one formal review with the `github_pr_review_submit` tool
  ([spec 03](03-tools.md)): `body` (summary and verdict), `event`
  (`APPROVE` or `COMMENT`), and `comments` (path/line/body array).
  Inline findings each become a resolvable thread, which is what the
  follow-up path engages with. `repo_dir` is the review checkout. On failure
  (usually bad
  line anchoring), the affected finding moves into `body` with a
  file:line reference and the review is resubmitted. A formal review
  (not a plain comment) is required — submitting it is what clears the
  pending request and stops re-triggering. `REQUEST_CHANGES` is
  unrepresentable in the tool: blocking judgments stay with humans; a
  critical finding is a `COMMENT` review that says so.
- Never push to the PR branch, never merge, never close.

### Re-reviews on push

Once reviewed, a PR stays tracked until it closes — no explicit
re-request needed. A new head SHA triggers an incremental re-review,
scoped to the delta in the context of the prior review. The dispatched
message carries the previously reviewed SHA and instructs the model to:

- Read the incremental diff, not the whole PR: the channel has already
  prepared the review checkout (same as the initial review — new head
  detached, read-only), so `git log` and `git diff` over `{prev}..HEAD`
  in it, falling back to the full `gh pr diff` when that fails (e.g.
  after a force push).
- Recall its prior review: the review session carries it, and
  `gh pr view --json reviews` recovers the submitted text if compaction
  ate the details.
- Judge the delta against that feedback: does it address the prior
  review adequately and without introducing new bugs? A full re-review
  of untouched code is explicitly not wanted.
- Delegate any context-gathering beyond the diff to the `task` tool
  (explore), same as the initial review.
- Submit a formal review via `github_pr_review_submit`: `APPROVE` when
  the feedback is addressed, `COMMENT` naming the remaining gaps
  otherwise. Same comment discipline as the initial review: suspect or
  needs-change only, no praise, suggestion blocks where a concrete
  better version exists.

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
