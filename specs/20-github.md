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
2. **Review requests** (when `github.review_requests` is on):
   `gh search prs --review-requested=@me --state=open`.
3. **Tracked reviewed PRs** (same flag): for each PR in the `reviewed`
   map, fetch state, head SHA, and new comments. Closed/merged PRs are
   pruned; a new head SHA triggers an incremental re-review; new trusted
   comments trigger a discussion turn; both in one tick fold into a
   single combined turn.

Items are filtered (see below) and dispatched through the agent handle
with `ChannelSource::GitHub { pr_number, repo }` and the repo
(`owner/repo`) as session hint — the same session key as Linear issue
routing, so a repo's authored PRs, tickets, and reviews share one session.
`last_poll` advances only after a successful poll.

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

The dispatched message carries PR number, title, repo, author, and body,
plus an instruction block:

- Fetch the diff (`gh pr diff`) and whatever surrounding context is
  needed (`gh pr view`, file reads via a cloned checkout if warranted).
- Review for correctness, security, and design. Be specific: file and
  line references, not vibes.
- Submit a formal review via
  `gh pr review <n> -R <repo> --comment|--approve`. A formal review (not
  a plain comment) is required — submitting it is what clears the pending
  request and stops re-triggering. `--request-changes` is not used:
  blocking judgments stay with humans; a critical finding is a
  `--comment` review that says so.
- Never push to the PR branch, never merge, never close.

### Re-reviews on push

Once reviewed, a PR stays tracked until it closes — no explicit
re-request needed. A new head SHA triggers an incremental re-review,
scoped to the delta in the context of the prior review. The dispatched
message carries the previously reviewed SHA and instructs the model to:

- Fetch the incremental diff
  (`gh api repos/{nwo}/compare/{prev}...{head}`), not the whole PR.
- Recall its prior review: the repo session carries it, and
  `gh pr view --json reviews` recovers the submitted text if compaction
  ate the details.
- Judge the delta against that feedback: does it address the prior
  review adequately and without introducing new bugs? A full re-review
  of untouched code is explicitly not wanted.
- Submit a formal review: `--approve` when the feedback is addressed,
  `--comment` naming the remaining gaps otherwise.

The `reviewed` entry updates to the new SHA on dispatch, so each push
gets at most one incremental turn.

### Review thread follow-ups

Humans can push back on the bot's review comments, and the bot holds up
its end of the discussion. For each tracked PR, new comments since
`last_poll` — PR-level comments and inline diff comment replies — from
trusted users are dispatched with an instruction to engage on the
merits: agree, state what that concedes about the original comment, or
disagree and explain why, with specifics. Replies go to the same thread
(inline replies via the diff-comment reply endpoint, PR comments via a
normal comment). Going quiet is not an option; neither is reflexively
defending a bad take.

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

`memory/github_poll_state.json` via atomic write (tmp + rename):

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
| `github.review_requests` | `false` | Enable the review-request trigger |

`review_requests` is a separate flag because granting the bot review
duties is a bigger trust step than letting it read feedback on its own
PRs. Requires the `github-token` secret.

**Activity events**: not forwarded (passes `None` for activity sender).

## Boundaries

### Owns

- The poll passes (own PRs, review requests, tracked reviewed PRs) and
  their filtering
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
| Agent turn fails (review) | Logged. SHA already recorded, so no retry storm; the next push or a human re-request retries. |
| Model never submits a formal review | Pending request stays, but the SHA guard prevents re-dispatch. Visible as a stale request on the PR. |
| Head SHA / tracked-PR fetch fails | Skip the PR this tick |
| Incremental compare fetch fails | The model falls back to the full diff |
| State file corrupt | Defaults: `last_poll = now`, empty `reviewed` map |

No channel failure crashes the daemon; a disabled or failed channel
resolves to `std::future::pending()` and parks forever.

## Constraints

- Review only: no pushing, merging, closing, or label mutation
- Review verdicts are `--comment` or `--approve` — never
  `--request-changes`; blocking judgments stay with humans
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
