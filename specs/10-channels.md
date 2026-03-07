# Channels

## Purpose

A channel is a frontend that translates external messages into agent turns and delivers responses back. Telegram and the Unix socket are the two external channels. The REPL (`kitaebot chat`) is a local debug channel.

## Why Channels?

The agent core (provider, tools, session, workspace) is interface-agnostic. A channel is the glue between an external messaging platform and this core. Each channel parses input into one of two paths — messages go through `agent::process_message()` (LLM agent loop), slash commands go through `commands::execute()` (local operations). Both handle their own session lifecycle (load, execute, save).

Separating channels from the core means:

1. **Multiple interfaces** — Telegram, Unix socket, REPL — all drive the same agent
2. **Shared memory, isolated conversations** — Each channel has its own session but reads/writes the same workspace and long-term memory
3. **Independent lifecycles** — Adding a channel doesn't change the agent loop, provider, or tools

## Architecture

```
              ┌──────────────────────────────────────────┐
              │             kitaebot run  (daemon)       │
              │                                          │
              │  ┌──────────┐ ┌──────────┐ ┌───────────┐ │
              │  │ Telegram │ │  Socket  │ │ Heartbeat │ │
              │  │  poller  │ │ listener │ │   timer   │ │
              │  └─────┬────┘ └─────┬────┘ └─────┬─────┘ │
              │        │            │            │       │
              │        ▼            ▼            ▼       │
              │  ┌──────────────────────────────────┐    │
              │  │     agent::process_message()     │    │
              │  └───────────────┬──────────────────┘    │
              │                  │                       │
              │   ┌──────────────▼───────────────┐       │
              │   │      Provider / Tools        │       │
              │   └──────────────────────────────┘       │
              └──────────────────────────────────────────┘

              ┌─────────────────────────────────┐
              │       kitaebot chat             │
              │                                 │
              │  ┌─────────┐                    │
              │  │  REPL   │                    │
              │  └────┬────┘                    │
              │       │                         │
              │       ▼                         │
              │  ┌──────────────────────────┐   │
              │  │ agent::process_message() │   │
              │  └──────────┬───────────────┘   │
              │             │                   │
              │  ┌──────────▼───────────┐       │
              │  │ Provider / Tools     │       │
              │  └──────────────────────┘       │
              └─────────────────────────────────┘

Shared:
  ~/.local/share/kitaebot/
  ├── sessions/
  │   ├── telegram.json      # Telegram channel session
  │   ├── socket.json        # Unix socket channel session
  │   ├── heartbeat.json     # Heartbeat session
  │   └── repl.json          # REPL session
  └── memory/                # Long-term memory (all channels)
```

The daemon (`kitaebot run`) and the REPL (`kitaebot chat`) are separate processes. They share the workspace filesystem and coordinate via per-channel file locks. No IPC protocol between them.

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
│              │  3. Call process_message(message)
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
| `error`          | Invalid request or agent failure  |

All responses carry a `content: String` field. Embedded newlines are
JSON-escaped (`\n` in the string, not literal on the wire).

### Session

Persistent at `sessions/socket.json`. Same lifecycle as Telegram:
loaded per-message, saved after each turn, cleared via `/new`.

### Concurrency

Single client at a time. A second connection receives an error and is
closed immediately.

### Connection Lifecycle

1. Client connects → daemon rejects if another client is connected
2. Daemon sends `greeting`
3. Client sends messages/commands, daemon responds
4. Client disconnects (EOF) → daemon resumes accepting

No keepalives, no timeouts.

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
| Session load/save fails     | Log, error/response respectively       |
| Client disconnects mid-turn | Complete turn, save session, discard response |

## Channel as Pattern, Not Trait

Each channel follows the same shape:

1. Wait for input (poll Telegram, accept on socket, read stdin, timer tick)
2. Parse input into message or slash command
3. Dispatch: `agent::process_message()` for messages, `commands::execute()` for slash commands
4. Route the `Result` to the transport (send Telegram message, write to socket, print to stdout)

Session load/save is handled inside `process_message` and `commands::execute` — channels never manage session state directly.

There is no `Channel` trait. Each channel module implements this pattern directly. The specifics vary enough (HTTP polling vs NDJSON stream vs stdio) that a shared trait would be either too thin to enforce anything useful or too leaky to accommodate real differences.

## Per-Channel Locking

Lock files prevent concurrent access where multiple OS processes could collide:

| Lock | Holder |
|------|--------|
| `locks/repl.lock` | REPL process (`kitaebot chat`) |
| `locks/heartbeat.lock` | Heartbeat invocation (`kitaebot heartbeat` / systemd timer) |

Telegram needs no lock — the poller is a single sequential loop inside the daemon process. The loop itself serializes message processing. Messages arriving during a turn are queued by Telegram's `getUpdates` offset mechanism and picked up on the next poll.

The socket channel needs no lock — it enforces single-client at the listener level.

Different channels can run turns concurrently — the provider is stateless (full context sent each call) and sessions are isolated.

## Session Isolation vs Shared Memory

| Layer | Scope | Purpose |
|-------|-------|---------|
| `sessions/<channel>.json` | Per-channel | Conversational context for this channel |
| `memory/` | Shared | Long-term knowledge, learnings, history |
| `HEARTBEAT.md` | Shared | Heartbeat task definitions |
| Workspace files | Shared | Projects, SOUL.md, config |

The agent sees different conversational history depending on which channel it's serving, but its long-term memory and workspace are unified. A learning from a Telegram conversation (written to `memory/`) is visible during the next heartbeat or REPL session.

## Future Channels

Not in scope now, but the pattern accommodates:

- **Discord** — Bot API, similar long-poll or gateway websocket model
- **Matrix** — Synapse client API, sync endpoint for polling
- **Email** — IMAP polling, SMTP replies
- **HTTP API** — REST endpoint for programmatic access

Each would be a new module under `src/channels/`, following the same pattern.

## Simplifications

1. **Text only** — No images, documents, or media
2. **Single user** — One authorized Telegram chat_id; single socket client
3. **No message queuing** — Process one message at a time, Telegram buffers the rest
4. **No typing indicators** — Agent appears offline until response is ready
5. **No message splitting** — Long responses sent as a single message (Telegram's 4096 char limit may need handling later)
