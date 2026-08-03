//! TOML configuration for the agent.
//!
//! Loads `config.toml` from the workspace root. Missing file produces
//! defaults; malformed file is a hard error. Unknown fields are rejected
//! to catch typos early.

use std::path::Path;

use serde::Deserialize;

use crate::error::ConfigError;

/// Top-level configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub github: GithubConfig,
    #[serde(default)]
    pub duties: DutiesConfig,
    #[serde(default)]
    pub linear: LinearConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub socket: SocketConfig,
    #[serde(default)]
    pub sub_agents: SubAgentsConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
}

/// Agent loop settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub max_iterations: usize,
}

/// Sub-agent settings (spec 19).
/// MCP servers (spec 22). No `enabled` flag: an empty `servers` map
/// means no MCP anywhere — no children, no tools, no cost.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Spawn + handshake + `tools/list` budget per server, seconds.
    pub startup_timeout_secs: u64,
    /// Per-call budget, seconds.
    pub call_timeout_secs: u64,
    /// One table per server: `[mcp.servers.<name>]`.
    pub servers: std::collections::BTreeMap<String, McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            startup_timeout_secs: 30,
            call_timeout_secs: 60,
            servers: std::collections::BTreeMap::new(),
        }
    }
}

/// One MCP server child (spec 22).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Executable to spawn, resolved via `PATH`.
    pub command: String,
    /// Argument vector.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables (literals).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Env var -> credential name, loaded via `LoadCredential`
    /// (spec 13). Secrets never appear in config.toml.
    #[serde(default)]
    pub env_credentials: std::collections::BTreeMap<String, String>,
    /// Allowlist of advertised tool names to register; unset = all.
    /// The schema-size control for servers advertising dozens.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Admit this server's tools to the read-only sub-agent sets
    /// (explore, reviewer) — the operator asserting the server has no
    /// side effects.
    #[serde(default)]
    pub explore: bool,
}

/// Self-review pipeline (spec 23).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    /// Master switch: opens the review ledger and records reviewer
    /// findings. Gates are prompted, not enforced; disabling this
    /// only stops the recording.
    pub enabled: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubAgentsConfig {
    /// Tool-loop iteration cap per sub-agent turn. Lower than the
    /// parent's: a delegated task is narrower than a conversation.
    pub max_iterations: usize,
}

/// LLM provider settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    /// `OpenAI`-compatible API to use.
    pub api: Api,
    pub model: String,
    /// Output budget per request. Reasoning tokens count against it on
    /// `OpenRouter`, so a small value starves reasoning models into
    /// empty responses (`finish_reason = "length"`).
    pub max_tokens: u32,
    pub temperature: f32,
    /// Per-role model overrides. Unset roles use `model`.
    pub model_overrides: ModelOverrides,
}

/// Per-role model overrides. Each role falls back to `provider.model`
/// when unset. Roles are the structural seams where the difficulty of
/// the work is already known: sub-agent types, compaction summaries,
/// and memory distillation. Duty turns run on `provider.model` — the
/// duty scheduler replaced the heartbeat's override with the root
/// model (spec 24).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOverrides {
    /// Model for `explore` sub-agents (read-only research).
    pub explore: Option<String>,
    /// Model for `worker` sub-agents (delegated implementation).
    pub worker: Option<String>,
    /// Model for `reviewer` sub-agents (self-review gates, spec 23).
    pub reviewer: Option<String>,
    /// Model for context-compaction summaries.
    pub summarizer: Option<String>,
    /// Model for memory distillation turns.
    pub memory: Option<String>,
}

/// `OpenAI`-compatible chat completions API.
///
/// Known provider names map to their endpoint URLs; a full http(s)
/// URL selects a custom endpoint (self-hosted gateways, test fixture
/// servers). Invalid values are rejected at config parse time.
#[derive(Debug, Default, Clone)]
pub enum Api {
    #[default]
    OpenRouter,
    OpenAi,
    Groq,
    Together,
    Mistral,
    Custom(String),
}

impl Api {
    /// Endpoint URL for the chat completions API.
    pub fn endpoint(&self) -> &str {
        match self {
            Self::OpenRouter => "https://openrouter.ai/api/v1/chat/completions",
            Self::OpenAi => "https://api.openai.com/v1/chat/completions",
            Self::Groq => "https://api.groq.com/openai/v1/chat/completions",
            Self::Together => "https://api.together.xyz/v1/chat/completions",
            Self::Mistral => "https://api.mistral.ai/v1/chat/completions",
            Self::Custom(url) => url,
        }
    }
}

impl std::str::FromStr for Api {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "groq" => Ok(Self::Groq),
            "mistral" => Ok(Self::Mistral),
            "openai" => Ok(Self::OpenAi),
            "openrouter" => Ok(Self::OpenRouter),
            "together" => Ok(Self::Together),
            url if url.starts_with("http://") || url.starts_with("https://") => {
                Ok(Self::Custom(url.to_string()))
            }
            other => Err(format!(
                "unknown api {other:?}: expected a provider name or an http(s) URL"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Api {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Tool-level settings.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    /// Tool names to exclude from the agent's toolbox.
    pub disabled: Vec<String>,
    pub exec: ExecConfig,
    pub web_fetch: WebFetchConfig,
    pub web_search: WebSearchConfig,
}

/// Settings for the `exec` tool.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecConfig {
    pub timeout_secs: u64,
    /// Wrap each command in a bubblewrap sandbox that masks the
    /// daemon-owned paths (spec 15). Off until the VM smoke confirms
    /// the reconstructed view does not break builds or the devshell.
    pub sandbox: bool,
}

/// Settings for the `web_fetch` tool.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebFetchConfig {
    pub timeout_secs: u64,
    pub max_response_bytes: usize,
}

/// Settings for the `web_search` tool.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    pub model: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
}

