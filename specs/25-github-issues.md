# Spec 25: GitHub Issues Channel

## Motivation

Polls for open issues assigned to the bot account. Same choreography as
Linear ([spec 26](26-linear.md)): an assigned issue becomes a work item,
the bot posts a plan brief as a comment (format contract:
`src/prompts/plan-format.md` — decision-led, risk-acceptance explicit,
no commit-by-commit script), and a trusted user's comment approving the
plan triggers end-to-end execution (branch, implement, test, push, PR).
Shares the PR channel's REST client, identity, and trust model
([spec 20](20-github.md)); runs as its own daemon loop with its own
poll state.

**Assignment is the human gate for work.** An issue nobody assigned to
the bot dispatches no execution — including issues the bot filed itself
via `github_issue_create` (the spec 24 proposal path). A human triages a
proposal by assigning it; the channel then picks it up like any other
ticket. The bot's own comments are skipped by login, so it never
replies to itself.

**Discussion is comment-gated.** A second pass surfaces unassigned
issues for reply-only turns, so a human can nudge a proposal's
direction *before* assigning the hard work, and can talk to the bot on
tickets it will never be assigned. Three cursor-bounded searches:
bot-authored open issues (no mention needed — the bot filed it, a
trusted comment on it is addressed to the bot), bot-authored recently
closed issues (comments there are disposition on finished work; the
prompt says so and forbids reopening), and open issues where the bot
is mentioned (`mentions:{bot}`; once mentioned, the issue stays in the
search, so follow-up trusted comments dispatch without re-tagging).
The trigger is always a new trusted comment past the cursor — matching
the search alone dispatches nothing. Discussion turns prepare no
checkout, record no plan id, and instruct the model to reply as a peer
and not start work; if a discussion settles a direction, the later
assignment's announcement embeds the thread, so the work turn starts
from the settled direction. The first discussion turn on an issue
embeds title, body, and the trusted thread (tracked in the
`discussion_announced` set); later comments dispatch incrementally.
When a race surfaces an issue in both passes on one tick, the work
view wins. Unconfigured-repo hits are only warned about on the work
pass: a mention in a repo the bot does not manage is routine, not
evidence for self-analysis.

## Behavior

**Poll loop**: `tokio::time::interval` with `MissedTickBehavior::Skip`.
Each tick:

1. `GET /search/issues` with `is:issue is:open assignee:{bot}`, then
   the three discussion searches (`author:{bot}` open and closed,
   `mentions:{bot}`), each bounded by `updated:>{last_poll}` and
   deduplicated first-search-wins
2. Skip issues whose repo is not a `[git.repositories]` key (the repo
   is read from the issue itself — no label convention needed)
3. Fetch conversation comments, but only for unannounced issues (the
   announcement embeds them) and issues whose `updated_at` passed the
   cursor — untouched issues cost no extra request
4. Compute events (pure core) against the persisted poll state
5. Dispatch through the agent handle with
   `ChannelSource::GitHubIssue { issue: "owner/repo#42" }` and the
   repo as session hint — the same session key the PR and Linear
   channels use
6. Post the agent's reply as a comment on the issue
7. Save the poll state

