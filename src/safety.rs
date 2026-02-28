//! Output safety layer.
//!
//! Checks tool output for leaked secrets and wraps clean output in XML tags
//! before it enters the LLM conversation context.

use crate::error::SafetyError;

/// Secret patterns to scan for, ordered so more specific prefixes come first.
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

/// Check tool output for leaked secrets and wrap in XML tags.
///
/// Short-circuits on the first pattern match and returns an error.
/// Clean output is wrapped as `<tool_output name="...">...</tool_output>`.
pub fn check_tool_output(tool_name: &str, output: &str) -> Result<String, SafetyError> {
    for &(pattern, name) in PATTERNS {
        if output.contains(pattern) {
            return Err(SafetyError::LeakDetected {
                pattern_name: name.to_string(),
            });
        }
    }

    Ok(format!(
        "<tool_output name=\"{tool_name}\">\n{output}\n</tool_output>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_output_wrapped() {
        let result = check_tool_output("exec", "hello world").unwrap();
        assert!(result.contains("<tool_output name=\"exec\">"));
        assert!(result.contains("hello world"));
        assert!(result.contains("</tool_output>"));
    }

    #[test]
    fn test_wrapping_format() {
        let result = check_tool_output("my_tool", "some output").unwrap();
        assert_eq!(
            result,
            "<tool_output name=\"my_tool\">\nsome output\n</tool_output>"
        );
    }

    #[test]
    fn test_leak_detected_sk() {
        let err = check_tool_output("exec", "key is sk-1234").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "OpenAI API key"
        ));
    }

    #[test]
    fn test_leak_detected_anthropic() {
        let err = check_tool_output("exec", "key is sk-ant-xxx").unwrap_err();
        // Must match Anthropic, not OpenAI, because sk-ant- is checked first.
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "Anthropic API key"
        ));
    }

    #[test]
    fn test_leak_detected_aws() {
        let err = check_tool_output("exec", "AKIAIOSFODNN7EXAMPLE").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "AWS access key"
        ));
    }

    #[test]
    fn test_leak_detected_private_key() {
        let err = check_tool_output("exec", "-----BEGIN RSA PRIVATE KEY-----").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "Private key header"
        ));
    }

    #[test]
    fn test_leak_detected_connection_string() {
        let err = check_tool_output("exec", "postgres://user:pass@localhost/db").unwrap_err();
        assert!(matches!(
            err,
            SafetyError::LeakDetected { ref pattern_name } if pattern_name == "PostgreSQL connection string"
        ));
    }
}
