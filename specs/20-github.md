# Spec 20: GitHub Channel

## Motivation

The GitHub channel connects the bot to pull requests in three directions:

1. **Feedback on its own PRs**: the bot opens PRs (via GitHub tools or the
   Linear flow) and humans respond with reviews and comments. The channel
   polls for that feedback and turns it into agent turns, so the bot can
   revise its work.
2. **Reviewing others' PRs**: a human requests a review from the bot's
   account and the bot reviews the PR. Review requests are explicit,
   per-PR, auditable in the PR timeline, and self-clearing (GitHub drops
   the pending request once a review is submitted) — no mention parsing
   needed.
3. **PRs it contributes to**: the bot pushes fixes to third-party PRs —
   e.g. failing Dependabot PRs under a dependency duty (spec 24) — and
   humans respond there. Without this direction those responses land on
   a PR no pass watches and are never heard.

Both directions share one poll loop, one identity, one trust list, and one
state file. The GitHub *tools* (PR creation, issue creation, CI status, the
`github_api` escape hatch) are part of the tool registry and stay in spec 03;
this spec owns the PR channel. Issue polling is a separate channel with its
own loop and state, documented in [spec 25](25-github-issues.md); it shares
this channel's REST client, identity, and trust model.

**Rate-limit discipline.** All GitHub API traffic — both poll loops and
the model's tools — flows through one shared client whose requests
serialize through a gate: one request in flight at a time, at least a
second apart, per GitHub's best-practice guidance. A rate-limited
response (429, or 403 with a `Retry-After` header or rate-limit
message) surfaces as the distinct `GithubError::RateLimited` and pushes
the gate out by the `Retry-After` value (one minute when absent), so
every caller waits out a limit any caller hit. Channels treat
`RateLimited` as transient: comment-post retries pass back through the
gate and so retry only after the server-mandated cooldown.

## Behavior

### Poll loop

`tokio::time::interval` with `MissedTickBehavior::Skip`. Each tick runs
four passes:

1. **Own PRs**: `GET /search/issues` with `is:pr is:open author:{bot}`,
   then per PR fetch reviews, comments, and inline diff comments.
2. **Review requests**: `GET /search/issues` with
   `is:pr is:open review-requested:{bot}`.
3. **Tracked reviewed PRs**: for each PR in the `reviewed`
   map, fetch state, head SHA, comments, and reviews (the reviews
   date review-linked diff comments by `submitted_at`). Closed/merged
   PRs are pruned; a new head SHA triggers an incremental re-review;
   new trusted comments trigger a discussion turn; both in one tick
   fold into a single combined turn.
4. **Contributed PRs**: `GET /search/issues` with
   `is:pr is:open commenter:{bot} -author:{bot} -review-requested:{bot}`,
   minus PRs in the `reviewed` map; new trusted feedback folds into one
   discussion turn per PR. Runs last so `reviewed` reflects this tick's
   inserts and prunes.

Items are filtered (see below) and dispatched through the agent handle
with `ChannelSource::GitHub { pr_number, repo, role }`, where `role` is
`author` (feedback on the bot's own PR), `contributor` (discussion on a
third-party PR the bot intervened on), or `reviewer` (a review
request, a re-review, a discussion turn on a PR the bot reviewed). The
channel knows which of its four poll passes produced each item, so the
role costs nothing to carry. It selects the review protocol segment
([spec 06](06-system-prompt.md)) and nothing else.

**Author and contributor turns route to the repo's work session**
(`owner/repo`, the same key as Linear issue routing). **Reviewer turns
route to a per-PR review session** (`review:{nwo}#{n}`). `last_poll`
advances only after a successful poll, and only to the tick's start
time: turns run inline between passes, so a post-dispatch timestamp
would swallow feedback that arrived mid-tick on a PR whose snapshot
predates it — advancing to tick start re-examines such items next
tick, trading a seconds-scale duplicate window for the absence of
silent loss.