/// Duty scheduler settings (spec 24).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DutiesConfig {
    /// Schedule for the distillation duty (token gate still applies).
    pub distill: ScheduleConfig,
    /// Operator-defined prompt duties: recurring watch-tasks authored
    /// in config (spec 24). `[[duties.prompt]]` tables.
    pub prompt: Vec<PromptDutyConfig>,
    /// Schedule for the build-warm duty. Registered only when some
    /// repo in `git.repositories` configures a check command.
    pub warm: ScheduleConfig,
}

/// One operator-defined prompt duty.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptDutyConfig {
    /// Duty name: a single token, unique across all duties.
    pub name: String,
    /// Schedule (`every` or `daily`), flattened alongside.
    #[serde(flatten)]
    pub schedule: ScheduleConfig,
    /// Repository the duty works on (`owner/repo`). Must be listed in
    /// `git.repositories`; the turn runs on its work session.
    pub repo: String,
    /// Optional mechanical gate. `"new-commits"` keeps a last-reviewed
    /// SHA cursor and dispatches only on new commits; unset runs
    /// unconditionally on schedule. Requires `github.enabled`.
    #[serde(default)]
    pub gate: Option<String>,
    /// The turn's prompt text.
    pub prompt: String,
}

/// One duty's schedule: exactly one of `every` (interval, e.g.
/// `"30m"`) or `daily` (UTC time of day, e.g. `"06:00"`).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub every: Option<String>,
    pub daily: Option<String>,
}

impl ScheduleConfig {
    /// Parse into a [`Schedule`]. Called at validation, so later
    /// callers may expect success.
    pub fn parse(&self) -> Result<crate::duty::schedule::Schedule, String> {
        use crate::duty::schedule::{Schedule, parse_daily, parse_every};
        match (&self.every, &self.daily) {
            (Some(e), None) => Ok(Schedule::Every(parse_every(e)?)),
            (None, Some(d)) => Ok(Schedule::Daily(parse_daily(d)?)),
            (None, None) => Err("schedule needs `every` or `daily`".into()),
            (Some(_), Some(_)) => Err("schedule takes `every` or `daily`, not both".into()),
        }
    }
}

/// Memory subsystem settings (spec 21).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// Byte cap on the `memory/MEMORY.md` index injected into the
    /// system prompt each root turn. Content beyond it is truncated.
    /// Must be > 0.
    pub index_cap_bytes: usize,
    /// Undistilled tokens, summed across all sessions, that must
    /// accumulate before the distill duty folds them into memory. A
    /// mechanical gate: no LLM runs until the total is reached. Sized
    /// below the distiller's window so a triggered pass fits in one
    /// turn. Must be > 0.
    pub distill_threshold_tokens: u64,
}

/// Telegram channel settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramConfig {
    /// Base URL of the Telegram Bot API. Tests point this at a local
    /// fixture server; `mock-network` builds refuse non-loopback hosts.
    pub api_base: String,
    /// Enable the Telegram channel. Defaults to `false` so the daemon
    /// can start without Telegram credentials.
    pub enabled: bool,
    /// Telegram chat ID to accept messages from. Must be set when enabled.
    pub chat_id: i64,
    /// Long-poll timeout in seconds sent to `getUpdates`.
    pub poll_timeout_secs: u64,
}

/// Unix domain socket channel settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SocketConfig {
    /// Path to the Unix domain socket.
    pub path: String,
    /// Peer uids (`SO_PEERCRED`) the socket serves. Default root only:
    /// the operator reaches the VM over SSH; the daemon's own same-uid
    /// children must not drive it.
    pub allowed_uids: Vec<u32>,
}

/// Context window management settings.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Which context engine implementation to use.
    pub engine: EngineKind,
    /// Maximum context window size in tokens.
    pub max_tokens: u32,
    /// Percentage of `max_tokens` at which compaction triggers (1..=100).
    /// Used by the flat engine; ignored by LCM (see [`LcmConfig`]).
    pub budget_percent: u8,
    /// Tool result content above this many estimated tokens gets
    /// size-limited by the engine: LCM externalizes it to disk with a
    /// mechanical excerpt, flat truncates it tail-biased. Much lower
    /// than `lcm.large_file_threshold` because tool output arrives on
    /// every turn. Must be > 0.
    pub tool_output_tokens: u32,
    /// LCM-specific tuning. Ignored when `engine = "flat"`.
    pub lcm: LcmConfig,
}

/// LCM compaction tuning. The defaults match the constants we shipped
/// hardcoded; these knobs exist mostly so tests and benchmarks can
/// trip thresholds without pumping hundreds of thousands of tokens.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LcmConfig {
    /// Newest N message context items are never compacted. Must be > 0.
    pub fresh_tail_count: u32,
    /// Maximum tokens per leaf or condensed chunk. Runs that exceed
    /// this are skipped until sub-chunking lands.
    pub leaf_chunk_tokens: u32,
    /// Minimum children to form a condensed summary. Must be >= 2.
    pub min_condensed_fanout: u32,
    /// Percent of `max_tokens` at which background compaction starts.
    /// Must be 1..=100 and strictly less than `hard_budget_percent`.
    pub soft_budget_percent: u8,
    /// Percent of `max_tokens` at which compaction blocks the actor.
    /// Must be 1..=100 and strictly greater than `soft_budget_percent`.
    pub hard_budget_percent: u8,
    /// Message content above this many estimated tokens is stored on
    /// disk and replaced by a `<file>` reference at ingest. Must be > 0.
    pub large_file_threshold: u32,
    /// Token bound on LLM-generated exploration summaries for
    /// externalized plain-text payloads. Must be > 0.
    pub large_file_summary_tokens: u32,
}

/// Selects the [`ContextEngine`](crate::context::ContextEngine)
/// implementation. The flat session keeps each conversation in a
/// per-name JSON file; LCM stores everything in `SQLite` with a DAG
/// of summaries on top of raw messages.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    #[default]
    Flat,
    Lcm,
}

