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

**Assignment is the human gate.** An issue nobody assigned to the bot
dispatches nothing — including issues the bot filed itself via
`github_issue_create` (the spec 24 proposal path). A human triages a
proposal by assigning it; the channel then picks it up like any other
ticket. The bot's own comments are skipped by login, so it never
replies to itself.

## Behavior

**Poll loop**: `tokio::time::interval` with `MissedTickBehavior::Skip`.
Each tick:

1. `GET /search/issues` with `is:issue is:open assignee:{bot}`
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
and PR review still stand behind the result. The prompt keeps a
judgment backstop: a ticket that turns out underspecified or larger
than it reads gets a plan or questions instead of code. The label is
read at announcement time; adding it later changes nothing.

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
(shared `execution_checkout` logic). The branch convention is
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
pruned.

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
