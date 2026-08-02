//! Shared test infrastructure for GitHub tool tests.

use super::GithubApi;
use super::gh_cli::GhCli;
use crate::clients::RawResponse;
use crate::clients::github::GithubClient;
use crate::secrets::Secret;

/// Build a `GhCli` backed by a fake `.git` dir for testing.
///
/// Returns `(GhCli, repo_dir_name)`.
#[allow(deprecated)] // tempfile::TempDir::into_path
pub fn stub_gh_cli_with_repo() -> (GhCli, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = "projects/r";
    std::fs::create_dir_all(dir.path().join(repo).join(".git")).unwrap();
    let path = dir.into_path();
    (GhCli::new(Secret::test("fake"), &path), repo.to_string())
}

/// Build a `GithubApi` whose `projects/r` checkout has an origin
/// remote pointing at `nwo`, and whose HTTP layer is `handler`
/// (called with method, path, body; returns the 200 response body).
#[allow(deprecated)] // tempfile::TempDir::into_path
pub fn stub_api_with_repo<F>(nwo: &str, handler: F) -> GithubApi
where
    F: Fn(&'static str, String, Option<Vec<u8>>) -> Vec<u8> + Send + Sync + 'static,
{
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("projects/r");
    std::fs::create_dir_all(&repo).unwrap();
    let origin = format!("https://github.com/{nwo}.git");
    for args in [vec!["init"], vec!["remote", "add", "origin", &origin]] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }
    let handler = std::sync::Arc::new(handler);
    let client = GithubClient::from_fn(move |method, path, body| {
        let handler = handler.clone();
        async move {
            Ok(RawResponse {
                status: 200,
                body: handler(method, path, body),
            })
        }
    });
    GithubApi::new(client, dir.into_path())
}
