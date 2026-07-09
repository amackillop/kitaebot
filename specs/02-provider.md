# Spec 02: LLM Provider

## Motivation

The provider module abstracts LLM API communication behind a trait so the agent
loop is decoupled from any specific backend. A single integration covers any
OpenAI-compatible chat completions endpoint.

## Behavior

### Trait

The `Provider` trait exposes one method:

```
chat(messages, tools) -> Result<Response, ProviderError>
```

`messages` is the full context window (system + conversation history).
`tools` is the set of tool definitions the LLM may invoke. When `tools` is
empty, the `tools` field is omitted from the wire request entirely.

### Response

The provider returns one of:

- **Text** — the LLM produced a text response (content present, no tool calls)
- **ToolCalls** — the LLM requested one or more tool invocations, each with an
  `id`, `function.name`, and `function.arguments` (raw JSON string, not parsed)

A `ToolCalls` response may also carry accompanying text content.

### Wire Format

Requests are serialized to the OpenAI chat completions format:

```json
{
    "model": "...",
    "messages": [...],
    "tools": [...],
    "max_tokens": 32768,
    "temperature": 0.7
}
```

Messages are tagged by `role` (system, user, assistant, tool). Both text-only
assistant messages and tool-call assistant messages serialize to the `assistant`
role. Tool results use `tool_call_id` on the wire.

Wire types are zero-copy, borrowing from domain types.

### Multi-Backend Support

The `Api` enum selects the endpoint:

| Api | Endpoint |
|-----|----------|
| `openrouter` (default) | `https://openrouter.ai/api/v1/chat/completions` |
| `openai` | `https://api.openai.com/v1/chat/completions` |
| `groq` | `https://api.groq.com/openai/v1/chat/completions` |
| `together` | `https://api.together.xyz/v1/chat/completions` |
| `mistral` | `https://api.mistral.ai/v1/chat/completions` |

All backends use the same wire format. Switching backend requires only changing
`provider.api` in config — no code changes.

### Error Mapping

| HTTP Status | Error |
|-------------|-------|
| 200-299 | Parse response. If deserialization fails or choices is empty, `InvalidResponse`. |
| 401, 403 | `Authentication` |
| 429 | `RateLimited` |
| 500-599 | `ServerError` with status code and response body |
| Any other 4xx | `BadRequest` with status code and response body |

Transport failures (connection, timeout, reading the body) map to `Network`.
`ProviderError::is_transient()` classifies retryability: `Network`,
`ServerError`, and `RateLimited` may succeed if the identical request is
resent; everything else is a defect in the request or credentials and must
never be retried.

There is no retry logic yet. Errors are returned directly to the caller.

## Boundaries

### Owns

- HTTP request construction and response parsing
- Wire format serialization (domain types to OpenAI format)
- Status code to error variant mapping
- Endpoint selection based on `Api` config

### Does Not Own

- API key loading — injected via `CompletionsClient` at startup
- Config parsing — receives `ProviderConfig` from the runtime
- Decision of what to do with errors — the agent loop handles that
- Tool definition schemas — provided by the tool registry

### Layering

The provider is split into two layers:

1. **CompletionsClient** — the IO layer. Holds the HTTP closure and API key.
   Sends raw bytes, receives status + body, maps to `ProviderError`. Pure
   response interpretation is separated from IO for testability.
2. **CompletionsProvider** — the domain adapter. Holds model/tokens/temperature
   config. Converts domain `Message`/`ToolDefinition` types to wire format,
   delegates to the client, converts wire response back to domain `Response`.

### Testing

- `MockProvider` (`#[cfg(test)]`) — returns pre-loaded responses in sequence.
  Tracks call count atomically.
- `CompletionsClient::from_fn()` (`#[cfg(test)]`) — accepts an arbitrary async
  closure as the HTTP effect.
- `mock-network` feature flag — makes `CompletionsClient::new()` return a
  hardcoded stub response at build time, for integration tests that don't need
  real HTTP.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Bad API key | `ProviderError::Authentication` returned to caller |
| Rate limited | `ProviderError::RateLimited` returned to caller |
| Malformed response (valid HTTP, bad JSON) | `ProviderError::InvalidResponse` |
| Empty choices array | `ProviderError::InvalidResponse` |
| No content, no tool calls, `finish_reason = "length"` | `ProviderError::Truncated` — generation hit `provider.max_tokens` before any output (reasoning models can burn the whole budget on reasoning); the fix is a larger `provider.max_tokens` |
| No content, no tool calls, other finish reason | `ProviderError::EmptyResponse` |
| Partial text, `finish_reason = "length"` | Surfaced as a normal reply with `[truncated at max_tokens]` appended, so readers and the model (next turn) see the cut |
| Transport failure (connect, timeout, body read) | `ProviderError::Network` |
| HTTP 5xx | `ProviderError::ServerError` with status and body |
| HTTP 4xx (other than 401/403/429) | `ProviderError::BadRequest` with status and body |

No retries for any failure. The agent loop surfaces the error to the user and
saves the session.

## Constraints

Configuration via `config.toml` under `[provider]`:

| Field | Default | Description |
|-------|---------|-------------|
| `api` | `openrouter` | Backend endpoint selection |
| `model` | `arcee-ai/trinity-large-preview:free` | Model identifier |
| `max_tokens` | 32768 | Max tokens in LLM response. Reasoning tokens count against it (OpenRouter), so it must cover reasoning plus output |
| `temperature` | 0.7 | Sampling temperature |
| `model_overrides.explore` | unset | Model for `explore` sub-agents |
| `model_overrides.worker` | unset | Model for `worker` sub-agents |
| `model_overrides.summarizer` | unset | Model for compaction summaries |
| `model_overrides.heartbeat` | unset | Model for heartbeat turns |

Each `model_overrides.*` key overrides the model for that role only; unset roles
fall back to `model`. `max_tokens` and `temperature` are shared across
all roles. Role providers are built once at startup via
`CompletionsProvider::with_model`; the model choice per role is static —
the agent cannot select models at runtime.

On OpenRouter, requests opt into usage accounting (`usage: {include:
true}`) and the response's token usage — including prompt-cache hits
(`prompt_tokens_details.cached_tokens`) — is logged at debug level.
Other APIs skip the opt-in field (strict endpoints reject unknown
params) but any standard `usage` object they return is still logged.
Prompt caching itself is implicit on OpenRouter for providers that
support it; no `cache_control` breakpoints are sent, so Anthropic and
Qwen models do not get cached.

The API key is loaded from the credentials directory as `provider-api-key`
(see [spec 13](13-credentials.md)). It is not an environment variable.

## Open Questions

None currently.
