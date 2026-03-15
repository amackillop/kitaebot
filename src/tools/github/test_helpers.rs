//! Shared test infrastructure for GitHub tool tests.

use super::gh_cli::GhCli;
use super::git_cli::GitCli;
use crate::secrets::Secret;

/// Build a `GitCli` backed by a fake `.git` dir for testing.
///
/// Returns `(GitCli, repo_dir_name)`.
#[allow(deprecated)] // tempfile::TempDir::into_path
pub fn stub_git_cli_with_repo() -> (GitCli, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = "projects/r";
    std::fs::create_dir_all(dir.path().join(repo).join(".git")).unwrap();
    let path = dir.into_path();
    (
        GitCli::new(Secret::test("fake"), &path, vec![]),
        repo.to_string(),
    )
}

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
