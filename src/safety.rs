//! Output safety layer.
//!
//! Scans tool output for secret-shaped spans, redacts each span in
//! place, and wraps the result in XML tags before it enters the LLM
//! conversation context.
//!
//! Patterns use regex with minimum-entropy requirements so that reading
//! source code containing pattern *definitions* (e.g. `"sk-"`) does not
//! trigger false positives.

use std::sync::LazyLock;

use regex::{Regex, RegexSet};

/// A secret-detection pattern: regex source + human-readable label.
struct LeakPattern {
    regex: &'static str,
    name: &'static str,
}

/// Secret patterns.  Each regex requires enough structure beyond the bare
/// prefix to match real secrets, not mentions of the prefix in source code.
const PATTERNS: &[LeakPattern] = &[
    LeakPattern {
        regex: r"sk-ant-[a-zA-Z0-9_-]{20,}",
        name: "Anthropic API key",
    },
    LeakPattern {
        regex: r"sk-[a-zA-Z0-9_-]{20,}",
        name: "OpenAI API key",
    },
    LeakPattern {
        regex: r"ghp_[a-zA-Z0-9]{30,}",
        name: "GitHub PAT",
    },
    LeakPattern {
        regex: r"gho_[a-zA-Z0-9]{30,}",
        name: "GitHub OAuth",
    },
    LeakPattern {
        regex: r"ghs_[a-zA-Z0-9]{30,}",
        name: "GitHub server token",
    },
    LeakPattern {
        regex: r"AKIA[0-9A-Z]{16}",
        name: "AWS access key",
    },
    LeakPattern {
        // Spans through the END marker (or end of output): redacting
        // only the header would leave the key body readable.
        regex: r"(?s)-----BEGIN [A-Z ]+PRIVATE KEY-----.*?(?:-----END [A-Z ]+PRIVATE KEY-----|\z)",
        name: "Private key",
    },
    LeakPattern {
        regex: r"postgres://\S+:\S+@",
        name: "PostgreSQL connection string",
    },
    LeakPattern {
        regex: r"mysql://\S+:\S+@",
        name: "MySQL connection string",
    },
    LeakPattern {
        regex: r"mongodb(\+srv)?://\S+:\S+@",
        name: "MongoDB connection string",
    },
    LeakPattern {
        regex: r"redis://\S+:\S+@",
        name: "Redis connection string",
    },
];

/// Compiled pattern set for the single-pass clean-output fast path.
static LEAK_SET: LazyLock<RegexSet> =
    LazyLock::new(|| crate::text::static_regex_set(PATTERNS.iter().map(|p| p.regex)));

/// Per-pattern regexes for span replacement; only run on a set hit.
static LEAK_REGEXES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    PATTERNS
        .iter()
        .map(|p| crate::text::static_regex(p.regex))
        .collect()
});

/// Tool output after the safety pass: wrapped content with every
/// secret-shaped span replaced by a `[REDACTED: {pattern}]` marker.
pub struct CheckedOutput {
    pub wrapped: String,
    /// Names of the patterns actually redacted, in pattern order.
    pub redactions: Vec<&'static str>,
}

