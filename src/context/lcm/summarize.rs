//! Three-level summarization escalation.
//!
//! When LCM compacts a chunk of context, it tries up to three
//! strategies in order. A strategy "fails" when the LLM call errors,
//! when its output is no smaller than its input (no point keeping a
//! summary that costs as many tokens as the originals), or when its
//! output falls under the degenerate floor (a few-line summary of a
//! chunk worth compacting is a model failure, not compression).
//! Escalation falls through to the next level, and the deterministic
//! level is guaranteed to converge.
//!
//! | Level | Strategy                                          | LLM? |
//! |-------|---------------------------------------------------|------|
//! | 1     | Prose summary preserving specifics                | yes  |
//! | 2     | Terse bullets, decisions and outcomes only        | yes  |
//! | 3     | Truncate raw text to a fixed token budget         | no   |
//!
//! See spec 14 §"Three-Level Summarization Escalation". This module is
//! intentionally a pure function over a per-level call closure — it
//! has no database plumbing, so unit tests run against canned mocks.
//!
//! Compaction (3.7) calls into here once per chunk; the result feeds
//! straight into a leaf or condensed summary node, with the
//! [`EscalationLevel`] recorded on the row's `model` column so we can
//! see the level distribution in production.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::future::Future;

use tracing::{debug, warn};

use super::super::format_messages_for_summary;
use crate::error::ProviderError;
use crate::types::{Message, estimate_tokens};

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
- Include a \"Files:\" line tracking file operations (created, \
modified, deleted, renamed). Each entry: the path plus a short clause \
on why it matters, e.g. \"src/exec.rs (modified: added retry wrapper)\".
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
- Include a \"Files:\" line tracking file operations (created, \
modified, deleted, renamed). Each entry: the path plus a short clause \
on why it matters, e.g. \"src/exec.rs (modified: added retry wrapper)\".
- If no file operations appear, include exactly: \"Files: none\".
- End with exactly: \"Expand for details about: <comma-separated list \
of what was dropped or compressed>\".";

/// Hard cap for level-3 deterministic truncation, in estimated tokens.
/// 512 tokens at the `chars / 4` heuristic is roughly 2048 characters.
pub const LEVEL_3_TOKEN_BUDGET: usize = 512;

/// LLM output under this many characters is rejected as degenerate.
pub const DEGENERATE_FLOOR_CHARS: usize = 500;

/// The degenerate floor only applies to inputs of at least this many
/// estimated characters. A small residual chunk can honestly summarize
/// to under the floor; rejecting that wastes two LLM calls to end at
/// level-3 passthrough.
pub const DEGENERATE_FLOOR_MIN_INPUT_CHARS: usize = 4 * DEGENERATE_FLOOR_CHARS;

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

/// Estimate token count for a slice of messages.
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages.iter().map(Message::token_estimate).sum()
}