The review-session split is a cost decision, and the shape matters.
The root review turn is mechanical, but its calls multiply — anchoring
retries, resubmissions, dispositions ran one PR #106 re-review to 33
calls — and in the work session every call hauled that session's full
history: ~103k prompt tokens per call, ~88k of it history, 3.9M prompt
tokens and the largest single line item in the review's cost, spent on
the one participant that judges nothing. A per-PR session holds only
the static prompt and this PR's own rounds, so the per-call cost stays
at the ~20k floor for the PR's life. Per-PR rather than per-repo
because a shared review session drifts into the same compaction band
the work session lives in (the saving erodes), while same-PR rounds —
initial review, re-reviews, discussion — are the recall that is
actually wanted, and they stay uncompacted in a session this small.
The session stops receiving turns when the PR closes; distillation
drains it via per-session watermarks like any other, after which it
sits inert on disk. Nothing prunes the files yet — pruning hooks into
the tracked pass's close detection if the inert tail ever costs
anything.

An earlier revision routed reviews to a per-repo `review:{nwo}`
session, to keep prior-review context from compacting in-progress work
away and to accumulate repo knowledge across reviews. Those
justifications did not survive, and their obsolescence is what makes
the lean session safe — each mechanism that made the work session
unnecessary for correctness is also why the review session can carry
nothing:

- **Judgment isolation** belongs to the `reviewer` sub-agent
  ([spec 23](23-self-review.md)). The diff and the code reading happen
  in an ephemeral context that is discarded; the root review turn is
  four mechanical calls and holds no diff at all.
- **Knowledge accumulation** belongs to memory ([spec
  21](21-memory.md)). Distillation gathers pending spans across *all*
  sessions with per-session watermarks, so reviewer output reaches
  `memory/topics/` wherever the turn ran — the review session was never
  load-bearing for it. What the reviewer needs back is
  `state/review-checklist.md` and the findings ledger, both
  session-independent.
- **Prior-review recall** is authoritative on GitHub, not in a
  session: `github_pr_reviews` returns what was actually published,
  where session history is what the bot believes it said after
  compaction. The re-review dispatch also carries the prior round's
  ledger findings, so nothing about a review turn's inputs assumes a
  session that has seen the repo before.

The prompt segments stay keyed on the dispatch role, not the session
name — that decision stands from the per-repo retirement; a session is
where history accumulates, a role is a property of the turn. And the
reviewer sub-agent keeps no LCM tools: with review turns in their own
sessions the archive it would read is review history rather than the
author's context, but at the self-review gates the same agent type
still runs against the work session, and cross-review knowledge
reaches the judge through the checklist and memory — curated through
ledger calibration — not through replaying its own past verdicts.

### Review checkout

Review turns never touch the working tree under `projects/`. Before
dispatching a review, the channel prepares a **worktree** of the repo's
working clone at `reviews/<owner>/<repo>`: ensure
`projects/<owner>/<repo>` is cloned, `git worktree prune`,
`git worktree add --detach` on first use, then
`git fetch origin <base> pull/{n}/head` and
`git checkout --force --detach <head-sha>` inside it. Force-detaching at
the recorded SHA means leftover state from a previous review turn can
never block the next one, and the checkout matches the SHA recorded in
`reviewed` exactly. The model is told the checkout is read-only.

A worktree rather than a second clone because the object store is
shared: no duplicate full fetch per repo, and a review of a repo the bot
already works on costs almost nothing. `--detach` is load-bearing — git
refuses to check the same *branch* out in two worktrees, and a review
head that happens to match the working branch would otherwise collide.
`prune` precedes `add` because a deleted directory leaves registration
metadata behind that makes `add` fail.

Two consequences of sharing, recorded because the separate clone did not
have them:

- **Reviewing implies cloning.** A repo the bot has only ever reviewed
  now gets a `projects/` clone as a side effect. Review preparation
  clones without provisioning a devShell (nothing on the review path
  consumes one) and leaves warming to whoever first does actual work
  there — `git_clone` warms on its exists-path, and `exec`'s
  Blocked-then-re-allow fallback covers the rest. Preparation must not
  reintroduce the devShell cost one level up.
- **Untrusted objects land in the working clone.** `pull/{n}/head`
  fetches a contributor's objects into the object database the bot
  commits from, and `origin/pull/*` refs become visible to the working
  tree. HEAD stays independent per worktree, so nothing is checked out
  implicitly, and the workflow branches from `origin/HEAD`
  ([spec 06](06-system-prompt.md), AGENTS.md). Accepted, but it is a new
  adjacency: the separate clone kept PR content in a directory the
  working tree could not reach.

