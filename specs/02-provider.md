# LLM Provider

## Purpose

The provider module handles communication with the LLM API. It abstracts the HTTP details and provides a clean interface for the agent loop.

## Why OpenRouter?

OpenRouter is a gateway to multiple LLM providers (Anthropic, OpenAI, etc.) with a unified API. Benefits:

1. **Single integration** — One API, many models
2. **OpenAI-compatible** — Well-documented, widely supported format
3. **Fallback options** — Can switch models without code changes
4. **Usage tracking** — Built-in cost monitoring

## Interface

```rust
pub struct Provider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
}

impl Provider {
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<Response, ProviderError>;
}

pub enum Response {
    Text(String),
    ToolCalls(Vec<ToolCall>),
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}
```

## Request Format

```json
{
    "model": "anthropic/claude-sonnet-4",
    "messages": [
        {"role": "system", "content": "..."},
        {"role": "user", "content": "..."}
    ],
    "tools": [
        {
            "type": "function",
            "function": {
                "name": "exec",
                "description": "Execute a shell command",
                "parameters": { ... }
            }
        }
    ],
    "max_tokens": 4096,
    "temperature": 0.7
}
```

## Response Parsing

The API returns either:

**Text response:**
```json
{
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "Here's what I found..."
        }
    }]
}
```

**Tool call response:**
```json
{
    "choices": [{
        "message": {
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_abc123",
                "type": "function",
                "function": {
                    "name": "exec",
                    "arguments": "{\"command\": \"ls -la\"}"
                }
            }]
        }
    }]
}
```

## Error Handling

| HTTP Status | Meaning | Action |
|-------------|---------|--------|
| 200 | Success | Parse response |
| 400 | Bad request | Return error (likely our bug) |
| 401 | Unauthorized | Return error (bad API key) |
| 429 | Rate limited | Return error (could retry, but keep simple) |
| 500+ | Server error | Return error |

## Configuration

```toml
[provider]
api_key = "sk-or-..."  # Or OPENROUTER_API_KEY env var
model = "anthropic/claude-sonnet-4"
max_tokens = 4096
temperature = 0.7
```

## Why Not Streaming (MVP)?

Streaming complicates:
- Response parsing (SSE chunks)
- Tool call detection (must buffer)
- Error handling (partial responses)

Batch is simpler. Add streaming when UX demands it.

## Future Considerations

- **Retry logic**: Currently no retries. Could add exponential backoff for transient errors.
- **Multiple providers**: Could add direct Anthropic/OpenAI clients, but OpenRouter makes this unnecessary.
- **Caching**: Could cache identical requests, but LLM responses are rarely identical.
