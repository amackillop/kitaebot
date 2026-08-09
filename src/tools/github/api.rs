//! `github_api` tool — REST escape hatch scoped to one repo.
//!
//! Every path is prefixed with `repos/<owner>/<repo>/` (resolved from
//! the checkout's origin), so a request cannot leave the repo, and
//! the first path segment must name an allowed resource.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx};
use crate::error::ToolError;
use crate::tools::string_or_value;

/// Resources the model may touch.
const ALLOWED_RESOURCES: &[&str] = &["issues", "labels", "milestones", "pulls", "releases"];

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
enum Method {
    Delete,
    Get,
    Patch,
    Post,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "DELETE",
            Self::Get => "GET",
            Self::Patch => "PATCH",
            Self::Post => "POST",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// HTTP method.
    method: Method,
    /// Path under `repos/<owner>/<repo>/`, e.g. `issues/42/comments`
    /// or `pulls?state=open`. Must start with one of: issues, labels,
    /// milestones, pulls, releases.
    path: String,
    /// JSON body for POST/PATCH.
    #[serde(default, deserialize_with = "string_or_value")]
    body: Option<serde_json::Value>,
}

pub struct Api(pub GithubApi);

impl Tool for Api {
    fn name(&self) -> &'static str {
        "github_api"
    }

    fn description(&self) -> &'static str {
        "Call the GitHub REST API on a repo (for operations not covered by dedicated tools)"
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
            self.run(&args).await
        })
    }
}

/// Reject paths that could escape the repo prefix or name a resource
/// outside the allowlist.
fn validate_path(path: &str) -> Result<(), ToolError> {
    let resource = path.split(['/', '?']).next().unwrap_or("");
    if !ALLOWED_RESOURCES.contains(&resource) {
        return Err(ToolError::Blocked {
            operation: format!("github_api path {path}"),
            guidance: format!(
                "the path must start with one of: {}",
                ALLOWED_RESOURCES.join(", ")
            ),
        });
    }
    // '%' is blocked wholesale: percent-encoding can smuggle a '/'
    // past the segment check, and none of the allowed resources
    // need it.
    if path.contains(['#', '%']) || path.split(['/', '?']).any(|segment| segment == "..") {
        return Err(ToolError::InvalidArguments(
            "path may not contain '..', '#', or '%'".into(),
        ));
    }
    Ok(())
}

impl Api {
    async fn run(&self, args: &Args) -> Result<String, ToolError> {
        validate_path(&args.path)?;
        let nwo = self.0.nwo(&args.repo_dir).await?;
        let body = args
            .body
            .as_ref()
            .map(|value| {
                serde_json::to_vec(value).map_err(|e| ToolError::InvalidArguments(e.to_string()))
            })
            .transpose()?;
        let raw = self
            .0
            .client()
            .raw(
                args.method.as_str(),
                format!("repos/{nwo}/{}", args.path),
                body,
            )
            .await?;
        let text = String::from_utf8_lossy(&raw.body);
        if !(200..=299).contains(&raw.status) {
            return Err(ToolError::Github(crate::error::GithubError::Api {
                status: raw.status,
                body: text.into_owned(),
            }));
        }
        if text.trim().is_empty() {
            return Ok(format!("OK ({})", raw.status));
        }
        Ok(text.into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_api_with_repo;

    #[test]
    fn rejects_resources_outside_the_allowlist() {
        for path in ["", "collaborators", "hooks/1", "../other/pulls", "keys?x=1"] {
            assert!(
                matches!(validate_path(path), Err(ToolError::Blocked { .. })),
                "{path} should be blocked",
            );
        }
    }

    #[test]
    fn rejects_traversal_and_fragments() {
        for path in ["issues/../../other", "pulls/1#frag", "issues?q=..%2f.."] {
            assert!(validate_path(path).is_err(), "{path} should be rejected");
        }
    }

    #[test]
    fn accepts_allowed_resources() {
        for path in [
            "issues/42/comments",
            "pulls?state=open",
            "releases/latest",
            "labels",
            "milestones/3",
        ] {
            assert!(validate_path(path).is_ok(), "{path} should be allowed");
        }
    }

    #[tokio::test]
    async fn prefixes_the_origin_repo() {
        let api = stub_api_with_repo("owner/repo", |method, path, body| {
            assert_eq!(method, "POST");
            assert_eq!(path, "repos/owner/repo/issues/42/comments");
            let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            assert_eq!(payload["body"], "done");
            br#"{"id":1}"#.to_vec()
        });
        let tool = Api(api);
        let out = tool
            .run(&Args {
                repo_dir: "projects/r".into(),
                method: Method::Post,
                path: "issues/42/comments".into(),
                body: Some(serde_json::json!({"body": "done"})),
            })
            .await
            .unwrap();
        assert_eq!(out, r#"{"id":1}"#);
    }

    #[tokio::test]
    async fn stringified_body_delivered_as_object() {
        let api = stub_api_with_repo("owner/repo", |method, _path, body| {
            assert_eq!(method, "POST");
            let raw = body.unwrap();
            // The raw bytes must be a JSON object, not a JSON string.
            assert_eq!(raw[0], b'{', "body must start with '{{', got: {raw:?}");
            let payload: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(payload["body"], "done");
            br#"{"id":1}"#.to_vec()
        });
        let tool = Api(api);
        // Simulate the LLM passing body as a stringified JSON string.
        let args: Args = serde_json::from_value(serde_json::json!({
            "repo_dir": "projects/r",
            "method": "POST",
            "path": "issues/42/comments",
            "body": "{\"body\": \"done\"}"
        }))
        .unwrap();
        let out = tool.run(&args).await.unwrap();
        assert_eq!(out, r#"{"id":1}"#);
    }

    #[tokio::test]
    async fn object_body_delivered_as_object() {
        let api = stub_api_with_repo("owner/repo", |method, _path, body| {
            assert_eq!(method, "POST");
            let raw = body.unwrap();
            assert_eq!(raw[0], b'{', "body must start with '{{', got: {raw:?}");
            let payload: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(payload["body"], "done");
            br#"{"id":1}"#.to_vec()
        });
        let tool = Api(api);
        let args: Args = serde_json::from_value(serde_json::json!({
            "repo_dir": "projects/r",
            "method": "POST",
            "path": "issues/42/comments",
            "body": {"body": "done"}
        }))
        .unwrap();
        let out = tool.run(&args).await.unwrap();
        assert_eq!(out, r#"{"id":1}"#);
    }

    #[tokio::test]
    async fn absent_body_delivered_as_none() {
        let api = stub_api_with_repo("owner/repo", |method, _path, body| {
            assert_eq!(method, "GET");
            assert!(body.is_none(), "GET should have no body");
            br"[]".to_vec()
        });
        let tool = Api(api);
        let args: Args = serde_json::from_value(serde_json::json!({
            "repo_dir": "projects/r",
            "method": "GET",
            "path": "issues?state=open"
        }))
        .unwrap();
        let out = tool.run(&args).await.unwrap();
        assert_eq!(out, r"[]");
    }
}
