//! Error types for the agent.
//!
//! Models all failure modes explicitly as algebraic data types.
//! No generic "something failed" errors - each error explains what went wrong.

use std::path::PathBuf;

use thiserror::Error;

/// Top-level agent error.
#[derive(Debug, Error)]
pub enum Error {
    /// Context engine error.
    #[error("Engine error: {0}")]
    Engine(#[from] EngineError),

    /// Turn cancelled by client disconnect.
    #[error("Turn cancelled")]
    Cancelled,

    /// Maximum iterations reached without completion.
    ///
    /// The agent loop stopped after hitting the iteration limit to prevent
    /// infinite loops and runaway API costs.
    #[error("Maximum iterations reached without completion")]
    MaxIterationsReached,

    /// The model re-emitted the same tool call after being told it was
    /// no longer being executed.
    ///
    /// Distinct from [`Self::MaxIterationsReached`] because the budget
    /// was not the problem: the turn had rounds left and was spending
    /// them on a call whose result it already had. Ending early turns a
    /// livelock that costs the whole budget into one that costs a
    /// handful of calls.
    #[error("Turn made no progress: the same tool call was repeated after being refused")]
    NoProgress,

    /// LLM provider error (network, auth, etc.).
    #[error("Provider error: {0}")]
    Provider(#[from] ProviderError),

    /// Safety layer blocked the output.
    #[error("Safety error: {0}")]
    Safety(#[from] SafetyError),

    /// Session load or save failure.
    #[error("Session error: {0}")]
    Session(#[from] SessionError),

    /// Tool execution error.
    #[error("Tool error: {0}")]
    Tool(#[from] ToolError),
}

/// Context engine errors.
#[derive(Debug, Error)]
pub enum EngineError {
    /// LLM provider error during compaction summarization.
    #[error("Provider error: {0}")]
    Provider(#[from] ProviderError),

    /// Session persistence failure (I/O, parse, serialize).
    #[error("Session error: {0}")]
    Session(#[from] SessionError),

    /// `SQLite` or other storage backend failure.
    #[error("Storage error: {0}")]
    #[allow(dead_code)] // Used by the LCM engine.
    Storage(String),
}

/// LLM provider errors.
#[derive(Debug, Clone, Error)]
pub enum ProviderError {
    /// Authentication failed (invalid API key, etc.).
    #[error("Authentication failed")]
    Authentication,

    /// Provider returned success with no text and no tool calls.
    #[error("Provider returned an empty response")]
    EmptyResponse,

    /// Invalid response from provider (malformed JSON, missing fields, etc.).
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// A tool call whose name violates the API's own tool-name grammar,
    /// `^[a-zA-Z0-9_-]+$`. Refused before it can enter history.
    #[error("Provider returned a tool call with a malformed name: {name:?}")]
    MalformedToolCall { name: String },

    /// Transport-level failure (connection, timeout, reading the body).
    #[error("Network error: {0}")]
    Network(String),

    /// HTTP 5xx: the provider failed, the request may be fine.
    #[error("Provider server error: {0}")]
    ServerError(String),

    /// HTTP 4xx other than 401/403/429: the request itself is bad.
    #[error("Provider rejected the request: {0}")]
    BadRequest(String),

    /// Rate limited by the provider.
    #[error("Rate limited")]
    RateLimited,

    /// Generation hit `max_tokens` before producing any content or
    /// tool calls (e.g. a reasoning model spending the whole budget
    /// on reasoning). Raise `provider.max_tokens`.
    #[error("Provider response truncated at max_tokens before any content")]
    Truncated,
}

impl ProviderError {
    /// True when the identical request may succeed if resent.
    ///
    /// `MalformedToolCall` qualifies because sampling is not
    /// deterministic at our temperature: the request is fine, the draw
    /// was not, and a fresh draw is the whole remedy.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::MalformedToolCall { .. }
                | Self::Network(_)
                | Self::RateLimited
                | Self::ServerError(_)
        )
    }
}

/// Tool execution errors.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Tool execution blocked by policy.
    #[error("Blocked: {operation} ({guidance})")]
    Blocked {
        /// What was attempted (e.g. the shell command or path).
        operation: String,
        /// Why it was blocked / what to do instead.
        guidance: String,
    },

    /// Tool execution failed.
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),

    /// Invalid arguments passed to tool.
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),

    /// Tool not found in registry.
    #[error("Tool not found: {0}")]
    NotFound(String),

    /// A subprocess failed at the OS level before its output could be
    /// collected — an `execve`/`fork` failure (the usual case) or an
    /// I/O error while waiting on it. Distinct from a nonzero exit,
    /// which is not an error. Names the full argv, the cwd, and the OS
    /// error, so e.g. a Landlock-denied `/proc/self/exe` re-exec shows
    /// the confine wrapper in `argv` and `EACCES` in `source`.
    #[error("Failed to spawn `{argv}` (cwd {cwd}): {source}")]
    Spawn {
        /// The full argument vector, including any `confine` wrapper.
        argv: String,
        /// Working directory the spawn was attempted from.
        cwd: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// Tool execution timed out. Names the command and the budget so a
    /// timeout is never a bare "timed out" with no way to tell what or
    /// how long.
    #[error("`{command}` timed out after {secs}s")]
    Timeout {
        /// The command that exceeded the budget.
        command: String,
        /// The budget, in seconds.
        secs: u64,
    },
}

/// Workspace initialization errors.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// Failed to create or access workspace directory.
    #[error("Failed to initialize workspace at {0}: {1}")]
    Init(PathBuf, #[source] std::io::Error),
}

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Parsed successfully but values are invalid.
    #[error("Invalid config: {0}")]
    Invalid(String),

    /// I/O error reading config file.
    #[error("Config I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to parse TOML. The toml error carries line/column; the
    /// path names which file, which it does not.
    #[error("Failed to parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Safety layer errors.
#[derive(Debug, Error)]
pub enum SafetyError {
    /// Tool output contained a pattern matching a known secret format.
    #[error("Potential secret detected (pattern: {pattern_name})")]
    LeakDetected { pattern_name: String },
}

/// Secret loading errors.
#[derive(Debug, Error)]
pub enum SecretError {
    /// `CREDENTIALS_DIRECTORY` not set in environment.
    #[error("CREDENTIALS_DIRECTORY not set")]
    NoCredentialsDir,

    /// Secret file does not exist.
    #[error("Secret not found: {name}")]
    NotFound { name: String },

    /// I/O error reading secret file.
    #[error("Failed to read secret {name}: {source}")]
    Read {
        name: String,
        source: std::io::Error,
    },
}

/// Telegram channel errors.
#[derive(Clone, Debug, Error)]
pub enum TelegramError {
    /// Telegram Bot API returned `"ok": false`.
    #[error("Telegram API error ({error_code}): {description}")]
    Api {
        error_code: i32,
        description: String,
    },

    /// Failed to deserialize a Telegram API response body.
    #[error("Deserialize error: {0}")]
    Deserialize(String),

    /// HTTP request failed (timeout, DNS, connection reset, etc.).
    #[error("Network error: {0}")]
    Network(String),

    /// Session load/save failure.
    #[cfg(test)]
    #[error("Session error: {0}")]
    Session(String),
}

/// GitHub REST API errors.
#[derive(Clone, Debug, Error)]
pub enum GithubError {
    /// GitHub returned a non-2xx status.
    #[error("GitHub API error ({status}): {body}")]
    Api { status: u16, body: String },

    /// Failed to deserialize a GitHub API response body.
    #[error("Deserialize error: {0}")]
    Deserialize(String),

    /// HTTP request failed (timeout, DNS, connection reset, etc.).
    #[error("Network error: {0}")]
    Network(String),
}

/// Linear channel errors.
#[derive(Clone, Debug, Error)]
pub enum LinearError {
    /// GraphQL layer returned errors.
    #[error("Linear API error: {0}")]
    Api(String),

    /// Failed to deserialize a Linear API response body.
    #[error("Deserialize error: {0}")]
    Deserialize(String),

    /// HTTP request failed (timeout, DNS, non-2xx status, etc.).
    #[error("Network error: {0}")]
    Network(String),
}

/// Sandbox application errors.
///
/// Not wired into the top-level `Error` enum — sandbox failures are
/// handled at the call site in `main` via `warn!` (defense-in-depth,
/// not fatal) and never propagated through the agent loop.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// Failed to open a path for Landlock rule.
    #[error("Failed to open path {path}: {reason}")]
    OpenPath { path: String, reason: String },

    /// Failed to configure or apply Landlock ruleset.
    #[error("Landlock ruleset error: {0}")]
    Ruleset(String),
}

/// Session persistence errors.
#[derive(Debug, Error)]
pub enum SessionError {
    /// I/O error reading or writing session file.
    #[error("Session I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to parse session JSON.
    #[error("Failed to parse session: {0}")]
    Parse(#[source] serde_json::Error),

    /// Failed to serialize session to JSON.
    #[error("Failed to serialize session: {0}")]
    Serialize(#[source] serde_json::Error),
}