Diffs packed by reference go to `.diffs/` at the workspace root, not
under `reviews/`. They are not review-specific — spec 23's open question
has the self gates packing the same way — and `reviews/` now holds
worktrees of other repositories, which is the wrong parent for a scratch
directory.

Both the review-request and tracked passes prepare the checkout this
way. Preparation failure logs a warning and skips the PR for the tick
without writing state, so the next tick retries naturally. For tracked
PRs, push turns retry via the SHA delta; a comment-only turn is lost
once `last_poll` advances — accepted, since prep failures on an
established worktree are transient. First-use preparation is not in that
class: it clones and registers a worktree, and can fail for reasons that
persist across ticks (no disk, no network, a repo the API can reach
but `git` cannot). Such a failure costs a comment turn per tick until it is
fixed, and is loud in the log rather than silent. The head SHA
must be a 40-char hex string and the base ref must not start with `-`;
both come from the GitHub API, but git would parse an option-shaped
value as a flag.

### Bot identity

Resolved on startup via `GET /user`. All reviews/comments authored by
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
- **Contributed path**: trust is checked on the review/comment author;
  the PR author is untrusted by design. The authorization chain is the
  bot's own choice to intervene plus a trusted human commenting — not
  the third party's identity. In particular `dependabot` must not be
  added to `trusted_bots` to make this work: that list means "whose
  feedback the bot acts on", a much larger grant than needed here.

### Feedback on own PRs

For each of the bot's open PRs, fetch reviews
(`/repos/{nwo}/pulls/{n}/reviews`), conversation comments
(`/repos/{nwo}/issues/{n}/comments`), and inline diff comments
(`/repos/{nwo}/pulls/{n}/comments`). Skip the bot's own items,
items older than `last_poll`, and untrusted authors. A diff
comment linked to a review is dated by that review's
`submitted_at`, not its own `created_at`: GitHub stamps a
pending-review comment at draft time, so a draft-then-submit
review's comments would otherwise be older than every future
`last_poll` and filtered out of the very event they belong to —
permanently. (An in-thread reply carries the id of its own,
newly created review, so replies date correctly too.) While the
parent review is still pending, its comments are invisible
drafts and skip with it. All new
feedback on one PR folds into **one turn per PR per tick** — replies
must not race each other on the same branch, and a review with N
inline comments is one logical event, not N+1 turns. A per-PR fetch
failure skips that PR for the tick (one persistently broken PR must
not wedge the cursor and starve every other PR's feedback); a
search failure propagates so `last_poll` does not advance. Message
formats for the items inside the batched message:

- Review: `Review on PR #5 "Title" (owner/repo) by @alice: APPROVED\n\nBody`
- Comment: `Comment on PR #5 "Title" (owner/repo) by @carol:\n\nBody`
- Diff comment: `Inline comment on PR #5 "Title" (owner/repo) by @dave at src/main.rs:42 (comment id 12):\n\nBody`

The diff comment carries its id so the turn can reply in-thread
(`github_pr_diff_reply`) without a fetch round-trip. Item bodies are
inlined verbatim and unbounded — they are the work order, and a
truncated body is a dropped request. The fold bounds turn count, not
message size; oversized messages are externalized downstream by the
context engine ([spec 14](14-context-engine.md)).

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
commit messages (headline and body) — is fetched in the same tick as
the head SHA (`/pulls/{n}` plus its `/commits` and `/files`
sub-resources). A push landing between those calls can list commits
newer than the recorded SHA; the checkout is still prepared at the
recorded SHA, and the tracked pass re-reviews the new head on the next
tick, so the race heals itself. Commit messages
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
  Review checkout). Read-only: git only to read; never a checkout
  over it. The working checkout under `projects/` is not involved.
- The root produces the diff by redirecting
  `git diff origin/<base>...HEAD` to a file under `.diffs/`,
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
message carries the previously reviewed SHA, the ledger's findings for
the prior round (gate `pr` at that SHA — id, severity, note, and any
disposition), and instructs the model to:

- Produce the incremental diff, not the whole PR: the channel has
  already prepared the review checkout (same as the initial review —
  new head detached, read-only), so `git diff {prev}...HEAD` in it, to
  the same `.diffs/` path convention. Three dots: after a force
  push `{prev}` is no longer an ancestor, and diffing from the merge
  base degrades better than diffing against a diverged tip. Falls back
  to the full `gh pr diff` when that fails anyway.
