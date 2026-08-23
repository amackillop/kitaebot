//! The trust model both GitHub channels enforce.

use crate::config::GithubConfig;

/// Who the channels act on: the owner, listed human users, and listed
/// bot apps. Bot logins carry a `[bot]` suffix in the REST API but not
/// in GraphQL, so the suffix is stripped before matching the bot list.
pub(crate) struct Trust<'a> {
    owner: &'a str,
    users: &'a [String],
    bots: &'a [String],
}

impl<'a> Trust<'a> {
    pub(crate) fn new(config: &'a GithubConfig) -> Self {
        Self {
            owner: &config.owner,
            users: &config.trusted_users,
            bots: &config.trusted_bots,
        }
    }

    pub(crate) fn allows(&self, login: &str) -> bool {
        if login.eq_ignore_ascii_case(self.owner) {
            return true;
        }
        if self.users.iter().any(|u| u.eq_ignore_ascii_case(login)) {
            return true;
        }
        let bot = login.strip_suffix("[bot]").unwrap_or(login);
        self.bots.iter().any(|b| b.eq_ignore_ascii_case(bot))
    }
}

/// Test constructor bypassing config assembly; fields stay private.
#[cfg(test)]
pub(super) fn stub<'a>(owner: &'a str, users: &'a [String], bots: &'a [String]) -> Trust<'a> {
    Trust { owner, users, bots }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_owner_always_allowed() {
        let t = stub("alice", &[], &[]);
        assert!(t.allows("alice"));
        assert!(t.allows("ALICE"));
    }

    #[test]
    fn trust_filters_untrusted_users() {
        let users = vec!["bob".to_string(), "charlie".to_string()];
        let t = stub("alice", &users, &[]);
        assert!(t.allows("alice"));
        assert!(t.allows("bob"));
        assert!(t.allows("charlie"));
        assert!(!t.allows("eve"));
        assert!(!t.allows("mallory"));
    }

    #[test]
    fn trust_case_insensitive() {
        let users = vec!["BOB".to_string()];
        let t = stub("Alice", &users, &[]);
        assert!(t.allows("alice"));
        assert!(t.allows("ALICE"));
        assert!(t.allows("bob"));
        assert!(t.allows("Bob"));
    }

    #[test]
    fn trust_allows_bots_ignoring_bot_suffix() {
        let bots = vec!["chatgpt-codex-connector".to_string()];
        let t = stub("alice", &[], &bots);
        // GraphQL exposes the bare slug; REST appends `[bot]`.
        assert!(t.allows("chatgpt-codex-connector"));
        assert!(t.allows("chatgpt-codex-connector[bot]"));
        assert!(t.allows("Chatgpt-Codex-Connector[bot]"));
        assert!(!t.allows("some-other-bot[bot]"));
        assert!(!t.allows("mallory"));
    }
}
