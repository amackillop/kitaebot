//! Three-level summarization escalation.
//!
//! When LCM compacts a chunk of context, it tries up to three
//! strategies in order. A strategy "fails" when the LLM call errors or
//! when its output is no smaller than its input (no point keeping a
//! summary that costs as many tokens as the originals). Escalation
//! falls through to the next level, and the deterministic level is
//! guaranteed to converge.
//!
//! | Level | Strategy                                          | LLM? |
//! |-------|---------------------------------------------------|------|
//! | 1     | Prose summary preserving specifics                | yes  |
//! | 2     | Terse bullets, decisions and outcomes only        | yes  |
//! | 3     | Truncate raw text to a fixed token budget         | no   |
//!
//! See spec 14 §"Three-Level Summarization Escalation". This module is
//! intentionally a pure function over [`SummarizeFn`] — it has no
//! database plumbing, so unit tests run against canned mocks.
//!
//! Compaction (3.7) calls into here once per chunk; the result feeds
//! straight into a leaf or condensed summary node, with the
//! [`EscalationLevel`] recorded on the row's `model` column so we can
//! see the level distribution in production.
#![allow(dead_code)]

use std::fmt::Write as _;

use tracing::{debug, warn};

use super::super::{SummarizeFn, format_messages_for_summary};
use crate::types::Message;

/// Level-1 (normal) instruction block. Asks for prose that retains
/// specifics: decisions, file paths, commands, tool results.
///
/// Sent in the user turn by `make_summarize_fn`. The role-setting
/// system prompt is fixed there. Includes the `Expand for details
/// about: ...` trailer so future read-back has an explicit hook into
/// `lcm_grep` / `lcm_expand`.
pub const LEVEL_1_PROMPT: &str = "\
You are summarizing a SEGMENT of an agent conversation for future \
model turns. Treat this as incremental memory compaction input, not a \
full-conversation summary.

Normal summary policy:
- Preserve key decisions, rationale, constraints, and active tasks.
- Keep essential technical details needed to continue work safely.
- Preserve specifics: file paths, commands, tool names and their \
outcomes, open questions.
- Preserve any <file id=\"...\"> reference tags exactly as they \
appear.
- Include timestamps for key decisions if they appear in the input.
- Remove obvious repetition, conversational filler, and verbose tool \
output (the originals remain on disk).

Output requirements:
- Plain text only. No preamble, headings, or markdown formatting.
- Track file operations (created, modified, deleted, renamed) with \
file paths and current status.
- If no file operations appear, include exactly: \"Files: none\".
- End with exactly: \"Expand for details about: <comma-separated list \
of what was dropped or compressed>\".";

/// Level-2 (aggressive) instruction block. Asks for terse bullets
/// focused on decisions and outcomes only.
pub const LEVEL_2_PROMPT: &str = "\
You are aggressively summarizing a SEGMENT of an agent conversation \
for future model turns. The level-1 summary was rejected for being \
too long; produce something tighter.

Aggressive summary policy:
- Keep only durable facts and current task state.
- One bullet per key decision or outcome where useful.
- Drop everything that is not load-bearing.
- Preserve explicit TODOs, blockers, decisions, and constraints.
- Preserve any <file id=\"...\"> reference tags exactly as they \
appear.
- Aim for the smallest summary that still lets a future reader \
reconstruct what happened. Originals remain available via lcm_grep \
and lcm_expand if details are needed.

Output requirements:
- Plain text only. No preamble, headings, or markdown formatting.
- Track file operations (created, modified, deleted, renamed) with \
file paths and current status.
- If no file operations appear, include exactly: \"Files: none\".
- End with exactly: \"Expand for details about: <comma-separated list \
of what was dropped or compressed>\".";

/// Hard cap for level-3 deterministic truncation, in estimated tokens.
/// 512 tokens at the `chars / 4` heuristic is roughly 2048 characters.
pub const LEVEL_3_TOKEN_BUDGET: usize = 512;

/// Which level produced a summary. Recorded on the summary row so
/// we can see the level distribution in production.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationLevel {
    /// Normal prose summary.
    Normal,
    /// Aggressive bullet-point summary.
    Aggressive,
    /// Deterministic truncation; no LLM call.
    Deterministic,
}

impl EscalationLevel {
    /// Tag used in the `summaries.model` column.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Normal => "level1",
            Self::Aggressive => "level2",
            Self::Deterministic => "level3-truncate",
        }
    }
}