- Recall the prior review from `gh pr view --json reviews`, which is
  what was actually published. The work session may still carry it, but
  the API is the source of truth and the session is a cache of it. The
  ledger findings in the dispatch complement it rather than replace
  it: ids and severities never appear in the published text, and the
  per-finding dispositions the protocol demands need the ids. Session
  history used to be the only source of them, which made dispositions
  an archaeology exercise after compaction.
- Dispatch the reviewer with the delta diff, the prior review's
  substance including its pending ledger findings, and the question
  the initial review does not ask: does the delta address that
  feedback adequately and without introducing new problems? A full
  re-review of untouched code is explicitly not wanted. Same metadata
  shape, `git_ref` the new head SHA.
- Translate and submit as in the initial review: `correct` →
  `APPROVE` (the feedback is addressed), `incorrect` → `COMMENT`
  naming the remaining gaps as inline threads. Then disposition each
  pending finding by its id from the dispatch.

The `reviewed` entry updates to the new SHA on dispatch, so each push
gets at most one incremental turn.

**The quiet path.** A push landing on a standing approval usually just
polishes the nits that approval carried, and publishing another
`APPROVE` over it is noise — PR #106's third round had nothing to say
beyond "both nits addressed" (issue #112). When the bot's latest
published review is an `APPROVE` and nothing pending in the ledger for
the prior round is worse than a `nit`, the dispatch says so, and the
protocol licenses silence: the reviewer round still runs — nit fixes
can introduce defects, which is exactly what re-reviews exist for —
but a verdict of `correct` with no new findings on a delta that stays
within the pending feedback's scope publishes nothing. The standing
approval covers the push; the findings close through their normal
dispositions, whose notes carry what was verified. The alternative —
skipping the dispatch entirely and closing the findings mechanically —
was rejected: the channel cannot tell a nit-fix push from new work
without reading the delta, and a mechanical `fixed` records a
verification nobody performed.

The round stays visible without a published artifact: the reviewer
invocation records its `reviews` row and the dispositions their
timestamps ([spec 23](23-self-review.md)); what was published remains
the GitHub API's record. The eligibility gate is mechanical and
deliberately strict — a pending `should-fix`, or anything but an
`APPROVE` as the latest submitted bot review, publishes as usual. A
delta that regresses or does new work beyond the feedback also
publishes as usual; the reviewer judges scope, which is why the
dispatch still runs. Both invariants above stand: each push gets at
most one incremental turn, and every push gets its reviewer round.

**Closure prose.** When a re-review does publish, its per-finding
closures are one line each ("`<sha>` — confirmed fixed"), split by
destination: the published body carries the verdict and the closure
lines, the disposition notes carry the verification claims — what was
checked, against what. Recapping the fix commit in the body narrates
what its author just wrote; the verification claim is the only part
the review adds, and the ledger, not the body, is the audit trail
later escapes are judged against. Prose in the body is reserved for
findings that are *not* closed and for checks non-obvious enough that
a bare line would be an unsupported claim. Thinning the body is safe
only because the dispatch feeds the prior findings from the ledger
(above); before that, the published body — recalled via the GitHub
API — was what carried closure context into the next round, and
thinning it would have starved that round.

### Review thread follow-ups

Humans can push back on the bot's review comments, and the bot holds up
its end of the discussion. For each tracked PR, new comments since
`last_poll` — PR-level comments and inline diff comment replies — from
trusted users are dispatched with an instruction to engage on the
merits: agree, state what that concedes about the original comment, or
disagree and explain why, with specifics. Replies go to the same thread
(inline replies via `github_pr_diff_reply`, PR comments via a normal
comment). Going quiet is not an option; neither is reflexively
defending a bad take. Review-linked diff comments are dated by their
review's `submitted_at` like the feedback pass (the tracked pass
fetches reviews for exactly this), so a draft-then-submit reply event
is never lost to draft-time stamps.

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
arrive in the dispatch, read back from the ledger by repo and the
tracked `git_ref`; ids from an older round (a prior head SHA) are in
the work session's history from the turn that published them,
recoverable with `lcm_grep`.

