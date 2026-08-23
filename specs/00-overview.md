# Kitaebot System Overview

## What Is Kitaebot?

A personal AI agent that runs in a NixOS VM. You communicate with it via Telegram (phone), a Unix socket (computer), GitHub PR comments (code review), or Linear issues (work items). It has a persistent personality ("soul"), maintains a unified conversation history shared across all channels, and can execute shell commands in its isolated workspace.

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
│  │  ┌───────────────────────────────────────────────┐ │  │
│  │  │Telegram · Socket · GitHub · Linear · Heartbeat│ │  │
│  │  │ poller    listener  PR poll  issues    timer  │ │  │
│  │  └─────────────────────┬─────────────────────────┘ │  │
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
│  │  sessions/          memory/         SOUL.md        │  │
│  │  state/             └── MEMORY.md   AGENTS.md      │  │
│  │  └── JOURNAL.md                     USER.md        │  │
│  │  projects/                          config.toml    │  │
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
| 04 | Session | Folded into [spec 14](14-context-engine.md) (flat engine storage) |
| [05](05-workspace.md) | Workspace | Directory structure and prompt assembly |
| [06](06-system-prompt.md) | System Prompt | Prompt files, assembly, and injection |
| [07](07-heartbeat.md) | Heartbeat | Periodic awareness checks |
| [08](08-binaries.md) | Binaries | Daemon lifecycle and socket client |
| [09](09-vm.md) | NixOS VM | Deployment and system configuration |
| [10](10-channels.md) | Channels | Shared channel contract, Telegram, Unix socket |
| [11](11-safety.md) | Safety | Leak detection and output wrapping |
| 12 | Context | Folded into [spec 14](14-context-engine.md) (flat engine compaction) |
| [13](13-credentials.md) | Credentials | Secret loading and isolation |
| [14](14-context-engine.md) | Context Engine | Pluggable context management (LCM DAG, sessions) |
| [15](15-sandbox.md) | Sandbox | Landlock filesystem confinement |
| [16](16-activity.md) | Activity | Structured turn events for channel observability |
| [17](17-notify.md) | Notify | Push notifications to user |
| [18](18-egress-filter.md) | Egress Filter | Domain-allowlisted outbound proxy |
| [19](19-sub-agents.md) | Sub-Agents | Task delegation with isolated child contexts |
| [20](20-github.md) | GitHub Channel | PR feedback polling and review requests |
| [21](21-memory.md) | Memory | Durable cross-session knowledge: index, topics, distillation |
| [22](22-mcp.md) | MCP Client | External MCP stdio servers as tools (bkb, Grafana) |
| [23](23-self-review.md) | Self-Review | Review pipeline: gates, reviewer sub-agent, findings ledger |
| [24](24-self-directed-work.md) | Self-Directed Work | Duty scheduler and built-in duties |
| [25](25-github-issues.md) | GitHub Issues Channel | Assigned-issue polling, plan-then-execute flow |
| [26](26-linear.md) | Linear Channel | Assigned-issue polling, plan-then-execute flow |

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

1. Poller searches for the bot's open PRs via the REST search API
2. For each PR, fetches reviews, comments, and inline diff comments newer than `last_poll`
3. Each new item sent through `AgentHandle` with `ChannelSource::GitHub { pr_number }`
4. Agent responds in context of the full unified session

### Linear

1. Poller fetches issues assigned to the bot's Linear user
2. New issues and trusted comments sent through `AgentHandle` with `ChannelSource::Linear { issue }`
3. Agent replies (plan, revision, or completion note) are posted back as issue comments

### Duties

1. The scheduler wakes a duty that is due ([spec 24](24-self-directed-work.md))
2. Its input is sent through `AgentHandle` with `ChannelSource::Duty`, on the duty's session
3. The outcome is appended to `state/JOURNAL.md` — durable, unlike the systemd journal, which is where an unattended run would otherwise only be visible

## Design Principles

- **Flat over nested** — Start with simple module structure, extract when needed
- **Explicit over magic** — Configuration is visible and editable
- **Fail loudly** — Errors should be clear, not swallowed
- **Minimal dependencies** — Only add what's necessary
- **Channel as pattern, not trait** — Each channel follows the same shape but a shared trait adds no value given the transport differences
