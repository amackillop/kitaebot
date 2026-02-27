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
}
