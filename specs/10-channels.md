# Channels

## Purpose

A channel is a frontend that translates external messages into agent turns and delivers responses back. Telegram is the primary channel. The REPL (`kitaebot chat`) is a local debug channel.

## Why Channels?

The agent core (provider, tools, session, workspace) is interface-agnostic. It takes a `Message::User`, runs `run_turn()`, and produces a `Response::Text`. A channel is the glue between an external messaging platform and this core loop.

Separating channels from the core means:

1. **Multiple interfaces** — Telegram, Discord, Matrix, REPL — all drive the same agent
2. **Shared memory, isolated conversations** — Each channel has its own session but reads/writes the same workspace and long-term memory
3. **Independent lifecycles** — Adding a channel doesn't change the agent loop, provider, or tools

## Architecture

```
              ┌─────────────────────────────┐
              │        kitaebot run          │
              │                             │
              │  ┌─────────┐  ┌───────────┐ │
              │  │Telegram │  │ Heartbeat │ │
              │  │ poller  │  │  timer    │ │
              │  └────┬────┘  └─────┬─────┘ │
              │       │             │       │
              │       ▼             ▼       │
              │  ┌──────────────────────┐   │
              │  │    agent::run_turn() │   │
              │  └──────────┬───────────┘   │
              │             │               │
              │  ┌──────────▼───────────┐   │
              │  │ Provider / Tools     │   │
              │  └──────────────────────┘   │
              └─────────────────────────────┘

              ┌─────────────────────────────┐
              │       kitaebot chat          │
              │                             │
              │  ┌─────────┐                │
              │  │  REPL   │                │
              │  └────┬────┘                │
              │       │                     │
              │       ▼                     │
              │  ┌──────────────────────┐   │
              │  │    agent::run_turn() │   │
              │  └──────────┬───────────┘   │
              │             │               │
              │  ┌──────────▼───────────┐   │
              │  │ Provider / Tools     │   │
              │  └──────────────────────┘   │
              └─────────────────────────────┘

Shared:
  ~/.local/share/kitaebot/
  ├── sessions/
  │   ├── telegram.json      # Telegram channel session
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
│              │  3. Load session (sessions/telegram.json)
│              │  4. Call run_turn(message)
│              │  5. Send response via sendMessage API
│              │  6. Save session
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

The bot token is a secret, not a config value. Provided via environment variable:

```
TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
```

The `config.toml` may later hold non-secret Telegram settings (allowed chat IDs, polling timeout), but the token stays out of config files.

### Access Control

The bot should only respond to authorized users. MVP: a single allowed `chat_id` configured via environment variable or config. Messages from other chat IDs are silently ignored.

```
TELEGRAM_CHAT_ID=123456789
```

## Channel as Pattern, Not Trait

Each channel follows the same shape:

1. Wait for input (poll Telegram, read stdin, timer tick)
2. Acquire lock (if needed — only when multiple OS processes can collide)
3. Load per-channel session
4. Call `run_turn()` with the input as `Message::User`
5. Deliver the response (send Telegram message, print to stdout, write to HISTORY.md)
6. Save session

There is no `Channel` trait. Each channel module implements this pattern directly. The specifics vary enough (chat IDs, message threading, media types, delivery confirmation) that a shared trait would be either too thin to enforce anything useful or too leaky to accommodate real differences.

Extract the trait when the second channel arrives and the common shape is concrete, not before.

## Per-Channel Locking

Lock files prevent concurrent access where multiple OS processes could collide:

| Lock | Holder |
|------|--------|
| `locks/repl.lock` | REPL process (`kitaebot chat`) |
| `locks/heartbeat.lock` | Heartbeat invocation (`kitaebot heartbeat` / systemd timer) |

Telegram needs no lock — the poller is a single sequential loop inside the daemon process. The loop itself serializes message processing. Messages arriving during a turn are queued by Telegram's `getUpdates` offset mechanism and picked up on the next poll.

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

## MVP Simplifications

1. **Telegram only** — Single external channel
2. **Text only** — No images, documents, or media
3. **Single user** — One authorized chat_id
4. **No message queuing** — Process one message at a time, Telegram buffers the rest
5. **No typing indicators** — Agent appears offline until response is ready
6. **No message splitting** — Long responses sent as a single message (Telegram's 4096 char limit may need handling later)
