# Spec 10: Channels

## Motivation

A channel translates external messages into agent turns and delivers responses
back. Four channels drive the same agent core: Telegram (phone), Unix socket
(computer), GitHub PR polling (code review), and heartbeat (periodic timer).

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

#### Connection Lifecycle

1. Client connects → daemon rejects if another client is connected
2. Daemon sends `greeting`
3. Client sends messages, daemon responds
4. Client disconnects (EOF) → daemon resumes accepting

No keepalives, no timeouts.

#### Verbose Mode

`/verbose` toggles activity event forwarding for the current connection.
Intercepted before dispatch — it is UI state, not a slash command. Resets
on disconnect.

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

#### Error Handling

| Error | Behavior |
|-------|----------|
| Socket dir missing | Log info, park forever (daemon continues without socket) |
| Socket bind fails | Log error, park forever (daemon continues without socket) |
| Accept fails | Log, continue accepting |
| Invalid JSON from client | Error response, keep connection |
| Agent turn fails | Error response, keep connection |

---

### GitHub

Polls for new activity on the bot's own open pull requests.

**Poll loop**: `tokio::time::interval` with `MissedTickBehavior::Skip`. Each
tick:

1. List bot's open PRs via `gh search prs --author=@me --state=open`
2. For each PR: fetch reviews, comments, and inline diff comments
3. Filter: skip bot's own comments, skip items older than `last_poll`, skip
   untrusted users
4. Send each new item through the agent handle with
   `ChannelSource::GitHub { pr_number }`
5. Update `last_poll` timestamp

**Bot identity**: resolved on startup via `gh api user`. All comments from
this user are skipped to prevent self-reply loops.

**Access control**: the bot owner (from `github.owner`) is always trusted.
Additional users can be granted access via `trusted_users`. Both are
case-insensitive. Messages from anyone else are logged and skipped.

**Message format**:

- Review: `Review on PR #5 "Title" (owner/repo) by @alice: APPROVED\n\nBody`
- Comment: `Comment on PR #5 "Title" (owner/repo) by @carol:\n\nBody`
- Diff comment: `Inline comment on PR #5 "Title" (owner/repo) by @dave at src/main.rs:42:\n\nBody`

**State persistence**: `memory/github_poll_state.json` via atomic write.
Missing or corrupt state defaults to "now" to avoid replaying entire PR
histories.

**Activity events**: not forwarded (passes `None` for activity sender).

| Config key | Default | Description |
|------------|---------|-------------|
| `github.enabled` | `false` | Enable the GitHub channel |
| `github.poll_interval_secs` | `300` | Seconds between poll cycles |
| `github.owner` | — | Bot owner's GitHub username (required when enabled) |
| `github.trusted_users` | `[]` | Additional trusted GitHub usernames |

Requires the `github-token` secret.

#### Error Handling

| Error | Behavior |
|-------|----------|
| Bot login resolution fails | Log error, park forever (no polling) |
| PR list/fetch fails | Log error, retry next tick |
| Individual message send fails | Log error, continue with remaining items |

---

### Heartbeat

Documented separately in [spec 07](07-heartbeat.md).

## Boundaries

### Owns

- Transport-specific polling/listening logic
- Message formatting for each platform
- Access control per channel
- Verbose mode (socket and Telegram)
- Send retries (Telegram)
- State persistence (GitHub poll cursor)

### Does Not Own

- Agent turns — delegates to `AgentHandle::send_message()`
- Input classification (message vs command) — the actor handles that
- Session persistence — the session module handles that
- Activity event types — the activity module defines those

## Failure Modes

Channels are designed to be resilient. No channel failure crashes the daemon.
Disabled or failed channels resolve to `std::future::pending()` and park
forever.

## Constraints

- Text only — no images, documents, or media
- Single authorized user per channel (one Telegram chat_id, one socket client,
  trusted GitHub users)
- No message queuing — the actor processes one envelope at a time
- No typing indicators — the agent appears offline until the response is ready
  (activity events provide partial progress when verbose is on)

## Open Questions

- `telegram.poll_timeout_secs` is not wired to the `getUpdates` timeout
  parameter (hardcoded to 30). The config field only affects the HTTP client
  timeout. Should it be connected or removed?
