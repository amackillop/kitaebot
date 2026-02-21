# Tool System

## Purpose

Tools are capabilities the agent can invoke to interact with the environment. The tool system provides a registry for managing tools and a trait for implementing new ones.

## Why This Design?

1. **Extensibility** — New tools can be added without modifying the core loop
2. **Discoverability** — Tools describe themselves via JSON Schema for the LLM
3. **Safety** — Tools can validate arguments and enforce restrictions

## Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique name for this tool (used in function calls)
    fn name(&self) -> &'static str;

    /// Human-readable description for the LLM
    fn description(&self) -> &'static str;

    /// JSON Schema for parameters
    fn parameters(&self) -> serde_json::Value;

    /// Execute the tool with given arguments
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError>;
}
```

## Tool Registry

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: impl Tool + 'static);
    pub fn get(&self, name: &str) -> Option<&dyn Tool>;
    pub fn definitions(&self) -> Vec<ToolDefinition>;
    pub async fn execute(&self, name: &str, args: Value) -> Result<String, ToolError>;
}
```

## MVP Tool: `exec`

The only tool for MVP. Executes shell commands in the workspace.

### Parameters

```json
{
    "type": "object",
    "properties": {
        "command": {
            "type": "string",
            "description": "Shell command to execute"
        }
    },
    "required": ["command"]
}
```

### Behavior

1. Validate command against deny patterns
2. Execute with `tokio::process::Command`
3. Capture stdout/stderr
4. Return combined output (truncated if too long)

### Safety Guards

Commands are checked against deny patterns before execution:

```rust
const DENY_PATTERNS: &[&str] = &[
    r"rm\s+-[rf]",           // rm -r, rm -rf
    r"mkfs",                  // filesystem creation
    r"dd\s+if=",             // disk operations
    r">\s*/dev/",            // write to devices
    r"shutdown|reboot",      // system power
    r":\(\)\s*\{.*\};\s*:",  // fork bomb
];
```

**Note**: These are defense-in-depth, not security boundaries. The VM is the real isolation.

### Restrictions

| Restriction | Implementation |
|-------------|----------------|
| Working directory | Always `cwd = workspace` |
| Timeout | 60 seconds default |
| Output size | Truncate at 10KB |
| Path traversal | Block `../` in arguments |

### Output Format

```
$ ls -la
total 24
drwxr-xr-x  3 kitaebot kitaebot 4096 Feb 21 12:00 .
-rw-r--r--  1 kitaebot kitaebot  512 Feb 21 12:00 SOUL.md
...

Exit code: 0
```

On error:
```
$ invalid_command
Error: command not found: invalid_command

Exit code: 127
```

## Future Tools

Not in MVP, but the registry makes these easy to add:

| Tool | Purpose |
|------|---------|
| `read_file` | Read file contents (avoid shell for clarity) |
| `write_file` | Write file (atomic, with backup) |
| `edit_file` | Line-based editing |
| `web_fetch` | Fetch URL content |
| `web_search` | Search via SearXNG |

## Error Handling

```rust
pub enum ToolError {
    NotFound(String),
    InvalidArguments(String),
    ExecutionFailed(String),
    Timeout,
    Blocked(String),
}
```

Errors are returned to the LLM as text, not exceptions. The LLM decides how to proceed.
