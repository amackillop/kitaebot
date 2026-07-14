//! URL handling for git clone operations.

use crate::error::ToolError;

/// Convert SSH-style URLs to HTTPS. Passes HTTPS URLs through unchanged.
///
/// Handles:
/// - `git@github.com:owner/repo.git` → `https://github.com/owner/repo.git`
/// - `ssh://git@github.com/owner/repo.git` → `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo.git` → unchanged
pub(crate) fn to_https_url(url: &str) -> Result<String, ToolError> {
    // Already HTTPS
    if url.starts_with("https://") {
        return Ok(url.to_string());
    }

    // SCP-style: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
    {
        return Ok(format!("https://{host}/{path}"));
    }

    // ssh://git@github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        return Ok(format!("https://{rest}"));
    }

    Err(ToolError::InvalidArguments(format!(
        "unsupported URL scheme: {url}"
    )))
}

/// Extract `owner/repo` from an HTTPS URL.
///
/// `https://github.com/owner/repo.git` → `owner/repo`. The host is
/// ignored: which hosts are reachable at all is bounded by the egress
/// allowlist. Returns `None` unless the path is exactly two segments.
pub(crate) fn extract_nwo(url: &str) -> Option<String> {
    let path = url
        .strip_prefix("https://")?
        .trim_end_matches('/')
        .trim_end_matches(".git");

    match path.split('/').collect::<Vec<_>>().as_slice() {
        [_host, owner, repo] if !owner.is_empty() && !repo.is_empty() => {
            Some(format!("{owner}/{repo}"))
        }
        _ => None,
    }
}

/// Check whether `nwo` (`owner/repo`) is in the trusted list.
///
/// Entries are exact `owner/repo` matches or `owner/*` wildcards,
/// case-insensitive.
pub(crate) fn is_trusted_repo(nwo: &str, trusted: &[String]) -> bool {
    let Some((owner, _)) = nwo.split_once('/') else {
        return false;
    };
    trusted.iter().any(|entry| {
        entry.eq_ignore_ascii_case(nwo)
            || entry
                .strip_suffix("/*")
                .is_some_and(|o| o.eq_ignore_ascii_case(owner))
    })
}

/// Validate a user-provided directory name.
///
/// Rejects path traversal, absolute paths, and slashes.
pub(crate) fn validate_name(name: &str) -> Result<&str, ToolError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.starts_with('.')
        || name.starts_with('-')
    {
        return Err(ToolError::InvalidArguments(format!(
            "invalid directory name: {name}"
        )));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── URL conversion ──────────────────────────────────────────────

    #[test]
    fn https_url_passthrough() {
        let url = "https://github.com/owner/repo.git";
        assert_eq!(to_https_url(url).unwrap(), url);
    }

    #[test]
    fn scp_style_to_https() {
        assert_eq!(
            to_https_url("git@github.com:owner/repo.git").unwrap(),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn ssh_url_to_https() {
        assert_eq!(
            to_https_url("ssh://git@github.com/owner/repo.git").unwrap(),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn unsupported_scheme_rejected() {
        assert!(to_https_url("ftp://example.com/repo").is_err());
    }

    // ── Name-with-owner extraction ──────────────────────────────────

    #[test]
    fn extract_nwo_with_git_suffix() {
        assert_eq!(
            extract_nwo("https://github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
    }

    #[test]
    fn extract_nwo_without_git_suffix() {
        assert_eq!(
            extract_nwo("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
    }

    #[test]
    fn extract_nwo_trailing_slash() {
        assert_eq!(
            extract_nwo("https://github.com/owner/repo/").as_deref(),
            Some("owner/repo")
        );
    }

    #[test]
    fn extract_nwo_rejects_wrong_segment_count() {
        assert_eq!(extract_nwo("https://github.com/repo"), None);
        assert_eq!(extract_nwo("https://github.com/a/b/c"), None);
        assert_eq!(extract_nwo("not-a-url"), None);
    }

    // ── Trust matching ──────────────────────────────────────────────

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn trusted_repo_exact_match() {
        assert!(is_trusted_repo("owner/repo", &list(&["owner/repo"])));
        assert!(!is_trusted_repo("owner/other", &list(&["owner/repo"])));
        assert!(!is_trusted_repo("owner/repo", &[]));
    }

    #[test]
    fn trusted_repo_case_insensitive() {
        assert!(is_trusted_repo("Owner/Repo", &list(&["owner/repo"])));
        assert!(is_trusted_repo("owner/repo", &list(&["OWNER/REPO"])));
    }

    #[test]
    fn trusted_repo_owner_wildcard() {
        let trusted = list(&["owner/*"]);
        assert!(is_trusted_repo("owner/anything", &trusted));
        assert!(is_trusted_repo("OWNER/anything", &trusted));
        assert!(!is_trusted_repo("other/anything", &trusted));
    }

    #[test]
    fn trusted_repo_rejects_malformed_nwo() {
        assert!(!is_trusted_repo(
            "no-slash",
            &list(&["no-slash", "owner/*"])
        ));
    }

    // ── Name validation ─────────────────────────────────────────────

    #[test]
    fn valid_name() {
        assert_eq!(validate_name("myrepo").unwrap(), "myrepo");
        assert_eq!(validate_name("my_repo").unwrap(), "my_repo");
        assert_eq!(validate_name("my-repo").unwrap(), "my-repo");
    }

    #[test]
    fn reject_traversal() {
        assert!(validate_name("..").is_err());
        assert!(validate_name("../escape").is_err());
    }

    #[test]
    fn reject_slashes() {
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
    }

    #[test]
    fn reject_hidden() {
        assert!(validate_name(".hidden").is_err());
    }

    #[test]
    fn reject_dash_prefix() {
        assert!(validate_name("-flag").is_err());
    }

    #[test]
    fn reject_empty() {
        assert!(validate_name("").is_err());
    }
}
