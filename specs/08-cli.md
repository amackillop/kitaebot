# CLI Interface

## Purpose

The CLI is the primary way users interact with the agent. It provides an interactive REPL for conversations.

## Why CLI?

1. **Universal** — Works over SSH, in terminals, everywhere
2. **Simple** — No web server, no ports, no auth
3. **Scriptable** — Can pipe input/output
4. **Low overhead** — No UI framework needed

## Interactive Mode

```
$ kitaebot
New session

> What files are in my workspace?

Looking at your workspace...

[exec] ls -la

Your workspace contains:
- SOUL.md (agent personality)
- session.json (conversation history)
- projects/ (your working area)

> /quit
```

On resume:

```
$ kitaebot
Resumed session (5 messages)

>
```

## Commands

| Input | Action |
|-------|--------|
| `/new`  | Clear session, rebuild system prompt, start fresh |
| `/quit` | Exit the REPL |
| EOF (Ctrl-D) | Exit the REPL |

Empty/whitespace-only input is silently skipped.

## Startup

1. Initialize provider from `OPENROUTER_API_KEY` env var (exit 1 on failure)
2. Initialize workspace (exit 1 on failure)
3. Load tools (exec with workspace as cwd)
4. Load session from disk (exit 1 on failure)
5. Cache system prompt
6. Print session status ("New session" or "Resumed session (N messages)")
7. Enter REPL loop

## Turn Cycle

1. Read line from stdin
2. Skip if empty, break if `/quit` or EOF
3. Handle `/new` (clear session, save, rebuild prompt)
4. Otherwise: `run_turn()` → print response → save session
5. On error: print to stderr, continue

## Error Behavior

- Provider init failure: print message, suggest setting env var, exit 1
- Workspace init failure: print message, exit 1
- Session load failure: print message, exit 1
- Turn error: print to stderr, continue REPL
- Session save failure: print to stderr, continue REPL

## Future Considerations

- **clap integration** — Subcommands (`heartbeat`, `config`, `version`), CLI args (`--model`, `--config`, `-v`)
- **Slash commands** — `/help`, `/history`, `/config`, `/soul`
- **Non-interactive mode** — `kitaebot "message"` or `echo "message" | kitaebot`
- **Exit codes** — Distinguish config errors (2) from provider errors (3)
- **Readline support** — History, completion, editing
- **Colors** — Syntax highlighting for code blocks
- **Progress indicators** — Spinner while waiting for response
- **Streaming output** — Print tokens as they arrive
- **Multiline input** — For pasting code blocks
- **REPL lock** — Single-instance enforcement via PID lock file to prevent concurrent REPL sessions and coordinate with heartbeat
