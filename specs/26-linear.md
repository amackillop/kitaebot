# Spec 26: Linear Channel

## Motivation

Polls Linear for issues assigned to the bot's Linear user. An assigned issue
becomes a work item: the bot posts an implementation plan as a comment, and
a trusted user's comment approving the plan triggers end-to-end execution
(branch, implement, test, push, PR). The bot moves tickets between workflow
states with the `linear_set_state` tool when the workflow has matching
states; the ticket id in the branch name additionally links the PR to the
issue.

## Behavior

**Poll loop**: `tokio::time::interval` with `MissedTickBehavior::Skip`. Each
tick:

1. Fetch the viewer's assigned issues via GraphQL (states of type
   `completed`/`canceled` filtered server-side), with description, labels,
   and comments
2. Compute events (pure core) against the persisted poll state
3. Dispatch each event through the agent handle with
   `ChannelSource::Linear { issue }` and the issue's `owner/repo` label as
   session hint — the same session key GitHub uses, so a repo's PRs and
   tickets share one session
4. Post the agent's reply as a comment on the issue
5. Save the poll state

**Bot identity**: resolved once at loop start via the `viewer` query.
Comments authored by the viewer are skipped to prevent self-reply loops.

**Events**:

- *New issue* — an assigned issue whose identifier is not in the announced
  set. The message carries identifier, title, target repo, description, and
  pre-existing comments, plus an instruction to produce a review-ready
  markdown plan and not implement anything yet. Issues announced in a tick
  skip the comment pass for that tick (no double dispatch).
- *New comment* — `createdAt > last_poll`, not authored by the viewer, from
  a trusted user. The message carries identifier, repo, author, and body,
  plus an instruction: if the comment approves the plan, execute
  end-to-end — clone the repo, create a branch named
  `kitaebot_<ticket-id>_<summary>` (the ticket id links the PR to the issue
  automatically), implement, test, commit, push, and open a PR. On success
  the reply is one line at most (the PR attaches itself to the ticket);
  detail is reserved for failures or open decisions. Otherwise treat it as
  feedback and reply with a revised plan.

**The plan label chooses the choreography**, exactly as on the GitHub
issues channel ([spec 25](25-github-issues.md)): an issue carrying
`linear.plan_label` (default `needs-plan`, case-insensitive) gets
plan-first; without it the announcement is a direct execution turn,
with the same judgment backstop. The label is read at announcement
time.

**Repo selection**: the target repository comes from a label on the issue —
a label named like `owner/repo` (contains exactly one `/`). Issues without
such a label, or with more than one, are logged and skipped entirely: not
announced, not added to state, so they are picked up on a later tick once
the labels are fixed.

**Access control**: `linear.trusted_users` is a list of email addresses,
matched case-insensitively against the comment author. Comments with no
author (e.g. integrations) are untrusted. The bot's own email must not be
listed.

**State persistence**: the `linear_poll` document in the state
database (spec 05):

```json
{"last_poll": "2026-07-05T12:00:00Z", "announced_issues": ["MDK-123"]}
```

Missing or corrupt state defaults to `last_poll = now` with an empty
announced set — assigned issues are announced fresh, old comments are not
replayed. Announced identifiers absent from the fetched set (completed,
cancelled, or unassigned) are pruned. `last_poll` only advances after a
successful fetch.

**API**: GraphQL over HTTPS POST to `{linear.api_base}/graphql`,
authorized with a personal API key in the `Authorization` header (no
`Bearer` prefix). Three operations: `viewer`, `assignedIssues`, and
`commentCreate`. Raw query strings, no GraphQL client library. No
pagination: first 50 assigned issues, first 100 comments per issue.

**Send retries**: comment posting retries up to 3 times with exponential
backoff (1s, 2s, 4s) for transient errors.

**Activity events**: not forwarded (passes `None` for activity sender).

### Configuration

| Config key | Default | Description |
|------------|---------|-------------|
| `linear.enabled` | `false` | Enable the Linear channel |
| `linear.poll_interval_secs` | `120` | Seconds between poll cycles |
| `linear.trusted_users` | `[]` | Trusted email addresses (required when enabled) |
| `linear.api_base` | `https://api.linear.app` | API base URL; tests point it at a loopback fixture server |

Requires the `linear-api-key` secret.

## Boundaries

### Owns

- The Linear poll loop, event detection, and announcement/execution
  message formatting
- Repo selection from `owner/repo` labels
- The plan-label choreography
- Poll state persistence (`linear_poll`)
- Comment posting with retries

### Does Not Own

- Agent turns — `AgentHandle::send_message`, as everywhere
- Workflow state moves — the `linear_set_state` tool (spec 03)
- Execution checkouts — shared `execution_checkout` logic
- Session persistence — the context engine (spec 14)

## Failure Modes

| Error | Behavior |
|-------|----------|
| Viewer resolution fails | Log error, park forever (no polling) |
| Issue fetch fails | Log error, retry next tick without advancing `last_poll` |
| Agent turn fails | Post the error text as a comment |
| Comment post fails after retries | Log error, continue with remaining events |
| No/ambiguous repo label | Log warning, skip issue until labels are fixed |

No channel failure crashes the daemon; a disabled or failed channel
resolves to `std::future::pending()` and parks forever.

## Constraints

Shared channel constraints ([spec 10](10-channels.md)): text only, no
message queuing, trusted users only.
