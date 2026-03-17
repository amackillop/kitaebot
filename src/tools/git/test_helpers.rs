//! Shared test infrastructure for git tool tests.

use super::git_cli::GitCli;
use crate::secrets::Secret;
use crate::tools::DirenvCache;

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
        GitCli::new(Secret::test("fake"), &path, DirenvCache::new()),
        repo.to_string(),
    )
}
