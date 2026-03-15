# Channels

## Purpose

A channel is a frontend that translates external messages into agent turns and delivers responses back. Telegram, the Unix socket, GitHub PR polling, and the heartbeat timer are the four channels.

## Why Channels?

The agent core (provider, tools, session, workspace) is interface-agnostic. A channel is the glue between an external messaging platform and this core. Each channel sends messages through `AgentHandle::send_message()` and awaits a reply. The actor classifies input as either a message (→ agent turn) or a slash command (→ `commands::execute()`).

Separating channels from the core means:

1. **Multiple interfaces** — Telegram, Unix socket, GitHub, and heartbeat drive the same agent
2. **Unified session** — All channels share a single session, with messages tagged by `ChannelSource`
3. **Independent lifecycles** — Adding a channel doesn't change the agent loop, provider, or tools
4. **No locking** — The actor processes envelopes sequentially

## Architecture

```
              ┌──────────────────────────────────────────┐
              │             kitaebot run  (daemon)       │
              │                                          │
              │  ┌──────────┐ ┌──────────┐ ┌──────────┐  │
              │  │ Telegram │ │  Socket  │ │  GitHub  │  │
              │  │  poller  │ │ listener │ │ PR poll  │  │
              │  └─────┬────┘ └─────┬────┘ └─────┬────┘  │
              │        │            │            │       │
              │  ┌─────┘   ┌────────┘   ┌────────┘       │
              │  │         │            │  ┌───────────┐ │
              │  │         │            │  │ Heartbeat │ │
              │  │         │            │  │   timer   │ │
              │  │         │            │  └─────┬─────┘ │
              │  │         │            │        │       │
              │  ▼         ▼            ▼        ▼       │
              │  ┌──────────────────────────────────┐    │
              │  │       AgentHandle (cloneable)    │    │
              │  └───────────────┬──────────────────┘    │
              │                  │ mpsc                  │
              │                  ▼                       │
              │  ┌──────────────────────────────────┐    │
              │  │     Agent actor (sequential)     │    │
              │  │     └─ unified session.json      │    │
              │  └──────────────────────────────────┘    │
              └──────────────────────────────────────────┘

Workspace:
  ~/.local/share/kitaebot/
  ├── session.json          # Unified session (all channels)
  └── memory/               # Long-term memory (all channels)
```

All channels run inside the daemon process. Interactive access from the host is through `kchat` connecting to the Unix socket.

## Telegram Channel

### Why Long-Polling?

Telegram offers two modes: webhooks (Telegram pushes to your HTTPS endpoint) and long-polling (`getUpdates` — you pull from Telegram).

Long-polling is the right choice:

1. **No public endpoint** — Works behind NAT, inside VMs, no TLS cert needed
2. **Simple** — One HTTP call in a loop, no web server
3. **Reliable** — No missed messages from webhook delivery failures
4. **Stateless** — Just track the last `update_id` offset

### Message Flow

```
Telegram servers
       │
       │  getUpdates (long-poll)
       ▼
┌──────────────┐
│   Telegram   │  1. Receive update
│   poller     │  2. Extract message text + chat_id
│              │  3. Send through AgentHandle
│              │  4. Send response via sendMessage API
└──────────────┘
```

### Bot API

Only a minimal subset of the Telegram Bot API is needed:

| Method | Purpose |
|--------|---------|
| `getUpdates` | Long-poll for new messages |
| `sendMessage` | Reply to user |

Both are simple HTTPS POST calls to `https://api.telegram.org/bot<token>/<method>`. No Telegram client library needed — `reqwest` + `serde` suffices.

### Configuration

The bot token is a secret loaded via systemd `LoadCredential` at startup (see [spec 13](13-credentials.md)). Non-secret settings live in `config.toml`:

| Key | Default | Purpose |
|-----|---------|---------|
| `telegram.enabled` | `false` | Enable the Telegram channel |
| `telegram.chat_id` | — | Authorized chat ID (required when enabled) |
| `telegram.poll_timeout_secs` | `30` | Long-poll timeout for `getUpdates` |

### Access Control

The bot only responds to a single authorized `chat_id`. Messages from other chats are silently ignored.

## Socket Channel

A Unix domain socket at `/run/kitaebot/chat.sock` providing an interactive
chat channel from the host machine. Telegram is for the phone; the socket
is for the computer.

### Protocol

Newline-delimited JSON (NDJSON). One JSON object per `\n`-terminated line.

#### Client → Daemon

| Type      | Fields           | Behavior               |
|-----------|------------------|------------------------|
| `message` | `content: String`| Send to agent          |
| `command` | `name: String`   | Execute slash command  |

Unknown types or command names → error response. Connection stays open.

#### Daemon → Client

| Type             | When                              |
|------------------|-----------------------------------|
| `greeting`       | Immediately on connect            |
| `response`       | Agent turn completed              |
| `command_result` | Slash command completed           |
| `activity`       | During turn, when verbose is on   |
| `error`          | Invalid request or agent failure  |

All responses carry a `content: String` field. Embedded newlines are
JSON-escaped (`\n` in the string, not literal on the wire).

#### Activity Events

When verbose mode is on, the server sends `activity` messages during a turn. These are human-readable strings describing what the agent is doing (tool calls, compaction). See [spec 16](16-activity.md) for the full event type.

The client should display activity messages as transient progress — they are not part of the conversation. `kchat` prints them to stderr with a dim prefix.