/// Git settings.
///
/// Identity (user.name, user.email) is managed at the system level via
/// NixOS `programs.git`. This section holds agent-level settings only.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfig {
    /// Base URL clones and fetches resolve `owner/repo` against.
    /// Tests point this at `file://` fixture repos.
    pub clone_base: String,
    /// Enable the git tools (clone, push, commit). Defaults to `false`
    /// so the daemon can start without a GitHub token.
    pub enabled: bool,
    /// `Co-authored-by` trailers appended to commit messages.
    /// Each entry is `"Name <email>"`.
    pub co_authors: Vec<String>,
    /// Per-repository settings, keyed by exact `owner/repo`, matched
    /// case-insensitively. Listing a repo is the trust grant: its
    /// `.envrc` gets `direnv allow` on clone. Unlisted repos clone
    /// fine but run without a devshell.
    pub repositories: std::collections::BTreeMap<String, RepoConfig>,
}

/// Settings for one `[git.repositories."owner/repo"]` entry.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    /// The repo's check command — what its pre-commit hook runs. The
    /// daemon runs it ahead of need (spec 03 Build Warm: on checkout
    /// preparation and on the warm duty) so `git_commit` never meets
    /// a cold store. Exact `owner/repo` entries only.
    pub check: Option<String>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            clone_base: "https://github.com".into(),
            enabled: false,
            co_authors: Vec::new(),
            repositories: std::collections::BTreeMap::new(),
        }
    }
}

impl GitConfig {
    /// The trust list: every key of `repositories`.
    pub fn trusted_repos(&self) -> Vec<String> {
        self.repositories.keys().cloned().collect()
    }

    /// Repos with a build-warm command configured.
    pub fn warm_commands(&self) -> std::collections::BTreeMap<String, String> {
        self.repositories
            .iter()
            .filter_map(|(nwo, repo)| repo.check.clone().map(|check| (nwo.clone(), check)))
            .collect()
    }
}

/// GitHub integration settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    /// Base URL of the GitHub REST API. Tests point this at a local
    /// fixture server; `mock-network` builds refuse non-loopback hosts.
    pub api_base: String,
    /// Enable the GitHub integration. Defaults to `false` so the daemon
    /// can start without a GitHub token.
    pub enabled: bool,
    /// Seconds between PR polling cycles. Defaults to 300 (5 minutes).
    pub poll_interval_secs: u64,
    /// GitHub username of the bot owner. Required when enabled.
    /// The owner is always allowed to interact with the bot.
    pub owner: String,
    /// Additional GitHub usernames allowed to interact with the bot.
    pub trusted_users: Vec<String>,
    /// GitHub App bot logins whose PR feedback the bot acts on. Matched
    /// case-insensitively; a trailing `[bot]` suffix is ignored.
    pub trusted_bots: Vec<String>,
}

/// Linear channel settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinearConfig {
    /// Base URL of the Linear API. Tests point this at a local
    /// fixture server; `mock-network` builds refuse non-loopback hosts.
    pub api_base: String,
    /// Enable the Linear channel. Defaults to `false` so the daemon
    /// can start without a Linear API key.
    pub enabled: bool,
    /// Seconds between issue polling cycles.
    pub poll_interval_secs: u64,
    /// Email addresses allowed to interact with the bot, matched
    /// case-insensitively. Must be non-empty when enabled.
    pub trusted_users: Vec<String>,
}

// --- Default impls ---

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 100,
        }
    }
}

impl Default for SubAgentsConfig {
    fn default() -> Self {
        Self { max_iterations: 30 }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            engine: EngineKind::default(),
            max_tokens: 200_000,
            budget_percent: 80,
            tool_output_tokens: 4096,
            lcm: LcmConfig::default(),
        }
    }
}

impl Default for LcmConfig {
    fn default() -> Self {
        Self {
            fresh_tail_count: 32,
            leaf_chunk_tokens: 20_000,
            min_condensed_fanout: 2,
            soft_budget_percent: 70,
            hard_budget_percent: 90,
            large_file_threshold: 25_000,
            large_file_summary_tokens: 400,
        }
    }
}

impl ContextConfig {
    /// Section-local invariants. The cross-section check against
    /// `provider.max_tokens` (output reserve) lives in
    /// [`Config::validate`].
    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context max_tokens must be > 0".into(),
            ));
        }
        if self.budget_percent == 0 || self.budget_percent > 100 {
            return Err(ConfigError::Invalid(
                "context budget_percent must be 1..=100".into(),
            ));
        }
        if self.tool_output_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context tool_output_tokens must be > 0".into(),
            ));
        }
        self.lcm.validate()
    }
}

impl LcmConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.fresh_tail_count == 0 {
            return Err(ConfigError::Invalid(
                "context.lcm fresh_tail_count must be > 0".into(),
            ));
        }
        if self.leaf_chunk_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context.lcm leaf_chunk_tokens must be > 0".into(),
            ));
        }
        if self.min_condensed_fanout < 2 {
            return Err(ConfigError::Invalid(
                "context.lcm min_condensed_fanout must be >= 2".into(),
            ));
        }
        if self.soft_budget_percent == 0 || self.soft_budget_percent > 100 {
            return Err(ConfigError::Invalid(
                "context.lcm soft_budget_percent must be 1..=100".into(),
            ));
        }
        if self.hard_budget_percent == 0 || self.hard_budget_percent > 100 {
            return Err(ConfigError::Invalid(
                "context.lcm hard_budget_percent must be 1..=100".into(),
            ));
        }
        if self.soft_budget_percent >= self.hard_budget_percent {
            return Err(ConfigError::Invalid(
                "context.lcm soft_budget_percent must be < hard_budget_percent".into(),
            ));
        }
        if self.large_file_threshold == 0 {
            return Err(ConfigError::Invalid(
                "context.lcm large_file_threshold must be > 0".into(),
            ));
        }
        if self.large_file_summary_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context.lcm large_file_summary_tokens must be > 0".into(),
            ));
        }
        Ok(())
    }
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 600,
            sandbox: false,
        }
    }
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.github.com".into(),
            enabled: false,
            poll_interval_secs: 300,
            owner: String::new(),
            trusted_users: Vec::new(),
            trusted_bots: Vec::new(),
        }
    }
}

