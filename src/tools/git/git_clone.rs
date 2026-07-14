//! `git_clone` tool — clone or refresh a repository in the workspace.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::git_cli::GitCli;
use super::url::{extract_nwo, to_https_url};
use super::{Tool, ToolCtx, checkout, origin_trusted};
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository URL (HTTPS or SSH). SSH URLs are rewritten to HTTPS
    /// automatically.
    url: String,
}

pub struct GitClone {
    pub git: GitCli,
}

impl Tool for GitClone {
    fn name(&self) -> &'static str {
        "git_clone"
    }

    fn description(&self) -> &'static str {
        "Clone a repository into the workspace, or fetch it if already cloned"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args.url).await
        })
    }
}

/// Pure: normalize the URL and derive the checkout location.
///
/// Returns `(https_url, nwo, workspace-relative path)`.
fn target(url: &str) -> Result<(String, String, String), ToolError> {
    let https_url = to_https_url(url)?;
    let nwo = extract_nwo(&https_url).ok_or_else(|| {
        ToolError::InvalidArguments(format!("cannot extract owner/repo from: {url}"))
    })?;
    let rel = checkout::rel_path("projects", &nwo)?;
    Ok((https_url, nwo, rel))
}

impl GitClone {
    async fn run(&self, url: &str) -> Result<String, ToolError> {
        let (https_url, nwo, rel) = target(url)?;
        let dir = self.git.workspace_root().join(&rel);

        let mut output = if dir.join(".git").is_dir() {
            // Existing checkout: refresh remote refs only. The working
            // tree may hold in-progress work; never reposition or clean.
            checkout::run(&self.git, &["fetch", "origin"], &dir, true).await?;
            format!("{rel} already cloned; fetched origin")
        } else {
            checkout::ensure_cloned(&self.git, &https_url, &rel).await?;
            format!("Cloned to {rel}")
        };
        let _ = write!(output, " (use working_dir: \"{rel}\" with exec)");

        if dir.join(".envrc").exists() {
            // Trust check here is messaging only; warm_devshell re-gates.
            if origin_trusted(&dir, &self.git.trusted_repos).await {
                let (git, warm_dir) = (self.git.clone(), dir.clone());
                tokio::spawn(async move { git.warm_devshell(&warm_dir).await });
                let _ = write!(
                    output,
                    "\nDetected .envrc — warming the devshell in the background; \
                     it will be available shortly."
                );
            } else {
                let _ = write!(
                    output,
                    "\n.envrc detected but {nwo} is not in git.trusted_repos; \
                     direnv/devshell disabled for this checkout."
                );
            }
        }

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_derives_owner_repo_layout() {
        let (https, nwo, rel) = target("https://github.com/owner/repo.git").unwrap();
        assert_eq!(https, "https://github.com/owner/repo.git");
        assert_eq!(nwo, "owner/repo");
        assert_eq!(rel, "projects/owner/repo");
    }

    #[test]
    fn target_rewrites_ssh_to_https() {
        let (https, _, rel) = target("git@github.com:owner/repo.git").unwrap();
        assert_eq!(https, "https://github.com/owner/repo.git");
        assert_eq!(rel, "projects/owner/repo");
    }

    #[test]
    fn target_rejects_url_without_owner_repo() {
        assert!(matches!(
            target("https://github.com/just-a-repo"),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn target_rejects_option_shaped_segments() {
        assert!(target("https://github.com/-flag/repo").is_err());
    }
}