### Concurrency

Single client at a time. A second connection receives an error and is
closed immediately.

### Connection Lifecycle

1. Client connects → daemon rejects if another client is connected
2. Daemon sends `greeting`
3. Client sends messages/commands, daemon responds
4. Client disconnects (EOF) → daemon resumes accepting

No keepalives, no timeouts.

### Verbose Mode

`/verbose` toggles activity event forwarding for the current connection. It is intercepted before dispatch — it is UI state, not a slash command. The toggle resets on disconnect.

When verbose is on, the server sends `ServerMsg::Activity` for each event emitted by the agent during a turn. When off, events are silently discarded.

### Client Binary

`kchat <socket-path>` — a dumb REPL that wraps stdin lines into the
NDJSON protocol and prints response `content`. Lines starting with `/`
are sent as commands; everything else as messages.

### Error Handling

| Error                       | Behavior                               |
|-----------------------------|----------------------------------------|
| Socket bind fails           | Fatal — exit                           |
| Accept fails                | Log, continue accepting                |
| Invalid JSON from client    | Error response, keep connection        |
| Agent turn fails            | Error response, keep connection        |
| Client disconnects mid-turn | Complete turn, save session, discard response |

### Telegram Verbose Mode

`/verbose` toggles activity event forwarding within a polling session. It is intercepted before dispatch. The toggle resets on daemon restart.

When verbose is on, activity events are sent as separate Telegram messages (fire-and-forget, errors logged). When off, events are discarded.

## GitHub Channel

The GitHub channel polls for new activity on the bot's own open pull requests. It is the code-review interface — reviewers leave comments, and the agent responds in the context of the full unified session.

### Poll Loop

```
┌──────────────────┐
│  GitHub poller   │  1. Resolve bot login (gh api user)
│                  │  2. gh search prs --author=@me --state=open
│                  │  3. For each PR:
│                  │     a. Fetch reviews + comments (gh pr view)
│                  │     b. Fetch inline diff comments (gh api)
│                  │     c. Filter: skip bot's own, skip older than last_poll
│                  │     d. Send each new item through AgentHandle
│                  │  4. Update last_poll timestamp
│                  │  5. Sleep until next tick
└──────────────────┘
```

### Bot Identity

On startup, the poller resolves the bot's GitHub username via `gh api user`. All subsequent reviews/comments from this user are skipped to avoid infinite self-reply loops.

### What Gets Polled

For each of the bot's open PRs (across all repos):

| Item | Source | Filtered by |
|------|--------|-------------|
| Reviews | `gh pr view --json reviews` | `submitted_at > last_poll`, not by bot |
| PR comments | `gh pr view --json comments` | `created_at > last_poll`, not by bot |
| Inline diff comments | `gh api repos/{nwo}/pulls/{n}/comments` | `created_at > last_poll`, not by bot |

### Message Format

Each item is formatted as a human-readable message and sent with `ChannelSource::GitHub { pr_number }`:

- **Review**: `Review on PR #5 "Title" (owner/repo) by @alice: APPROVED\n\nLooks good!`
- **Comment**: `Comment on PR #5 "Title" (owner/repo) by @carol:\n\nWhat about edge cases?`
- **Diff comment**: `Inline comment on PR #5 "Title" (owner/repo) by @dave at src/main.rs:42:\n\nNit: rename this`

### State Persistence

Poll state (`last_poll` timestamp) is persisted at `memory/github_poll_state.json` via atomic write (tmp + rename). On first boot or missing state, `last_poll` defaults to "now" to avoid replaying entire PR histories.

### Configuration

| Key | Default | Purpose |
|-----|---------|---------|
| `github.enabled` | `false` | Enable the GitHub channel |
| `github.poll_interval_secs` | `300` | Seconds between poll cycles (5 minutes) |

Requires the `github-token` secret.

### Error Handling

| Error | Behavior |
|-------|----------|
| Bot login resolution fails | Log error, park the loop forever (no polling) |
| PR list/fetch fails | Log error, retry next tick |
| Individual message send fails | Log error, continue with remaining items |

## Channel as Pattern, Not Trait

Each channel follows the same shape:

1. Wait for input (poll Telegram, accept on socket, poll GitHub, timer tick)
2. Construct a message string
3. Send through `AgentHandle::send_message()` with appropriate `ChannelSource`
4. Handle the reply (send Telegram message, write to socket, log)

There is no `Channel` trait. Each channel module implements this pattern directly. The specifics vary enough (HTTP polling vs NDJSON stream vs GitHub API) that a shared trait would be either too thin to enforce anything useful or too leaky to accommodate real differences.

## Simplifications

1. **Text only** — No images, documents, or media
2. **Single user** — One authorized Telegram chat_id; single socket client
3. **No message queuing** — Process one message at a time via the actor
4. **No typing indicators** — Agent appears offline until response is ready (activity events provide partial progress when verbose is on)
5. **No message splitting** — Long responses sent as a single message (Telegram's 4096 char limit may need handling later)

## Future Channels

Not in scope now, but the pattern accommodates:

- **Discord** — Bot API, similar long-poll or gateway websocket model
- **Matrix** — Synapse client API, sync endpoint for polling
- **Email** — IMAP polling, SMTP replies
- **HTTP API** — REST endpoint for programmatic access

Each would be a new module following the same pattern.
