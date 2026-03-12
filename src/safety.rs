//! Output safety layer.
//!
//! Checks tool output for leaked secrets and wraps clean output in XML tags
//! before it enters the LLM conversation context.
//!
//! Patterns use regex with minimum-entropy requirements so that reading
//! source code containing pattern *definitions* (e.g. `"sk-"`) does not
//! trigger false positives.

use std::sync::LazyLock;

use regex::RegexSet;

use crate::error::SafetyError;

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
        regex: r"-----BEGIN [A-Z ]+PRIVATE KEY-----",
        name: "Private key header",
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

/// Compiled pattern set for fast matching.
static LEAK_SET: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new(PATTERNS.iter().map(|p| p.regex)).expect("invalid leak pattern")
});

/// Check tool output for leaked secrets and wrap in XML tags.
///
/// Returns the first matching pattern as an error.
/// Clean output is wrapped as `<tool_output name="...">...</tool_output>`.
pub fn check_tool_output(tool_name: &str, output: &str) -> Result<String, SafetyError> {
    if let Some(idx) = LEAK_SET.matches(output).into_iter().next() {
        return Err(SafetyError::LeakDetected {
            pattern_name: PATTERNS[idx].name.to_string(),
        });
    }

    Ok(format!(
        "<tool_output name=\"{tool_name}\">\n{output}\n</tool_output>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_output_wrapped() {
        let result = check_tool_output("exec", "hello world").unwrap();
        assert!(result.contains("<tool_output name=\"exec\">"));
        assert!(result.contains("hello world"));
        assert!(result.contains("</tool_output>"));
    }

    #[test]
    fn wrapping_format() {
        let result = check_tool_output("my_tool", "some output").unwrap();
        assert_eq!(
            result,
            "<tool_output name=\"my_tool\">\nsome output\n</tool_output>"
        );
    }

    // --- Real secrets must be caught ---

    #[test]
    fn leak_openai_key() {
        let err = check_tool_output("exec", "key is sk-proj-abc123def456ghi789jkl012").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "OpenAI API key"
        ));
    }

    #[test]
    fn leak_anthropic_key() {
        let err =
            check_tool_output("exec", "key is sk-ant-api03-abc123def456ghi789jkl012").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "Anthropic API key"
        ));
    }

    #[test]
    fn leak_github_pat() {
        let err =
            check_tool_output("exec", "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "GitHub PAT"
        ));
    }

    #[test]
    fn leak_aws_key() {
        let err = check_tool_output("exec", "AKIAIOSFODNN7EXAMPLE").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "AWS access key"
        ));
    }

    #[test]
    fn leak_private_key_header() {
        let err = check_tool_output("exec", "-----BEGIN RSA PRIVATE KEY-----").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "Private key header"
        ));
    }

    #[test]
    fn leak_ec_private_key_header() {
        let err = check_tool_output("exec", "-----BEGIN EC PRIVATE KEY-----").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "Private key header"
        ));
    }

    #[test]
    fn leak_postgres_connection_string() {
        let err = check_tool_output("exec", "postgres://admin:s3cret@db.host/mydb").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "PostgreSQL connection string"
        ));
    }

    #[test]
    fn leak_mongodb_srv_connection_string() {
        let err = check_tool_output("exec", "mongodb+srv://user:pass@cluster.mongodb.net/db")
            .unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "MongoDB connection string"
        ));
    }

    // --- Short prefixes in source code must NOT trigger ---

    #[test]
    fn no_false_positive_bare_sk_prefix() {
        // String literal mentioning the prefix without a real key suffix.
        check_tool_output("file_read", r#"("sk-", "OpenAI API key")"#).unwrap();
    }

    #[test]
    fn no_false_positive_bare_sk_ant_prefix() {
        check_tool_output("file_read", r#"("sk-ant-", "Anthropic API key")"#).unwrap();
    }

    #[test]
    fn no_false_positive_bare_ghp_prefix() {
        check_tool_output("file_read", r#"("ghp_", "GitHub PAT")"#).unwrap();
    }

    #[test]
    fn no_false_positive_bare_connection_scheme() {
        // Bare scheme without credentials.
        check_tool_output("file_read", r"postgres://localhost/db").unwrap();
    }

    #[test]
    fn no_false_positive_begin_without_private_key() {
        // Certificate header (public, not secret).
        check_tool_output("file_read", "-----BEGIN CERTIFICATE-----").unwrap();
    }

    #[test]
    fn no_false_positive_begin_prefix_only() {
        check_tool_output("file_read", r#"("-----BEGIN", "Private key header")"#).unwrap();
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
        check_tool_output("file_read", source).unwrap();
    }
}
