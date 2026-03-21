# Kitaebot System Overview

## What Is Kitaebot?

A personal AI agent that runs in a NixOS VM. You communicate with it via Telegram (phone), a Unix socket (computer), or GitHub PR comments (code review). It has a persistent personality ("soul"), maintains a unified conversation history shared across all channels, and can execute shell commands in its isolated workspace.

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
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────┐ │  │
│  │  │ Telegram │ │  Socket  │ │ GitHub   │ │Heartbt │ │  │
│  │  │  poller  │ │ listener │ │ PR poll  │ │ timer  │ │  │
│  │  └────┬─────┘ └────┬─────┘ └────┬─────┘ └───┬────┘ │  │
│  │       │            │            │           │      │  │
│  │       └────────────┴────────────┴───────────┘      │  │
│  │                        │                           │  │
│  │                        ▼                           │  │
│  │              ┌──────────────────┐                  │  │
│  │              │   AgentHandle    │ (cloneable)      │  │
│  │              └────────┬─────────┘                  │  │
│  │                       │ mpsc                       │  │
│  │                       ▼                            │  │
│  │              ┌──────────────────┐                  │  │
│  │              │  Agent (actor)   │ sequential       │  │
│  │              │  ├─ Session      │ unified          │  │
│  │              │  ├─ Provider     │                  │  │
│  │              │  └─ Tools        │                  │  │
│  │              └──────────────────┘                  │  │
│  └────────────────────────────────────────────────────┘  │
│                                                          │
│  ┌────────────────────────────────────────────────────┐  │
│  │            ~/.local/share/kitaebot                 │  │
│  │                                                    │  │
│  │  session.json       memory/         SOUL.md        │  │
│  │  (unified)          └── HISTORY.md  AGENTS.md      │  │
│  │                                     HEARTBEAT.md   │  │
│  │                                     config.toml    │  │
│  └────────────────────────────────────────────────────┘  │
│                                                          │
│  ┌──────────────┐                                        │
│  │    sshd      │◄── user connects via SSH               │
│  │              │    runs `kchat` (socket client)        │
│  └──────────────┘                                        │
└──────────────────────────────────────────────────────────┘
```

## Binary Design

Two binaries:

| Binary | Role | Lifecycle |
|--------|------|-----------|
| `kitaebot run` | Daemon: Telegram + socket + GitHub + heartbeat | Long-lived (systemd service) |
| `kchat <socket-path>` | Thin NDJSON client for the Unix socket | Interactive, on-demand |

Interactive access is through `kchat` connecting to the daemon's Unix socket. No separate REPL process.

## Components

| Spec | Component | Purpose |
|------|-----------|---------|
| [01](01-agent-loop.md) | Agent Loop | Core conversation/tool execution cycle |
| [02](02-provider.md) | LLM Provider | Multi-backend chat completions |
| [03](03-tools.md) | Tool System | Tool registry, exec, file ops, git, GitHub |
| [04](04-session.md) | Session | Unified conversation persistence |
| [05](05-workspace.md) | Workspace | Directory structure and prompt assembly |
| [06](06-system-prompt.md) | System Prompt | Prompt files, assembly, and injection |
| [07](07-heartbeat.md) | Heartbeat | Periodic awareness checks |
| [08](08-cli.md) | Binaries | Daemon lifecycle and socket client |
| [09](09-vm.md) | NixOS VM | Deployment and system configuration |
| [10](10-channels.md) | Channels | External messaging interfaces (Telegram, Unix socket, GitHub) |
| [11](11-safety.md) | Safety | Leak detection and output wrapping |
| [12](12-context.md) | Context | Token budget and compaction |
| [13](13-credentials.md) | Credentials | Secret loading and isolation |
| [14](14-memory.md) | Memory | Long-term recall (not yet implemented) |
| [15](15-sandbox.md) | Sandbox | Landlock filesystem confinement |
| [16](16-activity.md) | Activity | Structured turn events for channel observability |

## Data Flow

All channels follow the same pattern: construct a message, send it through `AgentHandle::send_message()`, await the reply. The actor tags each message with its `ChannelSource` (e.g. `[Telegram]`, `[GitHub PR #42]`) before appending to the unified session.

### Telegram

1. Daemon polls Telegram for new messages
2. Channel sends message through `AgentHandle`
3. Actor loads unified session, runs agent turn
4. Final text response sent back to Telegram

### Socket

1. Client connects to `/run/kitaebot/chat.sock`
2. Sends NDJSON message or command
3. Routed through `AgentHandle` — same unified session as all channels
4. Response written back as NDJSON

### GitHub

1. Poller searches for bot's open PRs via `gh search prs --author=@me`
2. For each PR, fetches reviews, comments, and inline diff comments newer than `last_poll`
3. Each new item sent through `AgentHandle` with `ChannelSource::GitHub { pr_number }`
4. Agent responds in context of the full unified session

### Heartbeat

1. Internal timer fires (configurable interval, default 30 minutes)
2. Sends `/heartbeat` through `AgentHandle` — processed as a slash command
3. Command handler reads HEARTBEAT.md, builds prompt, runs agent turn
4. Result appended to `memory/HISTORY.md`

## Design Principles

- **Flat over nested** — Start with simple module structure, extract when needed
- **Explicit over magic** — Configuration is visible and editable
- **Fail loudly** — Errors should be clear, not swallowed
- **Minimal dependencies** — Only add what's necessary
- **Channel as pattern, not trait** — Each channel follows the same shape but a shared trait adds no value given the transport differences