/// Result of a successful escalation. Always succeeds — level 3 is
/// the floor.
#[derive(Debug, Clone)]
pub struct EscalationOutcome {
    pub content: String,
    pub level: EscalationLevel,
    pub input_tokens: usize,
    pub output_tokens: usize,
}

/// Estimate token count via the `chars / 4` heuristic used everywhere
/// else in the engine.
pub fn estimate_tokens(s: &str) -> usize {
    s.len() / 4
}

/// Estimate token count for a slice of messages.
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| match m {
            Message::User { content }
            | Message::Assistant { content }
            | Message::System { content }
            | Message::Tool { content, .. } => estimate_tokens(content),
            Message::ToolCalls { content, calls } => {
                estimate_tokens(content)
                    + calls
                        .iter()
                        .map(|c| {
                            estimate_tokens(&c.function.name)
                                + estimate_tokens(&c.function.arguments)
                        })
                        .sum::<usize>()
            }
        })
        .sum()
}

/// Run a chunk of messages through the escalation ladder.
///
/// Tries level 1, then level 2, then falls back to deterministic
/// truncation. Each LLM level is considered to have failed if it
/// returns an error or if its output is at least as long as the input
/// (in estimated tokens). Level 3 always succeeds.
///
/// This function performs no database I/O. It is the single hand-off
/// point between compaction (which knows about chunks and the DAG) and
/// the model (which knows about prose).
pub async fn summarize_with_escalation(
    messages: &[Message],
    summarize: &SummarizeFn,
) -> EscalationOutcome {
    let input_tokens = estimate_messages_tokens(messages);

    // Level 1: prose with specifics.
    match summarize(LEVEL_1_PROMPT, messages).await {
        Ok(content) => {
            let output_tokens = estimate_tokens(&content);
            if output_tokens < input_tokens {
                debug!(input_tokens, output_tokens, "level 1 summary accepted");
                return EscalationOutcome {
                    content,
                    level: EscalationLevel::Normal,
                    input_tokens,
                    output_tokens,
                };
            }
            warn!(
                input_tokens,
                output_tokens, "level 1 summary not smaller than input; escalating"
            );
        }
        Err(e) => warn!(error = %e, "level 1 summarization failed; escalating"),
    }

    // Level 2: terse bullets.
    match summarize(LEVEL_2_PROMPT, messages).await {
        Ok(content) => {
            let output_tokens = estimate_tokens(&content);
            if output_tokens < input_tokens {
                debug!(input_tokens, output_tokens, "level 2 summary accepted");
                return EscalationOutcome {
                    content,
                    level: EscalationLevel::Aggressive,
                    input_tokens,
                    output_tokens,
                };
            }
            warn!(
                input_tokens,
                output_tokens, "level 2 summary not smaller than input; escalating"
            );
        }
        Err(e) => warn!(error = %e, "level 2 summarization failed; escalating"),
    }

    // Level 3: deterministic truncation. Cannot fail.
    let content = truncate_messages(messages, LEVEL_3_TOKEN_BUDGET, input_tokens);
    let output_tokens = estimate_tokens(&content);
    debug!(input_tokens, output_tokens, "level 3 truncation applied");
    EscalationOutcome {
        content,
        level: EscalationLevel::Deterministic,
        input_tokens,
        output_tokens,
    }
}