impl Default for DutiesConfig {
    fn default() -> Self {
        Self {
            distill: ScheduleConfig {
                every: Some("1h".into()),
                daily: None,
            },
            prompt: Vec::new(),
            warm: ScheduleConfig {
                every: Some("24h".into()),
                daily: None,
            },
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            index_cap_bytes: 8192,
            distill_threshold_tokens: 40_000,
        }
    }
}

impl Default for LinearConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.linear.app".into(),
            enabled: false,
            poll_interval_secs: 120,
            trusted_users: Vec::new(),
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api: Api::default(),
            model: "arcee-ai/trinity-large-preview:free".to_string(),
            max_tokens: 32_768,
            temperature: 0.7,
            model_overrides: ModelOverrides::default(),
        }
    }
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            path: "/run/kitaebot/chat.sock".to_string(),
            allowed_uids: vec![0],
        }
    }
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.telegram.org".into(),
            enabled: false,
            chat_id: 0,
            poll_timeout_secs: 30,
        }
    }
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            max_response_bytes: 512 * 1024,
        }
    }
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            model: "perplexity/sonar".to_string(),
            max_tokens: 1024,
            timeout_secs: 30,
        }
    }
}

// --- Loading & validation ---

impl Config {
    /// Load configuration from `config.toml` in the given workspace directory.
    ///
    /// Missing file produces defaults. Any I/O or parse error is propagated.
    pub fn load(workspace: &Path) -> Result<Self, ConfigError> {
        let path = workspace.join(crate::workspace::CONFIG_FILE);

        let contents = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(ConfigError::Io(e)),
        };

        let config: Self =
            toml::from_str(&contents).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Context config with the provider's output budget reserved.
    ///
    /// The provider can generate up to `provider.max_tokens` on top of
    /// the prompt, so compaction thresholds must apply to the window
    /// minus that reserve. Otherwise a prompt sitting just under the
    /// hard threshold can still overflow the window mid-generation.
    pub fn effective_context(&self) -> ContextConfig {
        ContextConfig {
            max_tokens: self.context.max_tokens - self.provider.max_tokens,
            ..self.context
        }
    }

