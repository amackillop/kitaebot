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
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub socket: SocketConfig,
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

/// LLM provider settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    /// `OpenAI`-compatible API to use.
    pub api: Api,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

/// `OpenAI`-compatible chat completions API.
///
/// Each variant maps to a known endpoint URL. Invalid values are
/// rejected at config parse time.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    #[default]
    OpenRouter,
    OpenAi,
    Groq,
    Together,
    Mistral,
}

impl Api {
    /// Endpoint URL for the chat completions API.
    #[cfg_attr(feature = "mock-network", allow(dead_code))]
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::OpenRouter => "https://openrouter.ai/api/v1/chat/completions",
            Self::OpenAi => "https://api.openai.com/v1/chat/completions",
            Self::Groq => "https://api.groq.com/openai/v1/chat/completions",
            Self::Together => "https://api.together.xyz/v1/chat/completions",
            Self::Mistral => "https://api.mistral.ai/v1/chat/completions",
        }
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
    pub max_output_bytes: usize,
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

/// Heartbeat daemon settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeartbeatConfig {
    pub interval_secs: u64,
}

/// Telegram channel settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramConfig {
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
}

/// Context window management settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Maximum context window size in tokens.
    pub max_tokens: u32,
    /// Percentage of `max_tokens` at which compaction triggers (1–100).
    pub budget_percent: u8,
}

/// Git settings.
///
/// Identity (user.name, user.email) is managed at the system level via
/// NixOS `programs.git`. This section holds agent-level settings only.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfig {
    /// `Co-authored-by` trailers appended to commit messages.
    /// Each entry is `"Name <email>"`.
    pub co_authors: Vec<String>,
}

/// GitHub integration settings.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    /// Enable the GitHub integration. Defaults to `false` so the daemon
    /// can start without a GitHub token.
    pub enabled: bool,
}

// --- Default impls ---

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 100,
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 200_000,
            budget_percent: 80,
        }
    }
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 60,
            max_output_bytes: 10 * 1024,
        }
    }
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_secs: 1800,
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api: Api::default(),
            model: "arcee-ai/trinity-large-preview:free".to_string(),
            max_tokens: 4096,
            temperature: 0.7,
        }
    }
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            path: "/run/kitaebot/chat.sock".to_string(),
        }
    }
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
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
            max_response_bytes: 50 * 1024,
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
        let path = workspace.join("config.toml");

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

    /// Validate invariants that serde alone cannot enforce.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.agent.max_iterations == 0 {
            return Err(ConfigError::Invalid("max_iterations must be > 0".into()));
        }
        if self.context.max_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context max_tokens must be > 0".into(),
            ));
        }
        if self.context.budget_percent == 0 || self.context.budget_percent > 100 {
            return Err(ConfigError::Invalid(
                "context budget_percent must be 1..=100".into(),
            ));
        }
        if self.heartbeat.interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "heartbeat interval_secs must be > 0".into(),
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
        if self.tools.exec.timeout_secs == 0 {
            return Err(ConfigError::Invalid("timeout_secs must be > 0".into()));
        }
        if self.tools.exec.max_output_bytes == 0 {
            return Err(ConfigError::Invalid("max_output_bytes must be > 0".into()));
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
        assert_eq!(cfg.provider.max_tokens, 4096);
        assert!((cfg.provider.temperature - 0.7).abs() < f32::EPSILON);
        assert_eq!(cfg.agent.max_iterations, 100);
        assert_eq!(cfg.tools.exec.timeout_secs, 60);
        assert_eq!(cfg.tools.exec.max_output_bytes, 10240);
    }

    #[test]
    fn tools_disabled_defaults_empty() {
        let cfg = load_toml("").unwrap();
        assert!(cfg.tools.disabled.is_empty());
    }

    #[test]
    fn tools_disabled_parse() {
        let cfg = load_toml("[tools]\ndisabled = [\"web_search\", \"github_push\"]\n").unwrap();
        assert_eq!(cfg.tools.disabled, vec!["web_search", "github_push"]);
    }

    #[test]
    fn load_empty_string_returns_defaults() {
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.provider.max_tokens, 4096);
        assert_eq!(cfg.agent.max_iterations, 100);
    }

    #[test]
    fn load_partial_config() {
        let cfg = load_toml("[provider]\nmodel = \"anthropic/claude-sonnet-4\"\n").unwrap();
        assert_eq!(cfg.provider.model, "anthropic/claude-sonnet-4");
        // Other fields keep defaults
        assert_eq!(cfg.provider.max_tokens, 4096);
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
max_output_bytes = 20480
",
        )
        .unwrap();
        assert_eq!(cfg.provider.model, "openai/gpt-4");
        assert_eq!(cfg.provider.max_tokens, 8192);
        assert!((cfg.provider.temperature - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.agent.max_iterations, 30);
        assert_eq!(cfg.tools.exec.timeout_secs, 120);
        assert_eq!(cfg.tools.exec.max_output_bytes, 20480);
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
    fn heartbeat_defaults() {
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.heartbeat.interval_secs, 1800);
    }

    #[test]
    fn heartbeat_parse() {
        let cfg = load_toml("[heartbeat]\ninterval_secs = 600\n").unwrap();
        assert_eq!(cfg.heartbeat.interval_secs, 600);
    }

    #[test]
    fn heartbeat_reject_unknown_field() {
        let result = load_toml("[heartbeat]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn heartbeat_reject_zero_interval() {
        let result = load_toml("[heartbeat]\ninterval_secs = 0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
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

    // ── git ───────────────────────────────────────────────────────────

    #[test]
    fn git_defaults() {
        let cfg = load_toml("").unwrap();
        assert!(cfg.git.co_authors.is_empty());
    }

    #[test]
    fn git_parse() {
        let cfg = load_toml("[git]\nco_authors = [\"Alice <alice@example.com>\"]\n").unwrap();
        assert_eq!(cfg.git.co_authors, vec!["Alice <alice@example.com>"]);
    }

    #[test]
    fn git_reject_unknown_field() {
        let result = load_toml("[git]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    // ── github ────────────────────────────────────────────────────────

    #[test]
    fn github_defaults() {
        let cfg = load_toml("").unwrap();
        assert!(!cfg.github.enabled);
    }

    #[test]
    fn github_parse() {
        let cfg = load_toml("[github]\nenabled = true\n").unwrap();
        assert!(cfg.github.enabled);
    }

    #[test]
    fn github_reject_unknown_field() {
        let result = load_toml("[github]\ntypo = 1\n");
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }
}
