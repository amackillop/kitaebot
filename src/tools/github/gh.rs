//! `github_gh` tool — generic `gh` CLI execution.
//!
//! Escape hatch for `gh` operations not covered by dedicated tools.
//! Restricts subcommands to a static allowlist.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use crate::error::ToolError;
use crate::tools::cli_runner::{self, SubprocessCall};

/// Subcommands the model may invoke.
const ALLOWED_SUBCOMMANDS: &[&str] = &["issue", "pr", "release"];

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// Arguments passed to `gh` (e.g. `["pr", "edit", "42", "--title", "New title"]`).
    /// The first element must be an allowed subcommand.
    args: Vec<String>,
}

pub struct Gh(pub GhCli);

impl Tool for Gh {
    fn name(&self) -> &'static str {
        "github_gh"
    }

    fn description(&self) -> &'static str {
        "Run a gh CLI command (for operations not covered by dedicated tools)"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args.repo_dir, &args.args).await
        })
    }
}

impl Gh {
    fn prepare(&self, repo_dir: &str, args: &[String]) -> Result<SubprocessCall, ToolError> {
        let subcmd = args
            .first()
            .ok_or_else(|| ToolError::InvalidArguments("args must not be empty".into()))?;

        if !ALLOWED_SUBCOMMANDS.contains(&subcmd.as_str()) {
            return Err(ToolError::Blocked {
                operation: format!("gh {subcmd}"),
                guidance: format!(
                    "only these subcommands are allowed: {}",
                    ALLOWED_SUBCOMMANDS.join(", "),
                ),
            });
        }

        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Ok(self.0.prepare_gh(&refs, &cwd))
    }

    async fn run(&self, repo_dir: &str, args: &[String]) -> Result<String, ToolError> {
        let call = self.prepare(repo_dir, args)?;
        cli_runner::exec(&call).await?.format()
    }
}

#[cfg(test)]
mod tests {
    use std::string::ToString;

    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn rejects_empty_args() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = Gh(gh);
        let result = tool.prepare(&repo, &[]);
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[test]
    fn rejects_disallowed_subcommand() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = Gh(gh);
        for subcmd in ["auth", "config", "secret", "ssh-key", "gpg-key"] {
            let result = tool.prepare(&repo, &[subcmd.to_string()]);
            assert!(
                matches!(result, Err(ToolError::Blocked { .. })),
                "{subcmd} should be blocked",
            );
        }
    }

    #[test]
    fn accepts_allowed_subcommands() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = Gh(gh);
        for subcmd in ALLOWED_SUBCOMMANDS {
            let result = tool.prepare(&repo, &[subcmd.to_string()]);
            assert!(result.is_ok(), "{subcmd} should be allowed");
        }
    }

    #[test]
    fn builds_correct_command() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = Gh(gh);
        let args: Vec<String> = ["pr", "edit", "42", "--title", "New title"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let call = tool.prepare(&repo, &args).unwrap();
        assert_eq!(call.binary, "gh");
        assert_eq!(call.args, ["pr", "edit", "42", "--title", "New title"]);
        assert!(call.has_env("GH_TOKEN"));
    }
}