**Events** mirror Linear's: *new issue* (not in the announced set)
dispatches an announcement carrying title, body, and existing comments
from trusted users (and the bot's own plan posts); untrusted comments
are filtered out at the same trust boundary as the post-assignment
comment pass; *new comment* (`created_at > last_poll`, not the bot,
from a trusted login) dispatches an execution/revision turn.

**The plan label chooses the choreography.** An issue assigned with
the `github.issues.plan_label` label (default `needs-plan`, matched
case-insensitively) gets the plan-first flow: plan comment, human
approval, then execution. Without it the announcement is a direct
execution turn — the human made both gestures (assign + label) at
triage, so an unlabeled assignment means "just do it"; review gates
and PR review still stand behind the result. The prompt keeps two
judgment backstops: a ticket that turns out underspecified or larger
than it reads gets a plan or questions instead of code, and a plan
containing a fork that no in-repo precedent settles — where reasonable
designs genuinely disagree — is posted for sign-off even though the
dispatch waived it (the waiver covers settled ground only; the bot can
raise its own hand). The label is read at announcement time; adding it
later changes nothing.

**Plan turns think on the planner model.** A plan announcement
dispatches with the planner turn role, served by
`model_overrides.planner` when set (spec 02); execution, discussion,
and post-plan comment turns — revisions included — ride the default.
This is the plan/execute split: the thinking phase gets the strong
model, and a strong reviewed plan is what lets the default be cheap.
Revisions run on the default deliberately; a revised plan still passes
the plan gate, and the channel cannot tell approval from feedback
before dispatch. Unset, every turn routes identically. The plan
format ends with an executor-facing "Implementation notes" section —
files, verify commands, ordering constraints, landmines — because the
executing model may be weaker than the planning one and must not be
left re-deriving what the planner already knew; reviewers can stop
reading above that line, and the plan gate reviews it with the rest.

**The plan rides the dispatch.** When the recorded plan comment is in
the tick's fetched thread, post-plan dispatches embed its body
verbatim. Session history is not relied on: after compaction the
assembled prompt holds a summary of the plan, not the plan (LCM keeps
the bytes but recovery is a tool round the model must initiate; flat
summarizes lossily), and the execution turn — the cheap-default half
of the plan/execute split — must see the gate-reviewed text
unconditionally. A plan past the fetch cap or deleted degrades to the
id reference alone.

**Plan revisions edit in place.** The channel records the announcement
reply's comment id (that comment is the plan) and hands it to
revision turns, which are instructed to engage with feedback as a
peer: update the plan comment via `github_comment_update` where
persuaded — GitHub's edit history shows the reviewer the diff — and
push back with reasons where not, rather than complying with changes
the bot believes are wrong. The reply comment doubles as the
notification, since GitHub sends none for edits. Ids are pruned with
their issues; state predating the field falls back to
reply-with-full-plan. Execution turns
get a fresh base checkout prepared at `projects/<owner>/<repo>`
(shared `execution_checkout` logic). The reset preserves before it
destroys: a predecessor turn's uncommitted changes or stranded
detached-HEAD commits are parked on a `kitaebot_recovered/<epoch>`
branch, named in the turn's ready note, before the checkout is
force-detached and cleaned. The branch convention is
`kitaebot_issue-{n}_<summary>` and the PR description carries
`Closes #{n}`, so merging the PR closes the ticket — GitHub issues
have no workflow states to move, and no state tool exists.

**Access control**: reuses the PR channel's trust model — the owner is
always trusted, plus `github.trusted_users` and `github.trusted_bots`
(spec 20). Matched against comment author logins.

**State persistence**: the `github_issues_poll` document in the state
database, same shape as Linear's — `last_poll` cursor plus the
announced set, keyed `owner/repo#42`. Missing or corrupt state starts
from now. Announced keys absent from a fetch (closed, unassigned) are
pruned. The `discussion_announced` set follows the same lifetime: it
persists while the issue stays in a discussion search's view and
prunes when it leaves (a pruned closed issue that draws another
comment simply re-embeds — rare and harmless, and the state stays
bounded).

**Send retries**: comment posting retries up to 3 times with
exponential backoff (1s, 2s, 4s) on network errors, 429, and 5xx.

### Configuration

| Config key | Default | Description |
|------------|---------|-------------|
| `github.issues.enabled` | `false` | Enable issue polling (requires `github.enabled`) |
| `github.issues.poll_interval_secs` | `300` | Seconds between poll cycles |

Requires `github.enabled = true` — that is what builds the REST client
and loads the `github-token` secret.

## Boundaries

### Owns

- The issue poll loop, event detection, and announcement/revision
  message formatting
- The plan-label choreography and plan-comment id tracking
- Poll state persistence (`github_issues_poll`)
- Comment posting with retries

### Does Not Own

- Agent turns — `AgentHandle::send_message`, as everywhere
- The trust model and bot identity — shared with the PR channel
  (spec 20, `channel/github/trust.rs`)
- Execution checkouts — shared `execution_checkout` logic
- Issue creation — the `github_issue_create` tool (spec 03); this
  channel only consumes assignments

## Failure Modes

| Error | Behavior |
|-------|----------|
| Bot login resolution fails | Log error, park forever (no polling) |
| Search or comment fetch fails | Log error, retry next tick without advancing `last_poll` |
| Agent turn fails | Post the error text as a comment |
| Comment post fails after retries | Log error, continue with remaining events |
| Repo not configured | Log warning, skip issue until it is |

No channel failure crashes the daemon; a disabled or failed channel
resolves to `std::future::pending()` and parks forever.

## Constraints

Shared channel constraints ([spec 10](10-channels.md)): text only, no
message queuing, trusted users only.