/// Redact secret-shaped spans and wrap tool output in XML tags.
///
/// A span is redacted, never the whole output: one key-shaped string
/// must not cost the model the rest of a file, and a withheld output
/// only invites reconstructing the content through other commands.
/// The secret itself never enters the conversation either way.
pub fn check_tool_output(tool_name: &str, output: &str) -> CheckedOutput {
    let matches = LEAK_SET.matches(output);
    let mut redactions = Vec::new();
    let mut content = output.to_string();
    for idx in &matches {
        let marker = format!("[REDACTED: {}]", PATTERNS[idx].name);
        let replaced = LEAK_REGEXES[idx].replace_all(&content, marker.as_str());
        // An earlier pattern can consume an overlapping match (an
        // Anthropic key is also OpenAI-shaped); count real changes only.
        if replaced != content {
            redactions.push(PATTERNS[idx].name);
            content = replaced.into_owned();
        }
    }
    CheckedOutput {
        wrapped: format!("<tool_output name=\"{tool_name}\">\n{content}\n</tool_output>"),
        redactions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(tool: &str, output: &str) -> String {
        let checked = check_tool_output(tool, output);
        assert!(checked.redactions.is_empty(), "unexpected redaction");
        checked.wrapped
    }

    #[test]
    fn clean_output_wrapped() {
        let result = clean("exec", "hello world");
        assert!(result.contains("<tool_output name=\"exec\">"));
        assert!(result.contains("hello world"));
        assert!(result.contains("</tool_output>"));
    }

    #[test]
    fn wrapping_format() {
        let result = clean("my_tool", "some output");
        assert_eq!(
            result,
            "<tool_output name=\"my_tool\">\nsome output\n</tool_output>"
        );
    }

    // --- Real secrets must be redacted ---

    #[test]
    fn redacts_openai_key_span_only() {
        let checked = check_tool_output(
            "exec",
            "key is sk-proj-abc123def456ghi789jkl012, rest stays",
        );
        assert_eq!(checked.redactions, ["OpenAI API key"]);
        assert!(!checked.wrapped.contains("sk-proj-abc123def456ghi789jkl012"));
        assert!(
            checked
                .wrapped
                .contains("key is [REDACTED: OpenAI API key], rest stays")
        );
    }

    /// An Anthropic key is also OpenAI-shaped; the more specific
    /// pattern consumes it and the broader one must not double-report.
    #[test]
    fn redacts_anthropic_key_under_its_own_name() {
        let checked = check_tool_output("exec", "key is sk-ant-api03-abc123def456ghi789jkl012");
        assert_eq!(checked.redactions, ["Anthropic API key"]);
        assert!(checked.wrapped.contains("[REDACTED: Anthropic API key]"));
        assert!(!checked.wrapped.contains("sk-ant-api03"));
    }

    #[test]
    fn redacts_github_pat() {
        let checked = check_tool_output("exec", "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
        assert_eq!(checked.redactions, ["GitHub PAT"]);
        assert!(!checked.wrapped.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
    }

    #[test]
    fn redacts_aws_key() {
        let checked = check_tool_output("exec", "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(checked.redactions, ["AWS access key"]);
        assert!(!checked.wrapped.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_private_key_through_end_marker() {
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow...base64...\n-----END RSA PRIVATE KEY-----\nafter";
        let checked = check_tool_output("exec", pem);
        assert_eq!(checked.redactions, ["Private key"]);
        assert!(!checked.wrapped.contains("base64"));
        assert!(
            checked
                .wrapped
                .contains("before\n[REDACTED: Private key]\nafter")
        );
    }

    /// A header with no END marker redacts to end of output — the key
    /// body must not survive a truncated read.
    #[test]
    fn redacts_unterminated_private_key_to_end() {
        let pem = "before\n-----BEGIN EC PRIVATE KEY-----\nMIIEow...base64...";
        let checked = check_tool_output("exec", pem);
        assert_eq!(checked.redactions, ["Private key"]);
        assert!(!checked.wrapped.contains("base64"));
        assert!(checked.wrapped.contains("before\n[REDACTED: Private key]"));
    }

    #[test]
    fn redacts_postgres_connection_string() {
        let checked = check_tool_output("exec", "postgres://admin:s3cret@db.host/mydb");
        assert_eq!(checked.redactions, ["PostgreSQL connection string"]);
        assert!(!checked.wrapped.contains("s3cret"));
    }

    #[test]
    fn redacts_mongodb_srv_connection_string() {
        let checked = check_tool_output("exec", "mongodb+srv://user:pass@cluster.mongodb.net/db");
        assert_eq!(checked.redactions, ["MongoDB connection string"]);
        assert!(!checked.wrapped.contains("user:pass"));
    }

    #[test]
    fn redacts_every_occurrence_of_a_pattern() {
        let checked = check_tool_output(
            "exec",
            "a: sk-proj-abc123def456ghi789jkl012 b: sk-proj-zyx987wvu654tsr321qpo000",
        );
        assert_eq!(checked.redactions, ["OpenAI API key"]);
        assert!(!checked.wrapped.contains("sk-proj-"));
        assert_eq!(checked.wrapped.matches("[REDACTED:").count(), 2);
    }

    // --- Short prefixes in source code must NOT trigger ---

    #[test]
    fn no_false_positive_bare_sk_prefix() {
        // String literal mentioning the prefix without a real key suffix.
        clean("file_read", r#"("sk-", "OpenAI API key")"#);
    }

    #[test]
    fn no_false_positive_bare_sk_ant_prefix() {
        clean("file_read", r#"("sk-ant-", "Anthropic API key")"#);
    }

    #[test]
    fn no_false_positive_bare_ghp_prefix() {
        clean("file_read", r#"("ghp_", "GitHub PAT")"#);
    }

    #[test]
    fn no_false_positive_bare_connection_scheme() {
        // Bare scheme without credentials.
        clean("file_read", r"postgres://localhost/db");
    }

    #[test]
    fn no_false_positive_begin_without_private_key() {
        // Certificate header (public, not secret).
        clean("file_read", "-----BEGIN CERTIFICATE-----");
    }

    #[test]
    fn no_false_positive_begin_prefix_only() {
        clean("file_read", r#"("-----BEGIN", "Private key header")"#);
    }

    /// Source code containing pattern *definitions* (short prefixes in string
    /// literals) must not trigger detection — this is the original bug.
    #[test]
    fn no_false_positive_on_pattern_definitions() {
        let source = r#"
            const PATTERNS: &[(&str, &str)] = &[
                ("sk-ant-", "Anthropic API key"),
                ("sk-", "OpenAI API key"),
                ("ghp_", "GitHub PAT"),
                ("gho_", "GitHub OAuth"),
                ("ghs_", "GitHub server token"),
                ("AKIA", "AWS access key"),
                ("-----BEGIN", "Private key header"),
                ("postgres://", "PostgreSQL connection string"),
                ("mysql://", "MySQL connection string"),
                ("mongodb://", "MongoDB connection string"),
                ("redis://", "Redis connection string"),
            ];
        "#;
        clean("file_read", source);
    }
}
