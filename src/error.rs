//! Error types for the agent.
//!
//! Models all failure modes explicitly as algebraic data types.
//! No generic "something failed" errors - each error explains what went wrong.

#![allow(dead_code)] // Types defined here will be used in later commits

use std::path::PathBuf;

use thiserror::Error;

/// Top-level agent error.
#[derive(Debug, Error)]
pub enum Error {
    /// Heartbeat execution error.
    #[error("Heartbeat error: {0}")]
    Heartbeat(#[from] HeartbeatError),

    /// Turn cancelled by client disconnect.
    #[error("Turn cancelled")]
    Cancelled,

    /// Maximum iterations reached without completion.
    ///
    /// The agent loop stopped after hitting the iteration limit to prevent
    /// infinite loops and runaway API costs.
    #[error("Maximum iterations reached without completion")]
    MaxIterationsReached,

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

/// LLM provider errors.
#[derive(Debug, Clone, Error)]
pub enum ProviderError {
    /// Authentication failed (invalid API key, etc.).
    #[error("Authentication failed")]
    Authentication,

    /// Invalid response from provider (malformed JSON, missing fields, etc.).
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Network error (connection failed, timeout, etc.).
    #[error("Network error: {0}")]
    Network(String),

    /// Rate limited by the provider.
    #[error("Rate limited")]
    RateLimited,
}

/// Tool execution errors.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Tool execution blocked by policy.
    #[error("Tool blocked: {0}")]
    Blocked(String),

    /// Tool execution failed.
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),

    /// Invalid arguments passed to tool.
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),

    /// Tool not found in registry.
    #[error("Tool not found: {0}")]
    NotFound(String),

    /// Tool execution timed out.
    #[error("Tool execution timed out")]
    Timeout,
}

/// Workspace initialization errors.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// Failed to create or access workspace directory.
    #[error("Failed to initialize workspace at {0}: {1}")]
    Init(PathBuf, #[source] std::io::Error),
}

/// Heartbeat execution errors.
#[derive(Debug, Error)]
pub enum HeartbeatError {
    /// Failed to read HEARTBEAT.md.
    #[error("Failed to read tasks: {0}")]
    ReadTasks(#[source] std::io::Error),

    /// Failed to append to HISTORY.md.
    #[error("Failed to write history: {0}")]
    WriteHistory(#[source] std::io::Error),
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

    /// Failed to parse TOML.
    #[error("Config parse error: {0}")]
    Parse(String),
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

    /// HTTP request failed (timeout, DNS, connection reset, etc.).
    #[error("Network error: {0}")]
    Network(String),

    /// Session load/save failure.
    #[error("Session error: {0}")]
    Session(String),
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
