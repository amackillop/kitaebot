//! Error types for the agent.
//!
//! Models all failure modes explicitly as algebraic data types.
//! No generic "something failed" errors - each error explains what went wrong.

use std::path::PathBuf;

use thiserror::Error;

use crate::tools::mcp::McpError;
use crate::tools::truncate_output;

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
    /// infinite loops and runaway API costs. `report` carries the model's
    /// own state-at-exit summary from the final no-tools squeeze — what
    /// was attempted, what blocks, and any branch or commits left behind —
    /// so the failure a channel posts is recoverable by a successor.
    #[error("Maximum iterations reached without completion. State at exit:\n{report}")]
    MaxIterationsReached { report: String },

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

    /// `SQLite` failure in the engine's store.
    ///
    /// `#[from]` is safe here where it is not on [`ToolError`]: within
    /// this type a `rusqlite::Error` can only mean the store failed.
    #[error("Storage error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A blocking-pool task died before returning: a panic in the
    /// closure (the payload rides in the `JoinError`) or runtime
    /// shutdown.
    #[error("blocking task failed: {0}")]
    Join(#[source] tokio::task::JoinError),

    /// A filesystem operation on the engine's own files failed. Same
    /// shape as [`ToolError::Io`]; see there for why `operation` is a
    /// `&'static str`.
    #[error("failed to {operation} {}: {source}", path.display())]
    Io {
        /// What was attempted, e.g. "create" or "snapshot".
        operation: &'static str,
        /// The path acted on.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// Stored engine state that cannot be interpreted: a role column
    /// with an unknown value, a tool name the grammar rejects. The row
    /// is named; there is no underlying error to keep.
    #[error("Storage error: {0}")]
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
    /// deterministic at non-zero temperature (ours or any endpoint
    /// default): the request is fine, the draw was not, and a fresh
    /// draw is the whole remedy.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::EmptyResponse
                | Self::MalformedToolCall { .. }
                | Self::Network(_)
                | Self::RateLimited
                | Self::ServerError(_)
        )
    }
}

/// A string that cannot be used as a tool name.
///
/// Carries the rejected string: knowing a name was invalid is useless
/// without knowing which one, and the offender is often mangled in a
/// way that identifies the provider bug that produced it.
#[derive(Debug, Clone, Error)]
#[error("tool name {0:?} must match ^[a-zA-Z0-9_-]+$")]
pub struct InvalidToolName(pub String);

/// What every attempt in a [`ToolError::EditLoop`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditFutility {
    /// Each attempt returned the same match failure.
    FailedIdentically,
    /// Each attempt matched but left the file byte-identical.
    NoChange,
}

