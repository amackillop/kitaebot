//! `github_pr_list` tool — list pull requests.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx};
use crate::clients::github::PullSummary;
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// Filter by state: `"open"` (default), `"closed"`, `"merged"`, `"all"`.
    state: Option<String>,
}

pub struct PrList(pub GithubApi);

impl Tool for PrList {
    fn name(&self) -> &'static str {
        "github_pr_list"
    }

    fn description(&self) -> &'static str {
        "List pull requests"
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
            self.run(&args.repo_dir, args.state.as_deref()).await
        })
    }
}

/// The REST state to fetch for a requested filter. `merged` is not a
/// REST state: fetch `closed` and filter on `merged_at`.
fn rest_state(state: &str) -> Result<&'static str, ToolError> {
    match state {
        "all" => Ok("all"),
        "closed" | "merged" => Ok("closed"),
        "open" => Ok("open"),
        other => Err(ToolError::InvalidArguments(format!(
            "invalid state: {other} (expected one of: open, closed, merged, all)"
        ))),
    }
}

/// Display state: merged PRs are `closed` in REST, distinguished by
/// `merged_at`.
fn display_state(pr: &PullSummary) -> &'static str {
    match (pr.state.as_str(), pr.merged_at.is_some()) {
        (_, true) => "MERGED",
        ("open", _) => "OPEN",
        _ => "CLOSED",
    }
}

impl PrList {
    /// Pure: filter to the requested state and format for display.
    fn format_output(prs: &[PullSummary], state: &str) -> String {
        prs.iter()
            .filter(|pr| state != "merged" || pr.merged_at.is_some())
            .map(|pr| {
                format!(
                    "#{} {} [{}]\n  {}",
                    pr.number,
                    pr.title,
                    display_state(pr),
                    pr.html_url
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn run(&self, repo_dir: &str, state: Option<&str>) -> Result<String, ToolError> {
        let state = state.unwrap_or("open");
        let nwo = self.0.nwo(repo_dir).await?;
        let prs = self.0.client().pulls(&nwo, rest_state(state)?).await?;

        let output = Self::format_output(&prs, state);
        if output.is_empty() {
            return Ok(format!("No {state} pull requests."));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, state: &str, merged_at: Option<&str>) -> PullSummary {
        PullSummary {
            number,
            title: format!("PR {number}"),
            state: state.into(),
            html_url: format!("https://github.com/o/r/pull/{number}"),
            merged_at: merged_at.map(String::from),
        }
    }

    #[test]
    fn rejects_invalid_state() {
        assert!(matches!(
            rest_state("bogus"),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn merged_fetches_closed() {
        assert_eq!(rest_state("merged").unwrap(), "closed");
        assert_eq!(rest_state("closed").unwrap(), "closed");
    }

    #[test]
    fn formats_and_labels_states() {
        let prs = vec![
            pr(1, "open", None),
            pr(2, "closed", Some("2025-01-15T10:00:00Z")),
            pr(3, "closed", None),
        ];
        let result = PrList::format_output(&prs, "all");
        assert_eq!(
            result,
            "\
#1 PR 1 [OPEN]
  https://github.com/o/r/pull/1
#2 PR 2 [MERGED]
  https://github.com/o/r/pull/2
#3 PR 3 [CLOSED]
  https://github.com/o/r/pull/3"
        );
    }

    #[test]
    fn merged_filter_drops_unmerged_closed() {
        let prs = vec![
            pr(2, "closed", Some("2025-01-15T10:00:00Z")),
            pr(3, "closed", None),
        ];
        let result = PrList::format_output(&prs, "merged");
        assert!(result.contains("#2"));
        assert!(!result.contains("#3"));
    }
}