    /// Validate invariants that serde alone cannot enforce.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.agent.max_iterations == 0 {
            return Err(ConfigError::Invalid("max_iterations must be > 0".into()));
        }
        if self.sub_agents.max_iterations == 0 {
            return Err(ConfigError::Invalid(
                "sub_agents max_iterations must be > 0".into(),
            ));
        }
        self.context.validate()?;
        if self.context.max_tokens <= self.provider.max_tokens {
            return Err(ConfigError::Invalid(
                "context max_tokens must be > provider max_tokens (output reserve)".into(),
            ));
        }
        if let Err(e) = self.duties.distill.parse() {
            return Err(ConfigError::Invalid(format!("duties.distill: {e}")));
        }
        if let Err(e) = self.duties.warm.parse() {
            return Err(ConfigError::Invalid(format!("duties.warm: {e}")));
        }
        self.validate_prompt_duties()?;
        self.validate_repositories()?;
        if self.memory.index_cap_bytes == 0 {
            return Err(ConfigError::Invalid(
                "memory index_cap_bytes must be > 0".into(),
            ));
        }
        if self.memory.distill_threshold_tokens == 0 {
            return Err(ConfigError::Invalid(
                "memory distill_threshold_tokens must be > 0".into(),
            ));
        }
        if self.provider.max_tokens == 0 {
            return Err(ConfigError::Invalid("max_tokens must be > 0".into()));
        }
        if !(0.0..=2.0).contains(&self.provider.temperature) {
            return Err(ConfigError::Invalid(
                "temperature must be between 0.0 and 2.0".into(),
            ));
        }
        if self.telegram.enabled {
            if self.telegram.chat_id == 0 {
                return Err(ConfigError::Invalid(
                    "telegram chat_id must be set when enabled".into(),
                ));
            }
            if self.telegram.poll_timeout_secs == 0 {
                return Err(ConfigError::Invalid(
                    "telegram poll_timeout_secs must be > 0".into(),
                ));
            }
        }
        if self.github.enabled && self.github.poll_interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "github poll_interval_secs must be > 0".into(),
            ));
        }
        if self.github.enabled && self.github.owner.is_empty() {
            return Err(ConfigError::Invalid(
                "github owner must be set when enabled".into(),
            ));
        }
        if self.linear.enabled {
            if self.linear.poll_interval_secs == 0 {
                return Err(ConfigError::Invalid(
                    "linear poll_interval_secs must be > 0".into(),
                ));
            }
            if self.linear.trusted_users.is_empty() {
                return Err(ConfigError::Invalid(
                    "linear trusted_users must be non-empty when enabled".into(),
                ));
            }
        }
        if self.tools.exec.timeout_secs == 0 {
            return Err(ConfigError::Invalid("timeout_secs must be > 0".into()));
        }
        if self.tools.web_fetch.timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "web_fetch timeout_secs must be > 0".into(),
            ));
        }
        if self.tools.web_fetch.max_response_bytes == 0 {
            return Err(ConfigError::Invalid(
                "web_fetch max_response_bytes must be > 0".into(),
            ));
        }
        if self.tools.web_search.max_tokens == 0 {
            return Err(ConfigError::Invalid(
                "web_search max_tokens must be > 0".into(),
            ));
        }
        if self.tools.web_search.timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "web_search timeout_secs must be > 0".into(),
            ));
        }
        Ok(())
    }

    /// Validate `[[duties.prompt]]` entries: parseable schedule, a
    /// single-token unique name that shadows no built-in, a non-empty
    /// prompt, and a repo listed in `git.repositories`.
    fn validate_prompt_duties(&self) -> Result<(), ConfigError> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.duties.prompt {
            let ctx = |e: String| ConfigError::Invalid(format!("duties.prompt {:?}: {e}", p.name));
            if p.name.is_empty() || p.name.contains(char::is_whitespace) {
                return Err(ctx("name must be a single non-empty token".into()));
            }
            if p.name == "distill" || p.name == "warm" {
                return Err(ctx("name shadows a built-in duty".into()));
            }
            if !seen.insert(&p.name) {
                return Err(ctx("duplicate duty name".into()));
            }
            if p.prompt.trim().is_empty() {
                return Err(ctx("prompt must be non-empty".into()));
            }
            p.schedule.parse().map_err(ctx)?;
            if !crate::tools::git::url::is_trusted_repo(&p.repo, &self.git.trusted_repos()) {
                return Err(ctx(format!("repo {:?} is not in git.repositories", p.repo)));
            }
            match p.gate.as_deref() {
                None | Some("new-commits") => {}
                Some(other) => {
                    return Err(ctx(format!(
                        "unknown gate {other:?}; expected \"new-commits\""
                    )));
                }
            }
            if p.gate.is_some() && !self.github.enabled {
                return Err(ctx("gate \"new-commits\" requires github.enabled".into()));
            }
        }
        Ok(())
    }

    /// Validate `[git.repositories]`: exact `owner/repo` keys; check
    /// commands non-empty and only with the git tools enabled. Trust
    /// itself needs no validation — listing the repo is the grant.
    fn validate_repositories(&self) -> Result<(), ConfigError> {
        for (nwo, repo) in &self.git.repositories {
            let ctx = |e: String| ConfigError::Invalid(format!("git.repositories {nwo:?}: {e}"));
            let exact = matches!(nwo.split('/').collect::<Vec<_>>().as_slice(),
                [owner, repo] if !owner.is_empty() && !repo.is_empty() && !nwo.contains('*'));
            if !exact {
                return Err(ctx("key must be an exact owner/repo".into()));
            }
            let Some(check) = &repo.check else { continue };
            if !self.git.enabled {
                return Err(ctx(
                    "check requires git.enabled (warming serves git_commit)".into(),
                ));
            }
            if check.trim().is_empty() {
                return Err(ctx("check command must be non-empty".into()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write `config.toml` into a temp dir and load it.
    fn load_toml(content: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), content).unwrap();
        Config::load(dir.path())
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.provider.model, "arcee-ai/trinity-large-preview:free");
        assert_eq!(cfg.provider.max_tokens, 32_768);
        assert!((cfg.provider.temperature - 0.7).abs() < f32::EPSILON);
        assert_eq!(cfg.agent.max_iterations, 100);
        assert_eq!(cfg.tools.exec.timeout_secs, 600);
        assert_eq!(cfg.tools.web_fetch.max_response_bytes, 512 * 1024);
    }

    #[test]
    fn reject_context_window_not_larger_than_output_budget() {
        let result = load_toml("[provider]\nmax_tokens = 300000\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn effective_context_reserves_output_budget() {
        let cfg =
            load_toml("[provider]\nmax_tokens = 32768\n[context]\nmax_tokens = 200000\n").unwrap();
        assert_eq!(cfg.effective_context().max_tokens, 200_000 - 32_768);
    }

    #[test]
    fn tools_disabled_defaults_empty() {
        let cfg = load_toml("").unwrap();
        assert!(cfg.tools.disabled.is_empty());
    }

    #[test]
    fn tools_disabled_parse() {
        let cfg = load_toml("[tools]\ndisabled = [\"web_search\", \"git_push\"]\n").unwrap();
        assert_eq!(cfg.tools.disabled, vec!["web_search", "git_push"]);
    }

    #[test]
    fn load_empty_string_returns_defaults() {
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.provider.max_tokens, 32_768);
        assert_eq!(cfg.agent.max_iterations, 100);
    }

    #[test]
    fn load_partial_config() {
        let cfg = load_toml("[provider]\nmodel = \"anthropic/claude-sonnet-4\"\n").unwrap();
        assert_eq!(cfg.provider.model, "anthropic/claude-sonnet-4");
        // Other fields keep defaults
        assert_eq!(cfg.provider.max_tokens, 32_768);
        assert_eq!(cfg.agent.max_iterations, 100);
    }

    #[test]
    fn load_full_config() {
        let cfg = load_toml(
            "\
[provider]
model = \"openai/gpt-4\"
max_tokens = 8192
temperature = 0.5

[agent]
max_iterations = 30

[tools.exec]
timeout_secs = 120
",
        )
        .unwrap();
        assert_eq!(cfg.provider.model, "openai/gpt-4");
        assert_eq!(cfg.provider.max_tokens, 8192);
        assert!((cfg.provider.temperature - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.agent.max_iterations, 30);
        assert_eq!(cfg.tools.exec.timeout_secs, 120);
    }

    #[test]
    fn mcp_defaults_to_no_servers() {
        let cfg = load_toml("").unwrap();
        assert!(cfg.mcp.servers.is_empty());
        assert_eq!(cfg.mcp.startup_timeout_secs, 30);
        assert_eq!(cfg.mcp.call_timeout_secs, 60);
    }

    #[test]
    fn mcp_server_parses() {
        let cfg = load_toml(
            "\
[mcp.servers.bkb]
command = \"bkb-mcp\"
args = [\"--stdio\"]
explore = true
tools = [\"search\", \"timeline\"]

[mcp.servers.bkb.env]
BKB_MODE = \"full\"

[mcp.servers.bkb.env_credentials]
BKB_API_KEY = \"bkb-api-key\"
",
        )
        .unwrap();
        let server = &cfg.mcp.servers["bkb"];
        assert_eq!(server.command, "bkb-mcp");
        assert_eq!(server.args, vec!["--stdio"]);
        assert!(server.explore);
        assert_eq!(server.tools.as_deref().unwrap().len(), 2);
        assert_eq!(server.env["BKB_MODE"], "full");
        assert_eq!(server.env_credentials["BKB_API_KEY"], "bkb-api-key");
    }

    #[test]
    fn review_enabled_by_default() {
        let cfg = load_toml("").unwrap();
        assert!(cfg.review.enabled);
        let cfg = load_toml("[review]\nenabled = false\n").unwrap();
        assert!(!cfg.review.enabled);
    }

    #[test]
    fn model_overrides_default_unset() {
        let cfg = load_toml("").unwrap();
        assert!(cfg.provider.model_overrides.explore.is_none());
        assert!(cfg.provider.model_overrides.worker.is_none());
        assert!(cfg.provider.model_overrides.reviewer.is_none());
        assert!(cfg.provider.model_overrides.summarizer.is_none());
        assert!(cfg.provider.model_overrides.memory.is_none());
        assert!(cfg.provider.model_overrides.memory.is_none());
    }

    #[test]
    fn model_overrides_parse() {
        let cfg = load_toml(
            "\
[provider.model_overrides]
explore = \"cheap/explore\"
worker = \"mid/worker\"
reviewer = \"strong/reviewer\"
summarizer = \"cheap/summarizer\"
memory = \"cheap/memory\"
",
        )
        .unwrap();
        let overrides = &cfg.provider.model_overrides;
        assert_eq!(overrides.explore.as_deref(), Some("cheap/explore"));
        assert_eq!(overrides.worker.as_deref(), Some("mid/worker"));
        assert_eq!(overrides.reviewer.as_deref(), Some("strong/reviewer"));
        assert_eq!(overrides.summarizer.as_deref(), Some("cheap/summarizer"));
        assert_eq!(overrides.memory.as_deref(), Some("cheap/memory"));
    }

    #[test]
    fn model_overrides_reject_unknown_role() {
        let result = load_toml("[provider.model_overrides]\nroot = \"nope\"\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn reject_unknown_fields() {
        let result = load_toml("[provider]\ntypo_field = \"oops\"\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn reject_invalid_temperature() {
        let result = load_toml("[provider]\ntemperature = 3.0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn reject_zero_max_tokens() {
        let result = load_toml("[provider]\nmax_tokens = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn reject_malformed_toml() {
        let result = load_toml("not valid [[[toml");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn duties_defaults() {
        use crate::duty::schedule::Schedule;
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.duties.distill.parse().unwrap(), Schedule::Every(3600));
    }

    #[test]
    fn duties_parse_every_and_daily() {
        use crate::duty::schedule::Schedule;
        let cfg = load_toml("[duties.distill]\nevery = \"2h\"\n").unwrap();
        assert_eq!(cfg.duties.distill.parse().unwrap(), Schedule::Every(7200));
        let cfg = load_toml("[duties.distill]\ndaily = \"06:00\"\n").unwrap();
        assert_eq!(
            cfg.duties.distill.parse().unwrap(),
            Schedule::Daily(6 * 3600)
        );
    }

    #[test]
    fn duties_reject_unknown_field() {
        let result = load_toml("[duties.distill]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn duties_reject_both_and_garbage_schedules() {
        for toml in [
            "[duties.distill]\nevery = \"1h\"\ndaily = \"06:00\"\n",
            "[duties.distill]\nevery = \"soon\"\n",
            "[duties.distill]\ndaily = \"25:00\"\n",
        ] {
            let result = load_toml(toml);
            assert!(matches!(result, Err(ConfigError::Invalid(_))), "{toml}");
        }
    }

    /// A well-formed prompt duty on a trusted repo.
    const PROMPT_DUTY: &str = "\
[git]
enabled = true

[git.repositories.\"owner/repo\"]

[[duties.prompt]]
name = \"security-watch\"
daily = \"06:00\"
repo = \"owner/repo\"
prompt = \"Review recent commits for security issues.\"
";

    #[test]
    fn prompt_duty_parses() {
        let cfg = load_toml(PROMPT_DUTY).unwrap();
        let p = &cfg.duties.prompt[0];
        assert_eq!(p.name, "security-watch");
        assert_eq!(p.repo, "owner/repo");
        assert!(p.schedule.parse().is_ok());
    }

    #[test]
    fn prompt_duty_rejects_untrusted_repo() {
        let toml = PROMPT_DUTY.replace("repo = \"owner/repo\"", "repo = \"evil/repo\"");
        let result = load_toml(&toml);
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn prompt_duty_rejects_bad_names() {
        for (from, to) in [
            ("name = \"security-watch\"", "name = \"two tokens\""),
            ("name = \"security-watch\"", "name = \"\""),
            ("name = \"security-watch\"", "name = \"distill\""),
        ] {
            let toml = PROMPT_DUTY.replace(from, to);
            let result = load_toml(&toml);
            assert!(matches!(result, Err(ConfigError::Invalid(_))), "{to}");
        }
    }

    #[test]
    fn prompt_duty_rejects_duplicate_names() {
        let toml = format!(
            "{PROMPT_DUTY}\n[[duties.prompt]]\nname = \"security-watch\"\n\
             every = \"1h\"\nrepo = \"owner/repo\"\nprompt = \"again\"\n"
        );
        let result = load_toml(&toml);
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn prompt_duty_gate_requires_github() {
        // Gate on a config without github.enabled.
        let toml = format!("{PROMPT_DUTY}gate = \"new-commits\"\n");
        let result = load_toml(&toml);
        assert!(matches!(result, Err(ConfigError::Invalid(_))));

        // With github enabled it parses.
        let toml = format!(
            "[github]\nenabled = true\nowner = \"o\"\n\n{PROMPT_DUTY}gate = \"new-commits\"\n"
        );
        let cfg = load_toml(&toml).unwrap();
        assert_eq!(cfg.duties.prompt[0].gate.as_deref(), Some("new-commits"));
    }

    #[test]
    fn prompt_duty_rejects_unknown_gate() {
        let toml = format!(
            "[github]\nenabled = true\nowner = \"o\"\n\n{PROMPT_DUTY}gate = \"full-moon\"\n"
        );
        let result = load_toml(&toml);
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn prompt_duty_rejects_empty_prompt() {
        let toml = PROMPT_DUTY.replace(
            "prompt = \"Review recent commits for security issues.\"",
            "prompt = \"  \"",
        );
        let result = load_toml(&toml);
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn memory_defaults() {
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.memory.index_cap_bytes, 8192);
        assert_eq!(cfg.memory.distill_threshold_tokens, 40_000);
    }

    #[test]
    fn memory_parse() {
        let cfg = load_toml("[memory]\nindex_cap_bytes = 4096\ndistill_threshold_tokens = 20000\n")
            .unwrap();
        assert_eq!(cfg.memory.index_cap_bytes, 4096);
        assert_eq!(cfg.memory.distill_threshold_tokens, 20_000);
    }

    #[test]
    fn memory_reject_zero_cap() {
        let result = load_toml("[memory]\nindex_cap_bytes = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn memory_reject_zero_distill_threshold() {
        let result = load_toml("[memory]\ndistill_threshold_tokens = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn memory_reject_unknown_field() {
        let result = load_toml("[memory]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn sub_agents_defaults() {
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.sub_agents.max_iterations, 30);
    }

    #[test]
    fn sub_agents_parse() {
        let cfg = load_toml("[sub_agents]\nmax_iterations = 5\n").unwrap();
        assert_eq!(cfg.sub_agents.max_iterations, 5);
    }

    #[test]
    fn sub_agents_reject_zero_max_iterations() {
        let result = load_toml("[sub_agents]\nmax_iterations = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn sub_agents_reject_unknown_field() {
        let result = load_toml("[sub_agents]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn telegram_defaults() {
        let cfg = load_toml("").unwrap();
        assert!(!cfg.telegram.enabled);
        assert_eq!(cfg.telegram.chat_id, 0);
        assert_eq!(cfg.telegram.poll_timeout_secs, 30);
    }

    #[test]
    fn telegram_parse() {
        let cfg =
            load_toml("[telegram]\nenabled = true\nchat_id = 123456789\npoll_timeout_secs = 60\n")
                .unwrap();
        assert!(cfg.telegram.enabled);
        assert_eq!(cfg.telegram.chat_id, 123_456_789);
        assert_eq!(cfg.telegram.poll_timeout_secs, 60);
    }

    #[test]
    fn telegram_reject_unknown_field() {
        let result = load_toml("[telegram]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn telegram_reject_zero_chat_id_when_enabled() {
        let result = load_toml("[telegram]\nenabled = true\nchat_id = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn telegram_reject_zero_poll_timeout_when_enabled() {
        let result = load_toml("[telegram]\nenabled = true\nchat_id = 1\npoll_timeout_secs = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn telegram_disabled_skips_validation() {
        // chat_id=0 is fine when disabled — no credentials needed
        let cfg = load_toml("[telegram]\nenabled = false\nchat_id = 0\n").unwrap();
        assert!(!cfg.telegram.enabled);
    }

    #[test]
    fn context_defaults() {
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.context.max_tokens, 200_000);
        assert_eq!(cfg.context.budget_percent, 80);
        assert_eq!(cfg.context.tool_output_tokens, 4096);
        assert_eq!(cfg.context.engine, EngineKind::Flat);
    }

    #[test]
    fn context_parse_tool_output_tokens() {
        let cfg = load_toml("[context]\ntool_output_tokens = 8000\n").unwrap();
        assert_eq!(cfg.context.tool_output_tokens, 8000);
    }

    #[test]
    fn context_reject_zero_tool_output_tokens() {
        let result = load_toml("[context]\ntool_output_tokens = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn context_engine_parse_lcm() {
        let cfg = load_toml("[context]\nengine = \"lcm\"\n").unwrap();
        assert_eq!(cfg.context.engine, EngineKind::Lcm);
    }

    #[test]
    fn context_engine_reject_unknown_value() {
        let result = load_toml("[context]\nengine = \"hierarchical\"\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn context_parse() {
        let cfg = load_toml("[context]\nmax_tokens = 64000\nbudget_percent = 60\n").unwrap();
        assert_eq!(cfg.context.max_tokens, 64_000);
        assert_eq!(cfg.context.budget_percent, 60);
    }

    #[test]
    fn context_reject_unknown_field() {
        let result = load_toml("[context]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn context_reject_zero_max_tokens() {
        let result = load_toml("[context]\nmax_tokens = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn context_reject_zero_budget_percent() {
        let result = load_toml("[context]\nbudget_percent = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn context_reject_budget_percent_over_100() {
        let result = load_toml("[context]\nbudget_percent = 101\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn lcm_large_file_defaults() {
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.context.lcm.large_file_threshold, 25_000);
        assert_eq!(cfg.context.lcm.large_file_summary_tokens, 400);
    }

    #[test]
    fn lcm_large_file_parse() {
        let cfg = load_toml(
            "[context.lcm]\nlarge_file_threshold = 10000\nlarge_file_summary_tokens = 200\n",
        )
        .unwrap();
        assert_eq!(cfg.context.lcm.large_file_threshold, 10_000);
        assert_eq!(cfg.context.lcm.large_file_summary_tokens, 200);
    }

    #[test]
    fn lcm_reject_zero_large_file_threshold() {
        let result = load_toml("[context.lcm]\nlarge_file_threshold = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn lcm_reject_zero_large_file_summary_tokens() {
        let result = load_toml("[context.lcm]\nlarge_file_summary_tokens = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    // ── linear ────────────────────────────────────────────────────────

    #[test]
    fn linear_defaults() {
        let cfg = load_toml("").unwrap();
        assert!(!cfg.linear.enabled);
        assert_eq!(cfg.linear.poll_interval_secs, 120);
        assert!(cfg.linear.trusted_users.is_empty());
    }

    #[test]
    fn linear_parse() {
        let cfg = load_toml(
            "[linear]\nenabled = true\ntrusted_users = [\"me@example.com\"]\npoll_interval_secs = 60\n",
        )
        .unwrap();
        assert!(cfg.linear.enabled);
        assert_eq!(cfg.linear.poll_interval_secs, 60);
        assert_eq!(cfg.linear.trusted_users, vec!["me@example.com"]);
    }

    #[test]
    fn linear_reject_unknown_field() {
        let result = load_toml("[linear]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn linear_reject_zero_poll_interval_when_enabled() {
        let result = load_toml(
            "[linear]\nenabled = true\ntrusted_users = [\"me@example.com\"]\npoll_interval_secs = 0\n",
        );
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn linear_reject_empty_trusted_users_when_enabled() {
        let result = load_toml("[linear]\nenabled = true\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn linear_disabled_skips_validation() {
        let cfg = load_toml("[linear]\nenabled = false\npoll_interval_secs = 0\n").unwrap();
        assert!(!cfg.linear.enabled);
    }

    // ── git ───────────────────────────────────────────────────────────

    #[test]
    fn git_defaults() {
        let cfg = load_toml("").unwrap();
        assert!(!cfg.git.enabled);
        assert!(cfg.git.co_authors.is_empty());
        assert!(cfg.git.repositories.is_empty());
        assert!(cfg.git.warm_commands().is_empty());
    }

    #[test]
    fn git_parse() {
        let cfg = load_toml(
            "[git]\nenabled = true\nco_authors = [\"Alice <alice@example.com>\"]\n\
             [git.repositories.\"alice/repo\"]\ncheck = \"just check\"\n\
             [git.repositories.\"alice/other\"]\n",
        )
        .unwrap();
        assert!(cfg.git.enabled);
        assert_eq!(cfg.git.co_authors, vec!["Alice <alice@example.com>"]);
        assert_eq!(cfg.git.trusted_repos(), vec!["alice/other", "alice/repo"]);
        assert_eq!(
            cfg.git
                .warm_commands()
                .get("alice/repo")
                .map(String::as_str),
            Some("just check")
        );
    }

    #[test]
    fn git_reject_unknown_field() {
        let result = load_toml("[git]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn repositories_reject_unknown_field() {
        let result = load_toml("[git.repositories.\"alice/repo\"]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn trust_only_entry_needs_no_git_enabled() {
        let cfg = load_toml("[git.repositories.\"alice/repo\"]\n").unwrap();
        assert_eq!(cfg.git.trusted_repos(), vec!["alice/repo"]);
    }

    #[test]
    fn check_requires_git_enabled() {
        let result = load_toml("[git.repositories.\"alice/repo\"]\ncheck = \"just check\"\n");
        assert!(matches!(result, Err(ConfigError::Invalid(m)) if m.contains("git.enabled")));
    }

    #[test]
    fn repositories_reject_wildcard_and_malformed_keys() {
        for key in ["alice/*", "alice", "alice/repo/extra", "/repo", "alice/"] {
            let toml = format!("[git.repositories.\"{key}\"]\n");
            let result = load_toml(&toml);
            assert!(
                matches!(result, Err(ConfigError::Invalid(m)) if m.contains("owner/repo")),
                "key {key:?} must be rejected"
            );
        }
    }

    #[test]
    fn check_rejects_empty_command() {
        let result = load_toml(
            "[git]\nenabled = true\n\
             [git.repositories.\"alice/repo\"]\ncheck = \"  \"\n",
        );
        assert!(matches!(result, Err(ConfigError::Invalid(m)) if m.contains("non-empty")));
    }

    // ── github ────────────────────────────────────────────────────────

    #[test]
    fn github_defaults() {
        let cfg = load_toml("").unwrap();
        assert!(!cfg.github.enabled);
        assert_eq!(cfg.github.poll_interval_secs, 300);
        assert!(cfg.github.owner.is_empty());
        assert!(cfg.github.trusted_users.is_empty());
        assert!(cfg.github.trusted_bots.is_empty());
    }

    #[test]
    fn github_parse() {
        let cfg =
            load_toml("[github]\nenabled = true\nowner = \"alice\"\npoll_interval_secs = 600\n")
                .unwrap();
        assert!(cfg.github.enabled);
        assert_eq!(cfg.github.owner, "alice");
        assert_eq!(cfg.github.poll_interval_secs, 600);
    }

    #[test]
    fn github_parse_trusted_users() {
        let cfg =
            load_toml("[github]\nenabled = true\nowner = \"alice\"\ntrusted_users = [\"bob\"]\n")
                .unwrap();
        assert!(cfg.github.enabled);
        assert_eq!(cfg.github.owner, "alice");
        assert_eq!(cfg.github.trusted_users, vec!["bob"]);
    }

    #[test]
    fn github_parse_trusted_bots() {
        let cfg = load_toml(
            "[github]\nenabled = true\nowner = \"alice\"\n\
             trusted_bots = [\"chatgpt-codex-connector\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.github.trusted_bots, vec!["chatgpt-codex-connector"]);
    }

    #[test]
    fn github_reject_missing_owner_when_enabled() {
        let result = load_toml("[github]\nenabled = true\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn github_reject_unknown_field() {
        let result = load_toml("[github]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn github_reject_zero_poll_interval_when_enabled() {
        let result = load_toml("[github]\nenabled = true\npoll_interval_secs = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn github_disabled_skips_validation() {
        let cfg = load_toml("[github]\nenabled = false\npoll_interval_secs = 0\n").unwrap();
        assert!(!cfg.github.enabled);
    }
}