impl std::fmt::Display for EditFutility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FailedIdentically => f.write_str("failed identically"),
            Self::NoChange => f.write_str("produced no change"),
        }
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

    /// The turn was cancelled while the tool was in flight.
    ///
    /// Not a failure: the caller went away. Distinct from
    /// [`Self::Timeout`], which is the tool outrunning its budget.
    #[error("cancelled")]
    Cancelled,

    /// A subprocess ran to completion and exited non-zero.
    ///
    /// Distinct from [`Self::Spawn`], which never got that far. Carries
    /// the rendered output because the model reads it to decide what to
    /// do next; the log takes only the summary, since a whole command's
    /// stdout in the error tee evicts the rest of the duty's window
    /// (spec 24).
    #[error("Execution failed: {output}")]
    CommandFailed {
        /// The command as invoked.
        command: String,
        /// Its exit status.
        exit_code: i32,
        /// `$ command`, stdout, stderr, `Exit code: N`.
        output: String,
    },

    /// The same `file_edit` payload was futile several times running:
    /// the model is looping on a stale view of the file. No `#[source]`
    /// carrying the repeated result — the model already received it
    /// verbatim twice; the payload here is the instruction to stop.
    #[error(
        "file_edit on {path} {outcome} {attempts} times running; \
         re-read the file to re-anchor before editing it again"
    )]
    EditLoop {
        /// The workspace-relative path the edits targeted.
        path: String,
        /// Occurrences of this payload in the path's recent window.
        attempts: usize,
        /// What every attempt produced.
        outcome: EditFutility,
    },

    /// FTS5 rejected an `lcm_grep` search pattern as query syntax and
    /// the phrase fallback could not apply. Carries the pattern
    /// verbatim and guidance, since the sqlite message alone ("no such
    /// column: lane") names neither the operation nor the input.
    #[error(
        "FTS5 failed to parse search pattern {pattern:?}: {source}. \
         Hyphens and colons are query syntax in fts mode; quote each \
         literal term (\"security-lane\") or use mode=\"regex\"."
    )]
    FtsQuery {
        /// The pattern exactly as supplied.
        pattern: String,
        /// The underlying FTS5 parse failure.
        #[source]
        source: rusqlite::Error,
    },

    /// A GitHub API call made by a tool failed.
    ///
    /// Transparent: [`GithubError`] already distinguishes a non-2xx
    /// status (with its body) from a transport failure and from a
    /// deserialize failure, and already names the service. Wrapping it
    /// in another layer of prose would only bury what it says.
    #[error(transparent)]
    Github(#[from] GithubError),

    /// A Linear API call made by a tool failed.
    #[error(transparent)]
    Linear(#[from] LinearError),

    /// An HTTP request a tool made itself failed in transport: send,
    /// connect, or reading the body. `reqwest`'s own error says which.
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    #[error("fetch {url}: {source}")]
    Http {
        /// The URL requested.
        url: String,
        /// The transport failure.
        #[source]
        source: reqwest::Error,
    },

    /// An HTTP request a tool made itself got a non-2xx response.
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    #[error("HTTP {status} from {url}")]
    HttpStatus {
        /// The URL requested.
        url: String,
        /// The response status code.
        status: u16,
    },

    /// Invalid arguments passed to tool.
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),

    /// A blocking-pool task died before returning; see
    /// [`EngineError::Join`].
    #[error("blocking task failed: {0}")]
    Join(#[source] tokio::task::JoinError),

    /// A filesystem operation failed on a named path.
    ///
    /// `operation` is a verb phrase describing what was attempted, and
    /// is `&'static str` rather than an enum on purpose: nothing
    /// branches on it, so an enum would grow a variant per syscall to
    /// buy nothing. Being compile-time constant keeps it from becoming
    /// a runtime-formatted claim about what ran.
    #[error("failed to {operation} {}: {source}", path.display())]
    Io {
        /// What was attempted, e.g. "read" or "create the askpass dir".
        operation: &'static str,
        /// The path acted on, as the caller named it.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A call to an MCP server's tool failed.
    ///
    /// Not transparent: [`McpError`] names the protocol failure but not
    /// which registered tool was being called, and one server can back
    /// many of them.
    #[error("{tool}: {source}")]
    Mcp {
        /// The registered (namespaced) tool name.
        tool: String,
        /// The protocol-level failure.
        #[source]
        source: McpError,
    },

    /// Tool not found in registry.
    #[error("Tool not found: {0}")]
    NotFound(String),

    /// A precondition or invariant the tool checks itself did not
    /// hold: an edit pattern that matches zero or many places, a
    /// missing working directory, a rate limit, a rewrite that
    /// changed the tree. The message is the whole story -- there is
    /// no underlying error to keep. Messages must lead with the
    /// operation and failure mode: the log rendering keeps only the
    /// first 500 bytes (see `log_summary`).
    #[error("precondition failed: {0}")]
    Precondition(String),

    /// A `SQLite`-backed store a tool fronts failed.
    ///
    /// No `#[from]`: a `rusqlite::Error` inside `ToolError` is the LCM
    /// query store in one place and the review ledger in another, so
    /// each site names which through `context`.
    #[error("{context}: {source}")]
    Sqlite {
        /// Which store, e.g. `lcm` or `review_log`.
        context: &'static str,
        /// The underlying `SQLite` failure.
        #[source]
        source: rusqlite::Error,
    },

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

    /// A sub-agent's turn ended in error.
    ///
    /// Boxed because [`Error`] already contains a `ToolError`, so
    /// storing one inline would make the type recursive.
    #[error("sub-agent failed: {source}")]
    SubAgent {
        /// Whatever ended the sub-agent's turn.
        #[source]
        source: Box<Error>,
    },

    /// A Telegram API call made by a tool failed.
    #[error(transparent)]
    Telegram(#[from] TelegramError),

    /// The web-search provider call failed.
    ///
    /// Not transparent: [`ProviderError`]'s own rendering says
    /// "Provider", which the model would read as its main loop failing
    /// rather than the search it just requested.
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    #[error("search request failed: {0}")]
    WebSearch(#[source] ProviderError),

    /// Tool execution timed out. Names the command and the budget so a
    /// timeout is never a bare "timed out" with no way to tell what or
    /// how long, and carries what the child was doing when it died.
    #[error("`{command}` timed out after {secs}s{evidence}")]
    Timeout {
        /// The command that exceeded the budget.
        command: String,
        /// The budget, in seconds.
        secs: u64,
        /// What the kill would otherwise destroy; empty for timeouts
        /// with no subprocess (HTTP fetches).
        evidence: TimeoutEvidence,
    },
}

/// Diagnostics captured when a subprocess is killed at its budget:
/// the output it had written (a stall's last words — a lock wait, a
/// hung test name) and the cgroup pressure at kill time (the #74
/// stall hypothesis is silent memory.high throttling, which no
/// kernel log ever names).
#[derive(Debug, Default)]
pub struct TimeoutEvidence {
    /// Bounded tail of the child's output at kill time.
    pub output_tail: String,
    /// Cgroup snapshot: memory.current, memory.events, PSI.
    pub pressure: String,
}

impl std::fmt::Display for TimeoutEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.pressure.is_empty() {
            write!(f, "\ncgroup at kill: {}", self.pressure)?;
        }
        if !self.output_tail.is_empty() {
            write!(f, "\nlast output:\n{}", self.output_tail)?;
        }
        Ok(())
    }
}

impl ToolError {
    /// Compact rendering for the log and, through it, the error tee.
    ///
    /// Distinct from [`Display`], which the model reads and must stay
    /// complete. The tee has no per-entry cap and the self-analysis
    /// duty truncates its whole errors section, so one entry carrying a
    /// command's full output evicts every other incident in that window
    /// (spec 24, "What belongs in the error tee").
    ///
    /// Matched exhaustively: whether a variant's payload is safe to log
    /// whole is a question each new one has to answer.
    pub fn log_summary(&self) -> String {
        match self {
            Self::CommandFailed {
                command,
                exit_code,
                output: _,
            } => format!("`{command}` exited {exit_code}"),
            // API error bodies are the diagnostic and are bounded by
            // what the service returns, so they log whole.
            Self::Blocked { .. }
            | Self::Cancelled
            | Self::EditLoop { .. }
            | Self::FtsQuery { .. }
            | Self::Github(_)
            | Self::Http { .. }
            | Self::HttpStatus { .. }
            | Self::InvalidArguments(_)
            | Self::Io { .. }
            | Self::Join(_)
            | Self::Linear(_)
            | Self::Mcp { .. }
            | Self::NotFound(_)
            | Self::Sqlite { .. }
            | Self::Spawn { .. }
            | Self::SubAgent { .. }
            | Self::Telegram(_)
            | Self::Timeout { .. }
            | Self::WebSearch(_) => self.to_string(),
            // The message is the whole story, but it is also the
            // model-facing payload: a file_edit no-match embeds a
            // content excerpt, and other tools' messages carry
            // arbitrary length. The model reads the whole thing in the
            // tool result; the tee takes a bounded head, which keeps
            // the operation and the failure mode and drops the
            // payload (spec 24, entry size is correctness).
            Self::Precondition(_) => {
                truncate_output(&self.to_string(), PRECONDITION_LOG_MAX).into_owned()
            }
        }
    }
}

/// Byte cap on a `Precondition` failure's log rendering. Generous
/// against the structured variants (a path, a command, a URL) and
/// cuts the payload-carrying ones (`file_edit`'s content excerpt).
const PRECONDITION_LOG_MAX: usize = 500;

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
        /// Seconds Telegram asked us to wait before retrying (429 only).
        retry_after: Option<u64>,
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

impl TelegramError {
    /// Seconds to wait before retrying, when Telegram's 429 response
    /// included `parameters.retry_after`. `None` for non-429 errors or
    /// when the field was absent.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
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

    /// GitHub rate limited the request (primary or secondary; 429, or
    /// 403 with a rate-limit signature).
    #[error("GitHub rate limited ({status}), retry after {retry_after_secs:?}s: {body}")]
    RateLimited {
        status: u16,
        /// From the `Retry-After` header, when GitHub sent one.
        retry_after_secs: Option<u64>,
        body: String,
    },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn command_failed() -> ToolError {
        ToolError::CommandFailed {
            command: "git push origin HEAD".to_string(),
            exit_code: 1,
            output: "$ git push origin HEAD\nrejected: non-fast-forward\nExit code: 1".to_string(),
        }
    }

    /// The model reads Display and acts on it, so the whole rendered
    /// output has to survive the split intact.
    #[test]
    fn display_carries_the_whole_output_to_the_model() {
        assert_eq!(
            command_failed().to_string(),
            "Execution failed: $ git push origin HEAD\nrejected: non-fast-forward\nExit code: 1"
        );
    }

    /// The tee has no per-entry cap and the duty truncates its whole
    /// errors section, so an entry must not carry command output.
    #[test]
    fn log_summary_names_the_command_without_its_output() {
        let summary = command_failed().log_summary();
        assert_eq!(summary, "`git push origin HEAD` exited 1");
        assert!(!summary.contains("non-fast-forward"));
    }

    /// Everything else is already compact, so the summary is Display.
    #[test]
    fn other_variants_summarize_as_themselves() {
        let e = ToolError::NotFound("nosuchtool".to_string());
        assert_eq!(e.log_summary(), e.to_string());
    }

    /// A precondition's message is the model-facing payload (a
    /// `file_edit` no-match embeds a content excerpt); the tee takes a
    /// bounded head of it. The head keeps the operation and failure
    /// mode, which lead the rendering.
    #[test]
    fn precondition_log_summary_is_capped() {
        let e = ToolError::Precondition(format!(
            "no match found for old_string in src/duty/mod.rs; the file may \
             have changed since you read it. Nearest candidate:\n{}",
            "filler\n".repeat(200)
        ));
        let summary = e.log_summary();
        assert!(
            summary.len() <= PRECONDITION_LOG_MAX + 64,
            "{}",
            summary.len()
        );
        assert!(
            summary.starts_with("precondition failed: no match found"),
            "{summary}"
        );
        assert!(summary.contains("[truncated"), "{summary}");
    }

    /// Short precondition messages — the common case — log whole.
    #[test]
    fn short_precondition_logs_whole() {
        let e = ToolError::Precondition("ls-remote owner/repo: no HEAD in output".into());
        assert_eq!(e.log_summary(), e.to_string());
    }
}
