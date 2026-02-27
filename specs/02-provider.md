# LLM Provider

## Purpose

The provider module handles communication with the LLM API. It abstracts the HTTP details behind a trait so the agent loop doesn't care which backend is used.

## Why OpenRouter?

OpenRouter is a gateway to multiple LLM providers (Anthropic, OpenAI, etc.) with a unified API. Benefits:

1. **Single integration** — One API, many models
2. **OpenAI-compatible** — Well-documented, widely supported format
3. **Fallback options** — Can switch models without code changes
4. **Usage tracking** — Built-in cost monitoring

## Interface

The `Provider` trait defines a single `chat` method. `OpenRouterProvider` is the current production implementation; `StubProvider` (feature-gated behind `mock-network`) is used for testing.

Tool call arguments are passed as a JSON **string** (not a parsed `Value`), matching the OpenAI wire format. The `Tools` module deserializes them.

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

The `tools` field is omitted when no tools are registered.

## Response Parsing

The API returns either:

**Text response:** `choices[0].message.content` is present, no `tool_calls`.

**Tool call response:** `choices[0].message.tool_calls` contains one or more calls, each with `id`, `function.name`, and `function.arguments` (JSON string).

## Error Handling

| HTTP Status | Meaning | Action |
|-------------|---------|--------|
| 200 | Success | Parse response |
| 400 | Bad request | Return error (likely our bug) |
| 401 | Unauthorized | Return `ProviderError::Authentication` |
| 429 | Rate limited | Return `ProviderError::RateLimited` |
| Other | Server/unknown error | Return `ProviderError::Network` with status and body |

## Configuration

- **API key**: `OPENROUTER_API_KEY` environment variable (required, read in `main.rs`)
- **Model/tokens/temperature**: Read from `config.toml` via `ProviderConfig` (see `src/config.rs`)

The provider itself has no env var or config file awareness — `main.rs` reads both and passes values into `OpenRouterProvider::new(api_key, &config.provider)`.

Defaults (when no `config.toml` exists): model `arcee-ai/trinity-large-preview:free`, max_tokens 4096, temperature 0.7.

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
