# Spec 10: Channels

## Motivation

A channel translates external messages into agent turns and delivers responses
back. Five channels drive the same agent core: Telegram (phone), Unix socket
(computer), GitHub PR polling (code review), and two ticket surfaces — GitHub
issue polling and Linear issue polling. The duty scheduler
([spec 24](24-self-directed-work.md)) drives periodic turns through the same
actor but is not a transport channel.

Each channel sends messages through `AgentHandle::send_message()` and awaits a
reply. The actor classifies input as either a message (agent turn) or a slash
command. There is no `Channel` trait — each module implements the pattern
directly, since the transport differences (HTTP polling vs NDJSON stream vs
GitHub API) make a shared trait more leaky than useful.

## Behavior

### Telegram

Long-polls `getUpdates` from the Telegram Bot API in a loop.

**Flow**: receive update → extract message text + chat_id → send through
agent handle → send response via `sendMessage`. Only `getUpdates` and
`sendMessage` are used — no Telegram client library, just `reqwest` + `serde`.

**Access control**: only responds to a single authorized `chat_id`. Messages
from other chats are silently ignored.

**Verbose mode**: `/verbose` toggles activity event forwarding within the
polling session. When on, activity events are sent as separate Telegram
messages (fire-and-forget, errors logged). Resets on daemon restart.

**Send retries**: `sendMessage` retries up to 3 times with exponential backoff
(1s, 2s, 4s) for transient errors (network, 429, 5xx).

**Error handling**: `getUpdates` network errors trigger a 5-second sleep then
retry. Other API errors are logged and the loop continues.

**Preformatted output**: replies with `preformatted: true` are HTML-escaped
and wrapped in `<pre>` tags.

| Config key | Default | Description |
|------------|---------|-------------|
| `telegram.enabled` | `false` | Enable the Telegram channel |
| `telegram.chat_id` | — | Authorized chat ID (required when enabled) |
| `telegram.poll_timeout_secs` | `30` | Long-poll timeout for `getUpdates` |
| `telegram.api_base` | `https://api.telegram.org` | Bot API base URL; tests point it at a loopback fixture server |

---

### Socket

A Unix domain socket providing an interactive chat channel from the host.

#### Protocol

Newline-delimited JSON (NDJSON). One JSON object per `\n`-terminated line.

**Client → Daemon**: a single flat object:

```json
{"content": "hello"}
{"content": "/new"}
```

No type tag on the client side. Slash commands are parsed server-side from
the `content` field by `Input::parse()`.

**Daemon → Client**: tagged objects with `type` discriminator:

| Type | When |
|------|------|
| `greeting` | Immediately on connect (shows session status) |
| `response` | Agent turn or slash command completed |
| `activity` | During turn, when verbose is on |
| `error` | Invalid request, agent failure, or rejected connection |

All types carry a `content: String` field. Embedded newlines are JSON-escaped.

#### Concurrency

Single client at a time. While serving a client, new connections receive an
error and are closed immediately.

#### Access Control

On accept the daemon reads the peer's credentials (`SO_PEERCRED`) and serves
only uids in `socket.allowed_uids` (default `[0]` — root, the operator
reaching the VM over SSH). Any other peer receives an error and is closed.
Landlock does not mediate unix-socket connects, so this check is the only
thing keeping a same-uid exec child from driving the daemon as the operator.

#### Connection Lifecycle

1. Client connects → daemon rejects if the peer uid is not allowed, or if
   another client is connected
2. Daemon sends `greeting`
3. Client sends messages, daemon responds
4. Client disconnects (EOF) → daemon resumes accepting

No keepalives, no timeouts.

#### Verbose Mode

Activity forwarding starts **on** for each connection, so one-shot
clients (`kchat <socket> <message>`) see turn internals without a
toggle round trip. `/verbose` turns it off for the current connection.
Intercepted before dispatch — it is UI state, not a slash command.
Resets on disconnect.

#### Client Disconnect Mid-Turn

If the client disconnects while a turn is in progress, the turn is
**cancelled** via `CancellationToken`. The session is saved with whatever
partial state accumulated before cancellation. The response is discarded.

#### Client Binary

`kchat <socket-path>` — a synchronous REPL using blocking `UnixStream`.
Sends all input uniformly as `{"content": "..."}`. Activity messages are
printed to stderr with `  ~ ` prefix. Responses to stdout. `/exit` exits
locally without being sent to the server.

