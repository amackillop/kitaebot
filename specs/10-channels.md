# Spec 10: Channels

## Motivation

A channel translates external messages into agent turns and delivers responses
back. Five channels drive the same agent core: Telegram (phone), Unix socket
(computer), GitHub PR polling (code review), and two ticket surfaces — GitHub
issue polling and Linear issue polling. The duty scheduler
([spec 24](24-self-directed-work.md)) drives periodic turns through the same
actor but is not a transport channel.

This spec owns the shared channel contract and the two small channels
(Telegram, socket). Channels with their own spec appear here as
pointers: GitHub PRs ([spec 20](20-github.md)), GitHub issues
([spec 25](25-github-issues.md)), Linear ([spec 26](26-linear.md)).

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
retry. Transient API errors (429, 5xx) use exponential backoff (1s, 2s, 4s,
capped at 60s) and respect Telegram's `retry_after` when present. Other API
errors are logged and the loop continues immediately.

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

Documented separately in [spec 25](25-github-issues.md). Polls for open
issues assigned to the bot account; assignment is the human gate.

---

### Linear

Documented separately in [spec 26](26-linear.md). Polls Linear for
issues assigned to the bot's Linear user; same plan-then-execute
choreography as GitHub issues.

---

## Boundaries

### Owns

- Transport-specific polling/listening logic for Telegram and socket
- Message formatting for those platforms
- Access control per channel (chat_id, socket peer uids)
- Verbose mode (socket and Telegram)
- Send retries (Telegram)
- The shared channel constraints below; the GitHub PR, GitHub issues,
  and Linear channels own their own behavior (specs 20, 25, 26)

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
