# Session Management

## Purpose

The session module persists conversation history across agent restarts. It maintains the context that makes the agent feel continuous rather than amnesiac.

## Why Persistence Matters

Without persistence:
- Agent forgets everything on restart
- User must re-explain context every time
- No sense of ongoing relationship

With persistence:
- Conversations continue naturally
- Agent builds understanding over time
- User can reference past interactions

## Data Structure

```rust
pub struct Session {
    pub messages: Vec<SessionMessage>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct SessionMessage {
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    pub tool_results: Option<Vec<ToolResultRecord>>,
}

pub enum Role {
    User,
    Assistant,
}
```

## Storage Format

Session is stored as `session.json` in the workspace:

```json
{
    "messages": [
        {
            "role": "user",
            "content": "What files are in my workspace?",
            "timestamp": "2024-02-21T12:00:00Z"
        },
        {
            "role": "assistant",
            "content": "Let me check...",
            "timestamp": "2024-02-21T12:00:01Z",
            "tool_calls": [
                {"id": "call_123", "name": "exec", "arguments": {"command": "ls"}}
            ]
        },
        {
            "role": "assistant",
            "content": "Your workspace contains: SOUL.md, session.json, projects/",
            "timestamp": "2024-02-21T12:00:02Z"
        }
    ],
    "created_at": "2024-02-21T12:00:00Z",
    "updated_at": "2024-02-21T12:00:02Z"
}
```

## Operations

```rust
impl Session {
    /// Load session from disk, or create new if not exists
    pub fn load(workspace: &Path) -> Result<Self>;

    /// Save session to disk
    pub fn save(&self, workspace: &Path) -> Result<()>;

    /// Add a message to the session
    pub fn add_message(&mut self, message: SessionMessage);

    /// Get messages for context building
    pub fn get_history(&self, limit: usize) -> &[SessionMessage];

    /// Clear session (user command: /new)
    pub fn clear(&mut self);
}
```

## MVP Simplifications

For MVP, we keep it simple:

1. **No consolidation** — Messages accumulate unbounded
2. **No windowing** — All messages sent to LLM (until context limit)
3. **No backup** — Single file, atomic write
4. **No encryption** — Plain JSON (workspace is private anyway)

## File Safety

Writes use atomic rename to prevent corruption:

```rust
fn save(&self, workspace: &Path) -> Result<()> {
    let path = workspace.join("session.json");
    let temp = workspace.join("session.json.tmp");

    let content = serde_json::to_string_pretty(self)?;
    fs::write(&temp, content)?;
    fs::rename(&temp, &path)?;

    Ok(())
}
```

## Session Commands

User can control the session via CLI commands:

| Command | Action |
|---------|--------|
| `/new` | Clear session, start fresh |
| `/history` | Show recent messages |
| `/export` | Dump session to stdout |

## Future Considerations

### Memory Consolidation

When sessions grow large, we'll need to consolidate:

1. Take oldest N messages
2. Ask LLM to summarize key facts
3. Write to `MEMORY.md`
4. Append summary to `HISTORY.md`
5. Remove old messages from session

This keeps context manageable while preserving important information.

### Token Counting

Currently no token awareness. Eventually:

1. Estimate tokens per message
2. Track cumulative context size
3. Trigger consolidation before hitting limit
4. Truncate from oldest if emergency

### Multiple Sessions

Current design is single-session. Could extend to:

- Named sessions (`kitaebot --session project-x`)
- Auto-switching based on context
- Session templates