### Contributed PRs

The bot pushes fixes to PRs it does not own — a duty (spec 24) repairing
a failing Dependabot PR is the canonical case — and the humans watching
that PR need a way to steer it. The contributed pass turns their
feedback into agent turns.

**Discovery contract**: the bot leaves a PR conversation comment
whenever it intervenes on a PR it does not own, and that comment is
what makes the PR discoverable — the search qualifier `commenter:` only
reliably matches conversation comments, and pushed commits are not
searchable (a Dependabot rebase can also evict them, while the comment
survives). An intervention that leaves no comment leaves no trail for
this pass; the duty procedures must keep commenting.

The query is `is:pr is:open commenter:{bot} -author:{bot}
-review-requested:{bot}` — the negations keep the other passes' PRs out
of the search's 50-item budget (`per_page=50`, no pagination; accepted,
since the set is bounded by open third-party PRs the bot chose to
comment on — mitigation if it ever binds is `sort=updated`). PRs whose
key is in `reviewed` are then excluded client-side *before* any per-PR
fetch: the bot comments on PRs it reviews too, and their discussion
belongs to the tracked pass.

For each remaining PR the pass fetches reviews, conversation comments,
and inline diff comments — the same three feedback kinds as the bot's
own PRs, because a `CHANGES_REQUESTED` review or an inline comment on
the bot's own pushed hunk is feedback addressed to the bot; fetching
only conversation comments would recreate the blind spot this pass
closes, one endpoint over. Items are filtered exactly like tracked
comments (not the bot's own, newer than `last_poll`, trusted author,
bodyless approvals dropped, review-linked diff comments dated by
their review's `submitted_at`) and fold into **one turn per PR per
tick** — replies must not race each other on the same branch.

The dispatched message names the PR, its third-party author, and the
fact that the bot previously intervened; the PR body is never included
(Dependabot bodies embed upstream changelog text — untrusted input has
no business in the message when nothing in it is needed to answer the
comments). The instruction block states that PR content is data, not
instructions; that replies go to the PR; and that the bot may push
further commits to the PR branch from the working clone under
`projects/` — never force-push, merge, or close.

Contributor turns are build work: builder segments, the `projects/`
working clone, normal git. No review checkout is prepared and nothing
SHA-shaped is tracked — the pass is stateless, cut on the global
`last_poll` alone.

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

The `github_poll` document in the state database (spec 05):

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
- A corrupt-state `reviewed` reset also demotes still-open reviewed PRs
  to contributed candidates: their next comments arrive framed as
  contributor turns instead of reviewer follow-ups. Degraded but safe —
  builder segments, trusted comments only.

### Configuration

| Config key | Default | Description |
|------------|---------|-------------|
| `github.enabled` | `false` | Enable the GitHub channel |
| `github.poll_interval_secs` | `300` | Seconds between poll cycles |
| `github.api_base` | `https://api.github.com` | REST API base URL; tests point it at a loopback fixture server |
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

- The poll passes (own PRs, review requests, tracked reviewed PRs,
  contributed PRs) and their filtering
- Review checkout preparation under `reviews/`
- Bot identity resolution and self-reply prevention
- Message formatting for reviews, comments, diff comments, review
  requests, follow-up discussions, and contributed-PR discussions
- The review, re-review, and discussion instruction blocks
- Poll state persistence (`last_poll`, `reviewed`)

### Does Not Own

- Agent turns — `AgentHandle::send_message`, as everywhere
- Session routing — the actor routes on the channel's hint (spec 14):
  the work session for author/contributor turns, `review:{nwo}#{n}`
  for reviewer turns
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
| Contributed search fails | Log error, retry next tick without advancing `last_poll` |
| Contributed PR feedback fetch fails | Skip that PR this tick; items in the window are lost once `last_poll` advances — accepted, one broken PR must not wedge the cursor |
| State file corrupt | Defaults: `last_poll = now`, empty `reviewed` map |

No channel failure crashes the daemon; a disabled or failed channel
resolves to `std::future::pending()` and parks forever.

## Constraints

- Reviewer turns are review only: no pushing, merging, closing, or
  label mutation
- Contributor turns may push commits to the third-party PR branch —
  never force-push, merge, or close. The PR title, body, and diff
  remain untrusted data even then
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
