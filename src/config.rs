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
    pub provider: ProviderConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub context: ContextConfig,
}

/// LLM provider settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

/// Agent loop settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub max_iterations: usize,
}

/// Tool-level settings.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub exec: ExecConfig,
}

/// Settings for the `exec` tool.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecConfig {
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
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

/// Context window management settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Maximum context window size in tokens.
    pub max_tokens: u32,
    /// Fraction of `max_tokens` at which compaction triggers.
    pub budget_ratio: f32,
}

// --- Default impls ---

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            model: "arcee-ai/trinity-large-preview:free".to_string(),
            max_tokens: 4096,
            temperature: 0.7,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { max_iterations: 20 }
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

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 200_000,
            budget_ratio: 0.8,
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
        if self.provider.max_tokens == 0 {
            return Err(ConfigError::Invalid("max_tokens must be > 0".into()));
        }
        if !(0.0..=2.0).contains(&self.provider.temperature) {
            return Err(ConfigError::Invalid(
                "temperature must be between 0.0 and 2.0".into(),
            ));
        }
        if self.agent.max_iterations == 0 {
            return Err(ConfigError::Invalid("max_iterations must be > 0".into()));
        }
        if self.tools.exec.timeout_secs == 0 {
            return Err(ConfigError::Invalid("timeout_secs must be > 0".into()));
        }
        if self.tools.exec.max_output_bytes == 0 {
            return Err(ConfigError::Invalid("max_output_bytes must be > 0".into()));
        }
        if self.heartbeat.interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "heartbeat interval_secs must be > 0".into(),
            ));
        }
        if self.context.max_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context max_tokens must be > 0".into(),
            ));
        }
        if self.context.budget_ratio <= 0.0 || self.context.budget_ratio > 1.0 {
            return Err(ConfigError::Invalid(
                "context budget_ratio must be in (0.0, 1.0]".into(),
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
        assert_eq!(cfg.agent.max_iterations, 20);
        assert_eq!(cfg.tools.exec.timeout_secs, 60);
        assert_eq!(cfg.tools.exec.max_output_bytes, 10240);
    }

    #[test]
    fn load_empty_string_returns_defaults() {
        let cfg = load_toml("").unwrap();
        assert_eq!(cfg.provider.max_tokens, 4096);
        assert_eq!(cfg.agent.max_iterations, 20);
    }

    #[test]
    fn load_partial_config() {
        let cfg = load_toml("[provider]\nmodel = \"anthropic/claude-sonnet-4\"\n").unwrap();
        assert_eq!(cfg.provider.model, "anthropic/claude-sonnet-4");
        // Other fields keep defaults
        assert_eq!(cfg.provider.max_tokens, 4096);
        assert_eq!(cfg.agent.max_iterations, 20);
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
        assert!((cfg.context.budget_ratio - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn context_parse() {
        let cfg = load_toml("[context]\nmax_tokens = 64000\nbudget_ratio = 0.6\n").unwrap();
        assert_eq!(cfg.context.max_tokens, 64_000);
        assert!((cfg.context.budget_ratio - 0.6).abs() < f32::EPSILON);
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
    fn context_reject_zero_budget_ratio() {
        let result = load_toml("[context]\nbudget_ratio = 0.0\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn context_reject_budget_ratio_over_one() {
        let result = load_toml("[context]\nbudget_ratio = 1.1\n");
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }
}
