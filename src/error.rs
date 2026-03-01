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
    /// Maximum iterations reached without completion.
    ///
    /// The agent loop stopped after hitting the iteration limit to prevent
    /// infinite loops and runaway API costs.
    #[error("Maximum iterations reached without completion")]
    MaxIterationsReached,

    /// LLM provider error (network, auth, etc.).
    #[error("Provider error: {0}")]
    Provider(#[from] ProviderError),

    /// Tool execution error.
    #[error("Tool error: {0}")]
    Tool(#[from] ToolError),

    /// Heartbeat execution error.
    #[error("Heartbeat error: {0}")]
    Heartbeat(#[from] HeartbeatError),

    /// Safety layer blocked the output.
    #[error("Safety error: {0}")]
    Safety(#[from] SafetyError),
}

/// LLM provider errors.
#[derive(Debug, Clone, Error)]
pub enum ProviderError {
    /// Network error (connection failed, timeout, etc.).
    #[error("Network error: {0}")]
    Network(String),

    /// Authentication failed (invalid API key, etc.).
    #[error("Authentication failed")]
    Authentication,

    /// Invalid response from provider (malformed JSON, missing fields, etc.).
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Rate limited by the provider.
    #[error("Rate limited")]
    RateLimited,
}

/// Tool execution errors.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Tool not found in registry.
    #[error("Tool not found: {0}")]
    NotFound(String),

    /// Invalid arguments passed to tool.
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),

    /// Tool execution failed.
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),

    /// Tool execution timed out.
    #[error("Tool execution timed out")]
    Timeout,

    /// Tool execution blocked by policy.
    #[error("Tool blocked: {0}")]
    Blocked(String),
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

    /// Failed to load or save the heartbeat session.
    #[error("Session error: {0}")]
    Session(String),

    /// Failed to append to HISTORY.md.
    #[error("Failed to write history: {0}")]
    WriteHistory(#[source] std::io::Error),
}

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// I/O error reading config file.
    #[error("Config I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to parse TOML.
    #[error("Config parse error: {0}")]
    Parse(String),

    /// Parsed successfully but values are invalid.
    #[error("Invalid config: {0}")]
    Invalid(String),
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