/// Format the messages and truncate to `max_tokens`, appending a note
/// indicating how much was dropped. Uses
/// [`super::super::format_messages_for_summary`] so the on-disk shape
/// matches what the LLM would have seen.
fn truncate_messages(messages: &[Message], max_tokens: usize, input_tokens: usize) -> String {
    let formatted = format_messages_for_summary(messages);
    let max_chars = max_tokens * 4;
    if formatted.len() <= max_chars {
        return formatted;
    }
    // Cut at a UTF-8 boundary. `floor_char_boundary` is unstable, so
    // walk back manually.
    let mut end = max_chars;
    while end > 0 && !formatted.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 64);
    out.push_str(&formatted[..end]);
    let _ = write!(
        out,
        "\n\n[Truncated from {input_tokens} tokens; {max_tokens} tokens preserved.]"
    );
    out
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::error::ProviderError;

    fn user(content: &str) -> Message {
        Message::User {
            content: content.to_string(),
        }
    }

    /// Build a `SummarizeFn` whose answers depend on the prompt. Each
    /// call records the prompt it received, so tests can assert which
    /// levels ran.
    fn programmable_summarize(
        responses: Vec<Result<String, ProviderError>>,
    ) -> (SummarizeFn, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let log_inner = log.clone();
        let responses = Arc::new(Mutex::new(responses.into_iter()));
        let f: SummarizeFn = Arc::new(move |prompt: &str, _messages: &[Message]| {
            log_inner.lock().unwrap().push(prompt.to_string());
            let next = responses.lock().unwrap().next();
            Box::pin(async move { next.unwrap_or(Err(ProviderError::RateLimited)) })
                as Pin<Box<dyn std::future::Future<Output = _> + Send>>
        });
        (f, log)
    }

    #[test]
    fn token_estimate_uses_chars_over_four() {
        assert_eq!(estimate_tokens("a".repeat(40).as_str()), 10);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn token_estimate_messages_covers_all_variants() {
        let messages = vec![
            Message::User {
                content: "a".repeat(40),
            },
            Message::Assistant {
                content: "b".repeat(40),
            },
            Message::Tool {
                call_id: "c1".to_string(),
                content: "c".repeat(40),
            },
        ];
        assert_eq!(estimate_messages_tokens(&messages), 30);
    }

    #[tokio::test]
    async fn level_1_succeeds_when_output_is_smaller() {
        let big_input = "x".repeat(4000); // ~1000 tokens
        let (summarize, log) = programmable_summarize(vec![Ok("tiny summary".to_string())]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &summarize).await;
        assert_eq!(outcome.level, EscalationLevel::Normal);
        assert_eq!(outcome.content, "tiny summary");
        assert!(outcome.output_tokens < outcome.input_tokens);
        let prompts = log.lock().unwrap().clone();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0], LEVEL_1_PROMPT);
    }

    #[tokio::test]
    async fn level_1_too_large_falls_through_to_level_2() {
        let big_input = "x".repeat(4000);
        let bloated = "y".repeat(8000); // larger than input
        let (summarize, log) =
            programmable_summarize(vec![Ok(bloated), Ok("tight bullets".to_string())]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &summarize).await;
        assert_eq!(outcome.level, EscalationLevel::Aggressive);
        assert_eq!(outcome.content, "tight bullets");
        let prompts = log.lock().unwrap().clone();
        assert_eq!(prompts, vec![LEVEL_1_PROMPT, LEVEL_2_PROMPT]);
    }

    #[tokio::test]
    async fn level_1_error_falls_through_to_level_2() {
        let big_input = "x".repeat(4000);
        let (summarize, log) = programmable_summarize(vec![
            Err(ProviderError::RateLimited),
            Ok("recovered".to_string()),
        ]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &summarize).await;
        assert_eq!(outcome.level, EscalationLevel::Aggressive);
        assert_eq!(outcome.content, "recovered");
        let prompts = log.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2);
    }

    #[tokio::test]
    async fn both_levels_fail_falls_to_truncation() {
        let big_input = "x".repeat(4000);
        let bloated_a = "a".repeat(8000);
        let bloated_b = "b".repeat(8000);
        let (summarize, _log) = programmable_summarize(vec![Ok(bloated_a), Ok(bloated_b)]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &summarize).await;
        assert_eq!(outcome.level, EscalationLevel::Deterministic);
        assert!(outcome.content.contains("[Truncated from"));
        assert!(outcome.output_tokens <= LEVEL_3_TOKEN_BUDGET + 32);
    }

    #[tokio::test]
    async fn both_levels_error_falls_to_truncation() {
        let (summarize, _log) = programmable_summarize(vec![
            Err(ProviderError::RateLimited),
            Err(ProviderError::RateLimited),
        ]);
        let outcome = summarize_with_escalation(&[user(&"x".repeat(4000))], &summarize).await;
        assert_eq!(outcome.level, EscalationLevel::Deterministic);
        assert!(outcome.content.contains("[Truncated from"));
    }

    #[test]
    fn truncate_short_input_passes_through() {
        let messages = vec![user("hello")];
        let formatted = truncate_messages(&messages, LEVEL_3_TOKEN_BUDGET, 1);
        assert!(!formatted.contains("[Truncated"));
        assert!(formatted.contains("hello"));
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // A long string of multi-byte chars; truncation must not split
        // a codepoint in half.
        let s = "€".repeat(2000);
        let messages = vec![user(&s)];
        let out = truncate_messages(&messages, 32, 1500);
        assert!(out.is_char_boundary(out.len()));
        assert!(out.contains("[Truncated from 1500 tokens"));
    }

    #[test]
    fn level_tag_is_stable() {
        assert_eq!(EscalationLevel::Normal.tag(), "level1");
        assert_eq!(EscalationLevel::Aggressive.tag(), "level2");
        assert_eq!(EscalationLevel::Deterministic.tag(), "level3-truncate");
    }
}
