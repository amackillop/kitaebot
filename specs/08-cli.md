# CLI Interface

## Purpose

The CLI is the primary way users interact with the agent. It provides an interactive REPL for conversations and commands for management tasks.

## Why CLI?

1. **Universal** — Works over SSH, in terminals, everywhere
2. **Simple** — No web server, no ports, no auth
3. **Scriptable** — Can pipe input/output
4. **Low overhead** — No UI framework needed

## Usage

```bash
# Interactive REPL (primary use)
kitaebot

# Single message (scripting)
kitaebot "What's in my workspace?"

# Commands
kitaebot heartbeat      # Run heartbeat manually
kitaebot config         # Show configuration
kitaebot version        # Show version
```

## Interactive Mode

```
$ kitaebot
kitaebot v0.1.0
Type /help for commands, /quit to exit.

> What files are in my workspace?

Looking at your workspace...

[exec] ls -la

Your workspace contains:
- SOUL.md (agent personality)
- session.json (conversation history)
- projects/ (your working area)

> /quit
Goodbye!
```

## Commands

Commands start with `/` and are handled by the CLI, not sent to the agent:

| Command | Action |
|---------|--------|
| `/help` | Show available commands |
| `/quit` | Exit the CLI |
| `/new` | Clear session, start fresh |
| `/history` | Show recent messages |
| `/config` | Show current configuration |
| `/soul` | Display SOUL.md contents |

## Input Handling

```rust
fn read_input() -> Result<Input> {
    let line = readline("> ")?;
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return Ok(Input::Empty);
    }

    if trimmed.starts_with('/') {
        return Ok(Input::Command(trimmed[1..].to_string()));
    }

    Ok(Input::Message(line))
}
```

## Output Formatting

### Regular messages

```
> user message here

Agent response here, possibly
spanning multiple lines.
```

### Tool calls

```
> run the tests

Running your tests...

[exec] cargo test

running 5 tests
test test_one ... ok
test test_two ... ok
...

All 5 tests passed!
```

### Errors

```
> do something impossible

Error: Unable to complete request: file not found
```

## Non-Interactive Mode

For scripting and automation:

```bash
# Single message
echo "List files" | kitaebot

# With explicit message
kitaebot "List files"

# From file
kitaebot < prompt.txt
```

Output goes to stdout, errors to stderr.

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error |
| 2 | Configuration error |
| 3 | Provider error (API) |

## Configuration via CLI

```bash
# Override model
kitaebot --model anthropic/claude-sonnet-4

# Custom config file
kitaebot --config /path/to/config.toml

# Verbose output
kitaebot -v
```

## Implementation

```rust
#[derive(Parser)]
#[command(name = "kitaebot")]
struct Cli {
    /// Message to send (if not interactive)
    message: Option<String>,

    /// Run heartbeat
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run heartbeat check
    Heartbeat,
    /// Show configuration
    Config,
    /// Show version
    Version,
}
```

## REPL Loop

```rust
async fn repl(agent: &Agent) -> Result<()> {
    println!("kitaebot v{}", VERSION);
    println!("Type /help for commands, /quit to exit.\n");

    loop {
        match read_input()? {
            Input::Empty => continue,
            Input::Command(cmd) => {
                if !handle_command(&cmd)? {
                    break; // /quit
                }
            }
            Input::Message(msg) => {
                let response = agent.process_message(&msg).await?;
                println!("\n{}\n", response);
            }
        }
    }

    println!("Goodbye!");
    Ok(())
}
```

## Future Considerations

- **Readline support** — History, completion, editing
- **Colors** — Syntax highlighting for code blocks
- **Progress indicators** — Spinner while waiting for response
- **Streaming output** — Print tokens as they arrive
- **Multiline input** — For pasting code blocks
