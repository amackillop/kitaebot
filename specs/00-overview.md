# Kitaebot System Overview

## What Is Kitaebot?

A personal AI agent that runs in a NixOS VM. You communicate with it via Telegram (phone) or a Unix socket (computer). It has a persistent personality ("soul"), maintains per-channel conversation history, shares long-term memory across all channels, and can execute shell commands in its isolated workspace.

## Why Build This?

Existing solutions (nanobot, OpenClaw) are feature-rich but complex. Kitaebot prioritizes:

1. **Simplicity** — Minimal code, easy to understand and modify
2. **Security** — VM isolation, workspace confinement, no network exposure by default
3. **Privacy** — Self-hosted, your data stays on your machine
4. **Reproducibility** — NixOS means identical environments everywhere

## System Architecture

```
┌──────────────────────────────────────────────────────────┐
│                        NixOS VM                          │
│                                                          │
│  ┌────────────────────────────────────────────────────┐  │
│  │             kitaebot run  (daemon)                 │  │
│  │                                                    │  │
│  │  ┌──────────┐ ┌──────────┐ ┌───────────┐            │  │
│  │  │ Telegram │ │  Socket  │ │ Heartbeat │ ← channels │  │
│  │  │  poller  │ │ listener │ │   timer   │            │  │
│  │  └─────┬────┘ └─────┬────┘ └─────┬─────┘            │  │
│  │        │           │             │                  │  │
│  │        ▼           ▼             ▼                  │  │
│  │  ┌──────────┐  ┌────────────┐  ┌──────────┐        │  │
│  │  │  Agent   │  │ Provider   │  │  Tools   │        │  │
│  │  │  Loop    │──│(OpenRouter)│──│  (exec)  │        │  │
│  │  └──────────┘  └────────────┘  └──────────┘        │  │
│  └────────────────────────────────────────────────────┘  │
│                                                          │
│  ┌────────────────────────────────────────────────────┐  │
│  │            ~/.local/share/kitaebot                 │  │
│  │                                                    │  │
│  │  sessions/          memory/         SOUL.md        │  │
│  │  ├── telegram.json  └── HISTORY.md  AGENTS.md      │  │
│  │  ├── socket.json                    HEARTBEAT.md   │  │
│  │  ├── heartbeat.json                 config.toml    │  │
│  │  └── repl.json                                     │  │
│  └────────────────────────────────────────────────────┘  │
│                                                          │
│  ┌──────────────┐                                        │
│  │    sshd      │◄── user connects via SSH               │
│  │              │    runs `kitaebot chat` (debug REPL)   │
│  └──────────────┘                                        │
└──────────────────────────────────────────────────────────┘
```

## Binary Design

Single binary, two modes:

| Command | Role | Lifecycle |
|---------|------|-----------|
| `kitaebot run` | Daemon: Telegram poller + socket listener + heartbeat timer | Long-lived (systemd service) |
| `kitaebot chat` | Local REPL for debugging and backup | Interactive, on-demand |

Both are independent processes sharing the workspace. No IPC — coordination via per-channel file locks.

## Components

| Spec | Component | Purpose |
|------|-----------|---------|
| [01](01-agent-loop.md) | Agent Loop | Core conversation/tool execution cycle |
| [02](02-provider.md) | LLM Provider | OpenRouter API client |
| [03](03-tools.md) | Tool System | Extensible tool registry + exec tool |
| [04](04-session.md) | Session | Per-channel conversation persistence |
| [05](05-workspace.md) | Workspace | File structure and isolation |
| [06](06-soul.md) | Soul | Agent personality and system prompt |
| [07](07-heartbeat.md) | Heartbeat | Periodic awareness checks |
| [08](08-cli.md) | CLI | Subcommands and REPL interface |
| [09](09-vm.md) | NixOS VM | Deployment and system configuration |
| [10](10-channels.md) | Channels | External messaging interfaces (Telegram, Unix socket) |
| [11](11-safety.md) | Safety | Leak detection and output wrapping |
| [12](12-context.md) | Context | Token budget and conversation windowing |
| [13](13-credentials.md) | Credentials | Credential isolation and secret loading |
| [14](14-memory.md) | Memory | Long-term recall across sessions and channels |

## Data Flow

### Telegram

1. Daemon polls Telegram for new messages
2. Incoming message translated to `Message::User`
3. Agent loop builds context (soul + channel session + user message)
4. Provider sends request to OpenRouter
5. If response contains tool calls, execute them and loop
6. Final text response sent back to Telegram
7. Channel session updated and persisted

### Socket

1. Client connects to `/run/kitaebot/chat.sock`
2. Sends NDJSON message or command
3. Same agent loop as Telegram, different session (`sessions/socket.json`)
4. Response written back as NDJSON

### REPL (debug)

1. User SSHs into VM, runs `kitaebot chat`
2. Same agent loop, different session (`sessions/repl.json`)
3. Response printed to stdout

### Heartbeat

1. Internal timer fires (every 30 minutes)
2. Agent reviews HEARTBEAT.md in context of prior heartbeat session
3. Acts if needed, responds HEARTBEAT_OK if not
4. Result appended to `memory/HISTORY.md`

## Design Principles

- **Flat over nested** — Start with simple module structure, extract when needed
- **Explicit over magic** — Configuration is visible and editable
- **Fail loudly** — Errors should be clear, not swallowed
- **Minimal dependencies** — Only add what's necessary
- **Channel as pattern, not trait** — Each channel follows the same shape but a shared trait adds no value given the transport differences
