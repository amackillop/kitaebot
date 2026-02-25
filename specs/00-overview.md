# Kitaebot System Overview

## What Is Kitaebot?

A personal AI agent that runs in a NixOS VM. You SSH in, chat with it, and ask it to do things. It has a persistent personality ("soul"), remembers conversations, and can execute shell commands in its isolated workspace.

## Why Build This?

Existing solutions (nanobot, OpenClaw) are feature-rich but complex. Kitaebot prioritizes:

1. **Simplicity** — Minimal code, easy to understand and modify
2. **Security** — VM isolation, workspace confinement, no network exposure by default
3. **Privacy** — Self-hosted, your data stays on your machine
4. **Reproducibility** — NixOS means identical environments everywhere

## System Architecture

```
┌────────────────────────────────────────────────────────┐
│                     NixOS VM                           │
│  ┌──────────────────────────────────────────────────┐  │
│  │                  kitaebot daemon                 │  │
│  │  ┌────────────┐  ┌────────────┐  ┌────────────┐  │  │
│  │  │   Agent    │  │  Provider  │  │   Tools    │  │  │
│  │  │   Loop     │──│ (OpenRouter│──│  (exec)    │  │  │
│  │  └────────────┘  └────────────┘  └────────────┘  │  │
│  │         │                                        │  │
│  │  ┌──────▼─────┐  ┌────────────┐                  │  │
│  │  │  SOUL.md   │  │session.json│                  │  │
│  │  │  (prompt)  │  │ (history)  │                  │  │
│  │  └────────────┘  └────────────┘                  │  │
│  └──────────────────────────────────────────────────┘  │
│                                                        │
│  ┌──────────────────────────────────────────────────┐  │
│  │              ~/.local/share/kitaebot             │  │
│  │  (workspace: files, projects, agent state)       │  │
│  └──────────────────────────────────────────────────┘  │
│                                                        │
│  ┌──────────────┐                                      │
│  │    sshd      │◄─────── user connects via SSH        │
│  └──────────────┘                                      │
└────────────────────────────────────────────────────────┘
```

## Components

| Spec | Component | Purpose |
|------|-----------|---------|
| [01](01-agent-loop.md) | Agent Loop | Core conversation/tool execution cycle |
| [02](02-provider.md) | LLM Provider | OpenRouter API client |
| [03](03-tools.md) | Tool System | Extensible tool registry + exec tool |
| [04](04-session.md) | Session | Conversation persistence |
| [05](05-workspace.md) | Workspace | File structure and isolation |
| [06](06-soul.md) | Soul | Agent personality and system prompt |
| [07](07-heartbeat.md) | Heartbeat | Periodic task execution |
| [08](08-cli.md) | CLI | User interface |
| [09](09-vm.md) | NixOS VM | Deployment and system configuration |

## Data Flow

1. User SSHs into VM, runs `kitaebot`
2. CLI reads user input
3. Agent loop builds context (soul + session history + user message)
4. Provider sends request to OpenRouter
5. If response contains tool calls, execute them and loop
6. Final text response displayed to user
7. Session updated and persisted

## Design Principles

- **Flat over nested** — Start with simple module structure, extract when needed
- **Explicit over magic** — Configuration is visible and editable
- **Fail loudly** — Errors should be clear, not swallowed
- **Minimal dependencies** — Only add what's necessary