| Config key | Default | Description |
|------------|---------|-------------|
| `socket.path` | `/run/kitaebot/chat.sock` | Socket path |
| `socket.allowed_uids` | `[0]` | Peer uids (SO_PEERCRED) the socket serves |

#### Error Handling

| Error | Behavior |
|-------|----------|
| Socket dir missing | Log info, park forever (daemon continues without socket) |
| Socket bind fails | Log error, park forever (daemon continues without socket) |
| Accept fails | Log, continue accepting |
| Peer uid not allowed | Warn log, error response, close connection |
| Invalid JSON from client | Error response, keep connection |
| Agent turn fails | Error response, keep connection |

---

### GitHub PRs

Documented separately in [spec 20](20-github.md). Polls feedback on the
bot's own PRs and review requests for others' PRs.

---

### GitHub issues

Polls for open issues assigned to the bot account. Same choreography as
Linear: an assigned issue becomes a work item, the bot posts an
implementation plan as a comment, and a trusted user's comment approving
the plan triggers end-to-end execution (branch, implement, test, push,
PR). Shares the PR channel's REST client, identity, and trust model;
runs as its own daemon loop with its own poll state.

**Assignment is the human gate.** An issue nobody assigned to the bot
dispatches nothing — including issues the bot filed itself via
`github_issue_create` (the spec 24 proposal path). A human triages a
proposal by assigning it; the channel then picks it up like any other
ticket. The bot's own comments are skipped by login, so it never
replies to itself.

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
dispatches an announcement carrying title, body, and existing
comments; *new comment* (`created_at > last_poll`, not the bot, from a
trusted login) dispatches an execution/revision turn.

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

| Config key | Default | Description |
|------------|---------|-------------|
| `github.issues.enabled` | `false` | Enable issue polling (requires `github.enabled`) |
| `github.issues.poll_interval_secs` | `300` | Seconds between poll cycles |

Requires `github.enabled = true` — that is what builds the REST client
and loads the `github-token` secret.

#### Error Handling

| Error | Behavior |
|-------|----------|
| Bot login resolution fails | Log error, park forever (no polling) |
| Search or comment fetch fails | Log error, retry next tick without advancing `last_poll` |
| Agent turn fails | Post the error text as a comment |
| Comment post fails after retries | Log error, continue with remaining events |
| Repo not configured | Log warning, skip issue until it is |

---

### Linear

Polls Linear for issues assigned to the bot's Linear user. An assigned issue
becomes a work item: the bot posts an implementation plan as a comment, and
a trusted user's comment approving the plan triggers end-to-end execution
(branch, implement, test, push, PR). The bot moves tickets between workflow
states with the `linear_set_state` tool when the workflow has matching
states; the ticket id in the branch name additionally links the PR to the
issue.

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

| Config key | Default | Description |
|------------|---------|-------------|
| `linear.enabled` | `false` | Enable the Linear channel |
| `linear.poll_interval_secs` | `120` | Seconds between poll cycles |
| `linear.trusted_users` | `[]` | Trusted email addresses (required when enabled) |
| `linear.api_base` | `https://api.linear.app` | API base URL; tests point it at a loopback fixture server |

Requires the `linear-api-key` secret.

#### Error Handling

| Error | Behavior |
|-------|----------|
| Viewer resolution fails | Log error, park forever (no polling) |
| Issue fetch fails | Log error, retry next tick without advancing `last_poll` |
| Agent turn fails | Post the error text as a comment |
| Comment post fails after retries | Log error, continue with remaining events |
| No/ambiguous repo label | Log warning, skip issue until labels are fixed |

---

## Boundaries

### Owns

- Transport-specific polling/listening logic
- Message formatting for each platform
- Access control per channel
- Verbose mode (socket and Telegram)
- Send retries (Telegram)
- State persistence (Linear and GitHub-issues poll cursors; the PR
  channel's lives in spec 20)

### Does Not Own

- Agent turns — delegates to `AgentHandle::send_message()`
- Input classification (message vs command) — the actor handles that
- Session persistence — the context engine handles that (spec 14)
- Activity event types — the activity module defines those

## Failure Modes

Channels are designed to be resilient. No channel failure crashes the daemon.
Disabled or failed channels resolve to `std::future::pending()` and park
forever.

## Constraints

- Text only — no images, documents, or media
- Single authorized user per channel (one Telegram chat_id, one socket client,
  trusted GitHub users, trusted Linear emails)
- No message queuing — the actor processes one envelope at a time
- No typing indicators — the agent appears offline until the response is ready
  (activity events provide partial progress when verbose is on)
