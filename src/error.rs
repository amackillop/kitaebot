//! Error types for the agent.
//!
//! Models all failure modes explicitly as algebraic data types.
//! No generic "something failed" errors - each error explains what went wrong.

#![allow(dead_code)] // Types defined here will be used in later commits

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
