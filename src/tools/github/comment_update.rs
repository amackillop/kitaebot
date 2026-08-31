//! `github_comment_update` tool — edit one of the bot's own comments.
//!
//! Lets a plan revision land as an in-place edit of the original plan
//! comment (GitHub keeps the edit history visible) instead of a fresh
//! wall of text. Self-authored comments only, checked mechanically: a
//! write-scoped token can edit anyone's comments, and rewriting a
//! human's words must be impossible, not discouraged.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx};
use crate::error::ToolError;
use crate::tools::string_or_value_required;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository as `owner/repo`. Must be a configured repository.
    repo: String,
    /// Id of the comment to edit. Must be a comment the bot authored.
    #[serde(deserialize_with = "string_or_value_required")]
    comment_id: u64,
    /// The full replacement body.
    body: String,
}

pub struct CommentUpdate {
    pub api: GithubApi,
    /// Repos the bot may edit comments in — the `[git.repositories]` keys.
    pub repos: Vec<String>,
}

impl Tool for CommentUpdate {
    fn name(&self) -> &'static str {
        "github_comment_update"
    }

    fn description(&self) -> &'static str {
        "Replace the body of one of your own issue/PR comments (GitHub \
         shows the edit history). Use for revising a plan comment in \
         place; edits send no notification, so mention the update in \
         your reply."
    }

    fn parameters(&self) -> serde_json::Value {
        crate::tools::schema_of::<Args>()
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args).await
        })
    }
}

impl CommentUpdate {
    async fn run(&self, args: &Args) -> Result<String, ToolError> {
        if !self
            .repos
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&args.repo))
        {
            return Err(ToolError::Blocked {
                operation: format!("github_comment_update in {}", args.repo),
                guidance: format!(
                    "not a configured repository; comments can be edited in: {}",
                    self.repos.join(", ")
                ),
            });
        }
        let client = self.api.client();
        let me = client.user().await?.login;
        let comment = client.issue_comment(&args.repo, args.comment_id).await?;
        if comment.user.login != me {
            return Err(ToolError::Blocked {
                operation: format!(
                    "github_comment_update on comment {} by {}",
                    args.comment_id, comment.user.login,
                ),
                guidance: "only your own comments can be edited".into(),
            });
        }
        let updated = client
            .update_issue_comment(&args.repo, args.comment_id, &args.body)
            .await?;
        Ok(format!(
            "Updated comment {}; the previous content stays visible in \
             its edit history. Edits send no notification — mention the \
             update in your reply.",
            updated.id,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_api;

    fn tool(api: GithubApi) -> CommentUpdate {
        CommentUpdate {
            api,
            repos: vec!["owner/repo".into()],
        }
    }

    fn args(repo: &str) -> Args {
        Args {
            repo: repo.into(),
            comment_id: 7,
            body: "revised plan".into(),
        }
    }

    fn comment_json(author: &str) -> Vec<u8> {
        format!(
            r#"{{"id":7,"user":{{"login":"{author}"}},"body":"old",
                "created_at":"2026-01-01T00:00:00Z"}}"#
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn updates_own_comment() {
        let api = stub_api(|method, path, body| match (method, path.as_str()) {
            ("GET", "user") => br#"{"login":"kitaebot"}"#.to_vec(),
            ("GET", "repos/owner/repo/issues/comments/7") => comment_json("kitaebot"),
            ("PATCH", "repos/owner/repo/issues/comments/7") => {
                let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
                assert_eq!(payload["body"], "revised plan");
                comment_json("kitaebot")
            }
            other => panic!("unexpected request: {other:?}"),
        });

        let out = tool(api).run(&args("owner/repo")).await.unwrap();
        assert!(out.contains("Updated comment 7"), "{out}");
    }

    #[tokio::test]
    async fn refuses_someone_elses_comment() {
        let api = stub_api(|method, path, _| match (method, path.as_str()) {
            ("GET", "user") => br#"{"login":"kitaebot"}"#.to_vec(),
            ("GET", "repos/owner/repo/issues/comments/7") => comment_json("alice"),
            other => panic!("must not reach {other:?}"),
        });

        let err = tool(api).run(&args("owner/repo")).await.unwrap_err();
        assert!(
            matches!(err, ToolError::Blocked { ref guidance, .. } if guidance.contains("own comments")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn refuses_unconfigured_repo() {
        let api = stub_api(|_, _, _| panic!("must not reach the API"));

        let err = tool(api).run(&args("evil/repo")).await.unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }), "{err}");
    }
}