/// Run a chunk of messages through the escalation ladder.
///
/// Tries level 1, then level 2, then falls back to deterministic
/// truncation. Each LLM level is considered to have failed if it
/// returns an error or if its output is at least as long as the input
/// (in estimated tokens). Level 3 always succeeds.
///
/// `call` performs one LLM call for the given level instruction block;
/// the caller decides how the chunk reaches the model (fresh prompt or
/// the main session's cache prefix). `messages` stays here for the
/// acceptance thresholds and level-3 truncation.
///
/// This function performs no database I/O. It is the single hand-off
/// point between compaction (which knows about chunks and the DAG) and
/// the model (which knows about prose).
pub async fn summarize_with_escalation<F, Fut>(messages: &[Message], call: F) -> EscalationOutcome
where
    F: Fn(&'static str) -> Fut,
    Fut: Future<Output = Result<String, ProviderError>> + Send,
{
    let input_tokens = estimate_messages_tokens(messages);

    // Level 1: prose with specifics.
    match call(LEVEL_1_PROMPT).await {
        Ok(content) => {
            if let Some(output_tokens) = accept(&content, input_tokens) {
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
                output_chars = content.len(),
                "level 1 summary too long or degenerate; escalating"
            );
        }
        Err(e) => warn!(error = %e, "level 1 summarization failed; escalating"),
    }

    // Level 2: terse bullets.
    match call(LEVEL_2_PROMPT).await {
        Ok(content) => {
            if let Some(output_tokens) = accept(&content, input_tokens) {
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
                output_chars = content.len(),
                "level 2 summary too long or degenerate; escalating"
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

/// Accept an LLM summary only when it actually compresses: smaller
/// than the input, and above the degenerate floor when the input is
/// big enough for the floor to be meaningful. Returns the output token
/// estimate on acceptance.
fn accept(content: &str, input_tokens: usize) -> Option<usize> {
    let output_tokens = estimate_tokens(content);
    if output_tokens >= input_tokens {
        return None;
    }
    let input_chars = input_tokens * 4;
    if content.len() < DEGENERATE_FLOOR_CHARS && input_chars >= DEGENERATE_FLOOR_MIN_INPUT_CHARS {
        return None;
    }
    Some(output_tokens)
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

    type BoxedCall = Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send>>;

    /// Build a call closure whose answers arrive in sequence. Each
    /// call records the prompt it received, so tests can assert which
    /// levels ran.
    fn programmable_call(
        responses: Vec<Result<String, ProviderError>>,
    ) -> (impl Fn(&'static str) -> BoxedCall, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let log_inner = log.clone();
        let responses = Arc::new(Mutex::new(responses.into_iter()));
        let f = move |prompt: &'static str| {
            log_inner.lock().unwrap().push(prompt.to_string());
            let next = responses.lock().unwrap().next();
            Box::pin(async move { next.unwrap_or(Err(ProviderError::RateLimited)) }) as BoxedCall
        };
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

    /// A summary that clears the degenerate floor.
    fn plausible_summary(seed: &str) -> String {
        seed.repeat(DEGENERATE_FLOOR_CHARS / seed.len() + 1)
    }

    #[tokio::test]
    async fn level_1_succeeds_when_output_is_smaller() {
        let big_input = "x".repeat(4000); // ~1000 tokens
        let summary = plausible_summary("real summary ");
        let (call, log) = programmable_call(vec![Ok(summary.clone())]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &call).await;
        assert_eq!(outcome.level, EscalationLevel::Normal);
        assert_eq!(outcome.content, summary);
        assert!(outcome.output_tokens < outcome.input_tokens);
        let prompts = log.lock().unwrap().clone();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0], LEVEL_1_PROMPT);
    }

    #[tokio::test]
    async fn level_1_too_large_falls_through_to_level_2() {
        let big_input = "x".repeat(4000);
        let bloated = "y".repeat(8000); // larger than input
        let bullets = plausible_summary("tight bullets ");
        let (call, log) = programmable_call(vec![Ok(bloated), Ok(bullets.clone())]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &call).await;
        assert_eq!(outcome.level, EscalationLevel::Aggressive);
        assert_eq!(outcome.content, bullets);
        let prompts = log.lock().unwrap().clone();
        assert_eq!(prompts, vec![LEVEL_1_PROMPT, LEVEL_2_PROMPT]);
    }

    #[tokio::test]
    async fn level_1_error_falls_through_to_level_2() {
        let big_input = "x".repeat(4000);
        let recovered = plausible_summary("recovered ");
        let (call, log) =
            programmable_call(vec![Err(ProviderError::RateLimited), Ok(recovered.clone())]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &call).await;
        assert_eq!(outcome.level, EscalationLevel::Aggressive);
        assert_eq!(outcome.content, recovered);
        let prompts = log.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2);
    }

    #[tokio::test]
    async fn degenerate_level_1_escalates() {
        let big_input = "x".repeat(4000);
        let bullets = plausible_summary("bullets ");
        let (call, log) = programmable_call(vec![Ok("stub".to_string()), Ok(bullets.clone())]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &call).await;
        assert_eq!(outcome.level, EscalationLevel::Aggressive);
        assert_eq!(outcome.content, bullets);
        let prompts = log.lock().unwrap().clone();
        assert_eq!(prompts, vec![LEVEL_1_PROMPT, LEVEL_2_PROMPT]);
    }

    #[tokio::test]
    async fn degenerate_both_levels_falls_to_truncation() {
        let big_input = "x".repeat(4000);
        let (call, _log) = programmable_call(vec![Ok("a".to_string()), Ok("b".to_string())]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &call).await;
        assert_eq!(outcome.level, EscalationLevel::Deterministic);
        assert!(outcome.content.contains("[Truncated from"));
    }

    #[tokio::test]
    async fn small_chunk_short_summary_is_accepted() {
        // Input under DEGENERATE_FLOOR_MIN_INPUT_CHARS: the floor is
        // inactive and a short-but-honest summary passes at level 1.
        let small_input = "x".repeat(1200);
        let (call, _log) = programmable_call(vec![Ok("short but honest".to_string())]);
        let outcome = summarize_with_escalation(&[user(&small_input)], &call).await;
        assert_eq!(outcome.level, EscalationLevel::Normal);
        assert_eq!(outcome.content, "short but honest");
    }

    #[tokio::test]
    async fn both_levels_fail_falls_to_truncation() {
        let big_input = "x".repeat(4000);
        let bloated_a = "a".repeat(8000);
        let bloated_b = "b".repeat(8000);
        let (call, _log) = programmable_call(vec![Ok(bloated_a), Ok(bloated_b)]);
        let outcome = summarize_with_escalation(&[user(&big_input)], &call).await;
        assert_eq!(outcome.level, EscalationLevel::Deterministic);
        assert!(outcome.content.contains("[Truncated from"));
        assert!(outcome.output_tokens <= LEVEL_3_TOKEN_BUDGET + 32);
    }

    #[tokio::test]
    async fn both_levels_error_falls_to_truncation() {
        let (call, _log) = programmable_call(vec![
            Err(ProviderError::RateLimited),
            Err(ProviderError::RateLimited),
        ]);
        let outcome = summarize_with_escalation(&[user(&"x".repeat(4000))], &call).await;
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
