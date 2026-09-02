//! Type-aware exploration summaries for externalized large payloads.
//!
//! Pure core of large file handling. Given payload content and an
//! optional path hint, produce a compact structural description
//! for embedding in a `<file>` reference. Only the plain-text
//! dispatcher calls the LLM (via [`SummarizeFn`]), and it falls back
//! to a deterministic summary on failure. Every other dispatcher is
//! deterministic. Mirrors `large-files.ts` in the reference
//! implementation.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::context::SummarizeFn;
use crate::types::Message;

/// Bytes per head/mid/tail slice fed to the LLM for text payloads.
const TEXT_SLICE_BYTES: usize = 2_400;
/// Max detected section headers reported.
const TEXT_HEADER_LIMIT: usize = 18;
/// Bytes of hex dump for binary-looking payloads.
const BINARY_DUMP_BYTES: usize = 256;
/// Bytes sniffed for control characters when detecting binary content.
const BINARY_SNIFF_BYTES: usize = 4_096;

const CODE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cs", "go", "h", "hpp", "java", "js", "jsx", "kt", "m", "php", "py", "rb",
    "rs", "scala", "sh", "swift", "ts", "tsx",
];

/// Payload category driving dispatcher selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Json,
    Csv,
    Tsv,
    Yaml,
    Xml,
    Sql,
    Code,
    Text,
    Binary,
}

/// Best-effort MIME type for a detected payload kind, for the
/// `large_files.mime_type` column.
pub fn mime_hint(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Json => "application/json",
        FileKind::Csv => "text/csv",
        FileKind::Tsv => "text/tab-separated-values",
        FileKind::Yaml => "application/yaml",
        FileKind::Xml => "application/xml",
        FileKind::Sql => "application/sql",
        FileKind::Code => "text/x-source-code",
        FileKind::Text => "text/plain",
        FileKind::Binary => "application/octet-stream",
    }
}

/// Derive a stable file id from payload content: `file_` plus the
/// first 16 hex chars of SHA-256. Content-addressed, so identical
/// payloads dedupe to the same id.
pub fn file_id(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let prefix = digest
        .iter()
        .take(8)
        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    format!("file_{prefix:016x}")
}

static FILE_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"\bfile_[a-fA-F0-9]{16}\b"));

/// Extract every `file_xxx` id referenced in `content`, deduplicated
/// in order of first appearance and normalized to lowercase.
pub fn extract_file_ids(content: &str) -> Vec<String> {
    unique_ordered(
        FILE_ID_RE
            .find_iter(content)
            .map(|m| m.as_str().to_ascii_lowercase()),
    )
}

/// Render the `<file>` reference stored in `messages.content` in
/// place of the raw payload. `next_step` is the content-aware
/// dereference instruction; empty renders the bare reference.
pub fn format_file_reference(
    file_id: &str,
    path: &str,
    token_count: usize,
    summary: &str,
    next_step: &str,
) -> String {
    let body = if next_step.is_empty() {
        summary.trim().to_string()
    } else {
        format!("{}\n{next_step}", summary.trim())
    };
    format!("<file id=\"{file_id}\" path=\"{path}\" tokens=\"{token_count}\">\n{body}\n</file>")
}

/// Test-runner output shape, any ecosystem: the payloads a model most
/// often answers by re-running the command instead of searching.
pub fn looks_like_test_run(content: &str) -> bool {
    content.contains("test result:")
        || content.contains("panicked at")
        || content.contains("=== RUN")
        || (content.contains("passed") && content.contains("failed"))
}

/// The dereference instruction for a `<file>` stub. Models do not
/// follow references through the sanctioned tools unprompted, so each
/// stub names its own next step. The target is the on-disk copy at
/// `path`: the raw bytes never reach the FTS tables (`intercept_large`
/// replaces them before persistence), so `lcm_grep` cannot see them.
pub fn next_step(kind: FileKind, test_run: bool, has_source_path: bool, path: &str) -> String {
    if test_run {
        return format!(
            "Next: grep pattern='panicked at|FAILED|error' path={path} \
             pulls the failures from the stored copy; do not re-run the \
             command."
        );
    }
    if has_source_path {
        return format!("Next: file_read {path} with offset/limit windows the original file.");
    }
    match kind {
        FileKind::Json | FileKind::Xml | FileKind::Yaml => format!(
            "Next: the stored copy at {path} is line-oriented; grep it or \
             file_read a range of it."
        ),
        _ => format!(
            "Next: grep pattern=<regex> path={path} searches the stored \
             text; file_read {path} windows it."
        ),
    }
}

/// Reformat a payload for on-disk storage so line-based tools work:
/// a valid JSON payload is pretty-printed (grep, `file_read` ranges,
/// and `lcm_grep` regex mode all operate per line, and a minified
/// blob makes every one of them return the whole file — issue #119).
/// Wrapped or line-numbered tool framing is pretty-printed underneath
/// and re-wrapped; anything unparseable is returned unchanged.
pub fn normalize_payload(content: &str) -> std::borrow::Cow<'_, str> {
    let unwrapped = strip_tool_framing(content);
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&unwrapped) else {
        return std::borrow::Cow::Borrowed(content);
    };
    // Round-trip fidelity: object keys are re-sorted (Map is a
    // BTreeMap by default) and >64-bit integers approximate to f64;
    // harmless for API payloads.
    let Ok(pretty) = serde_json::to_string_pretty(&parsed) else {
        // Unreachable: serializing a parsed Value cannot fail. Return
        // the original rather than wrap garbage in fresh framing.
        return std::borrow::Cow::Borrowed(content);
    };
    // Re-emit the framing only for line-numbered file_read results,
    // keeping file_read parity; any other framing is dropped so the
    // stored body stays a plain pretty JSON.
    let Some(open) = framed_numbering(content) else {
        return std::borrow::Cow::Owned(pretty);
    };
    let mut rebuilt = String::with_capacity(pretty.len() + (content.len() - unwrapped.len()));
    rebuilt.push_str(open);
    rebuilt.push('\n');
    for (i, line) in pretty.lines().enumerate() {
        let _ = writeln!(rebuilt, "{}\t{line}", i + 1);
    }
    // Fresh stats over the stored body: strip_line_numbering requires
    // the trailer, so omitting it would leave invalid file_read
    // framing.
    let lines = pretty.lines().count();
    let _ = writeln!(
        rebuilt,
        "({lines} lines shown, {lines} total, {} bytes)",
        pretty.len()
    );
    rebuilt.push_str("</tool_output>");
    std::borrow::Cow::Owned(rebuilt)
}

/// The framing of a line-numbered `file_read` result: the
/// `<tool_output>` open line, re-emitted around the pretty body so
/// the stored copy keeps `file_read` parity. Unnumbered framing is
/// not rebuilt — the body gets no line numbers of its own, so
/// re-wrapping would wrap a bare pretty JSON and inject nothing.
fn framed_numbering(content: &str) -> Option<&str> {
    let (first, rest) = content.split_once('\n')?;
    if !TOOL_OUTPUT_OPEN_RE.is_match(first) {
        return None;
    }
    let inner = rest.trim_end().strip_suffix("</tool_output>")?;
    let numbered = inner.trim_end().lines().next().is_some_and(|l| {
        l.split_once('\t')
            .is_some_and(|(n, _)| n.parse::<u64>().is_ok())
    });
    numbered.then_some(first)
}

/// Lines kept at each end of a [`mechanical_excerpt`].
const EXCERPT_LINES_PER_SIDE: usize = 30;
/// Byte cap per excerpt side, so pathological few-line output (a
/// minified JSON blob, say) still yields a small excerpt.
const EXCERPT_BYTES_PER_SIDE: usize = 2_000;

/// Free head+tail excerpt for externalized tool output.
///
/// Tool results arrive on every turn, so unlike user payloads they
/// get no LLM exploration pass: the excerpt is the first and last
/// ~30 lines (byte-capped per side) with an omission marker in the
/// middle. Tail included deliberately: build and test logs put the
/// failure at the end.
pub fn mechanical_excerpt(content: &str) -> String {
    let head_end: usize = content
        .split_inclusive('\n')
        .take(EXCERPT_LINES_PER_SIDE)
        .map(str::len)
        .sum();
    let head = crate::text::prefix(
        crate::text::prefix(content, head_end),
        EXCERPT_BYTES_PER_SIDE,
    );
    let tail_len: usize = content
        .split_inclusive('\n')
        .rev()
        .take(EXCERPT_LINES_PER_SIDE)
        .map(str::len)
        .sum();
    let tail = crate::text::suffix(
        crate::text::suffix(content, tail_len),
        EXCERPT_BYTES_PER_SIDE,
    );

    // Head and tail meeting or overlapping means nothing would be
    // omitted; keep the content whole.
    if head.len() + tail.len() >= content.len() {
        return content.to_string();
    }
    let omitted = content
        .get(head.len()..content.len() - tail.len())
        .unwrap_or("");
    format!(
        "Tool output excerpt ({} lines, {} bytes total):\n{}\n... [{} lines / {} bytes omitted] ...\n{}",
        content.lines().count(),
        content.len(),
        head.trim_end_matches('\n'),
        omitted.lines().count(),
        omitted.len(),
        tail,
    )
}

static FILE_READ_TRAILER_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"^\(\d+ lines shown, \d+ total, \d+ bytes\)$"));

static TOOL_OUTPUT_OPEN_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r#"^<tool_output name="[^"]+">$"#));

/// Undo tool result framing so dispatchers see the underlying file
/// content. Tool results reach the engine wrapped in
/// `<tool_output name="...">...</tool_output>` (see
/// `safety::check_tool_output`), and `file_read` additionally
/// prefixes every line with `N\t` and appends a
/// `(N lines shown, M total, B bytes)` trailer. Without the strip,
/// every structured payload read through `file_read` fails to parse
/// (a line-numbered JSON file starts with `1\t{`, not `{`).
///
/// Each layer is only removed on an exact match: the wrapper needs
/// both its open and close tags, and the line numbering needs the
/// stats trailer plus a sequential number on every line, so data that
/// merely has a numeric first column passes through untouched.
pub fn strip_tool_framing(content: &str) -> std::borrow::Cow<'_, str> {
    let unwrapped = strip_tool_output_wrapper(content);
    match strip_line_numbering(unwrapped) {
        Some(stripped) => std::borrow::Cow::Owned(stripped),
        None => std::borrow::Cow::Borrowed(unwrapped),
    }
}

/// Peel a `<tool_output name="...">` / `</tool_output>` envelope,
/// returning the content unchanged when it isn't wrapped.
fn strip_tool_output_wrapper(content: &str) -> &str {
    let Some((first, rest)) = content.split_once('\n') else {
        return content;
    };
    if !TOOL_OUTPUT_OPEN_RE.is_match(first) {
        return content;
    }
    let Some(inner) = rest.trim_end().strip_suffix("</tool_output>") else {
        return content;
    };
    inner.strip_suffix('\n').unwrap_or(inner)
}

/// Undo `file_read` line numbering, or `None` when `content` doesn't
/// match the framing exactly.
fn strip_line_numbering(content: &str) -> Option<String> {
    let mut lines: Vec<&str> = content.lines().collect();
    if !lines
        .last()
        .is_some_and(|l| FILE_READ_TRAILER_RE.is_match(l))
    {
        return None;
    }
    lines.pop();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    let mut expected: Option<u64> = None;
    let mut stripped: Vec<&str> = Vec::with_capacity(lines.len());
    for line in lines {
        let (num, rest) = line.split_once('\t')?;
        let n = num.parse::<u64>().ok()?;
        if expected.is_some_and(|e| n != e) {
            return None;
        }
        expected = Some(n + 1);
        stripped.push(rest);
    }
    if stripped.is_empty() {
        return None;
    }
    Some(stripped.join("\n"))
}

/// Classify a payload from its path extension, falling back to
/// content sniffing when no path is known.
pub fn detect_kind(path: Option<&str>, content: &str) -> FileKind {
    if looks_binary(content) {
        return FileKind::Binary;
    }
    if let Some(ext) = extension(path) {
        return match ext.as_str() {
            "json" => FileKind::Json,
            "csv" => FileKind::Csv,
            "tsv" => FileKind::Tsv,
            "yaml" | "yml" => FileKind::Yaml,
            "xml" => FileKind::Xml,
            "sql" => FileKind::Sql,
            e if CODE_EXTENSIONS.contains(&e) => FileKind::Code,
            _ => FileKind::Text,
        };
    }
    match content.trim_start().chars().next() {
        Some('{' | '[') => FileKind::Json,
        Some('<') => FileKind::Xml,
        _ => FileKind::Text,
    }
}

/// Produce the exploration summary for a payload. Async because the
/// plain-text dispatcher may call the LLM; all other paths return
/// without awaiting anything.
pub async fn exploration_summary(
    content: &str,
    path: Option<&str>,
    summarize: &SummarizeFn,
    summary_token_bound: u32,
) -> String {
    match detect_kind(path, content) {
        FileKind::Json => match explore_json(content) {
            Some(summary) => summary,
            // Not valid JSON despite the .json path, which is normal
            // for a partial read: the head of a JSON file rarely
            // parses on its own. Text exploration still summarizes
            // whatever structure is there.
            None => explore_text(content, path, summarize, summary_token_bound).await,
        },
        FileKind::Csv => explore_delimited(content, ',', "CSV"),
        FileKind::Tsv => explore_delimited(content, '\t', "TSV"),
        FileKind::Yaml => explore_yaml(content),
        FileKind::Xml => explore_xml(content),
        FileKind::Sql => explore_sql(content),
        FileKind::Code => explore_code(content, path),
        FileKind::Binary => explore_binary(content),
        FileKind::Text => explore_text(content, path, summarize, summary_token_bound).await,
    }
}

// ── detection helpers ─────────────────────────────────────────────

fn extension(path: Option<&str>) -> Option<String> {
    let base = path?.rsplit(['/', '\\']).next()?;
    let (stem, ext) = base.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 10 {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    ext.chars()
        .all(|c| c.is_ascii_alphanumeric())
        .then_some(ext)
}

/// Control characters other than whitespace and ANSI escapes mark a
/// payload as binary. Message content is already valid UTF-8, so this
/// catches embedded NULs and similar, not arbitrary byte soup.
fn looks_binary(content: &str) -> bool {
    crate::text::prefix(content, BINARY_SNIFF_BYTES)
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{1b}'))
}

/// Collapse whitespace and truncate to `max_bytes`, appending `...`
/// when cut.
fn normalize_line(text: &str, max_bytes: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= max_bytes {
        return compact;
    }
    format!("{}...", crate::text::prefix(&compact, max_bytes))
}

fn unique_ordered(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values
        .into_iter()
        .filter(|v| seen.insert(v.clone()))
        .collect()
}

// ── deterministic dispatchers ─────────────────────────────────────

/// `None` when the payload is not valid JSON; the caller falls back
/// to text exploration.
fn explore_json(content: &str) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let top_level = match &parsed {
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
    };
    Some(format!(
        "Structured summary (JSON):\nTop-level type: {top_level}.\nShape: {}.",
        describe_json(&parsed, 0)
    ))
}

fn describe_json(value: &serde_json::Value, depth: u8) -> String {
    if depth >= 2 {
        return "...".to_string();
    }
    match value {
        serde_json::Value::Array(items) => {
            let sample: Vec<String> = items
                .iter()
                .take(3)
                .map(|v| describe_json(v, depth + 1))
                .collect();
            if sample.is_empty() {
                "array(len=0)".to_string()
            } else {
                format!("array(len={}, sample=[{}])", items.len(), sample.join(", "))
            }
        }
        serde_json::Value::Object(map) => {
            let preview: Vec<String> = map
                .iter()
                .take(10)
                .map(|(k, v)| format!("{k}: {}", describe_json(v, depth + 1)))
                .collect();
            if preview.is_empty() {
                "object(keys=0)".to_string()
            } else {
                format!("object(keys={}: {})", map.len(), preview.join(", "))
            }
        }
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(_) => "bool".to_string(),
        serde_json::Value::Number(_) => "number".to_string(),
        serde_json::Value::String(_) => "string".to_string(),
    }
}

fn explore_delimited(content: &str, delimiter: char, kind: &str) -> String {
    let lines: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return format!("Structured summary ({kind}): no rows found.");
    }
    let headers: Vec<&str> = lines[0]
        .split(delimiter)
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();
    let row_count = lines.len() - 1;
    let first_data = lines
        .get(1)
        .map_or_else(|| "(no data rows)".to_string(), |l| normalize_line(l, 180));
    format!(
        "Structured summary ({kind}):\nRows: {row_count}.\nColumns ({}): {}.\nFirst row sample: {first_data}.",
        headers.len(),
        if headers.is_empty() {
            "(none detected)".to_string()
        } else {
            headers.join(", ")
        },
    )
}

static YAML_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"^([A-Za-z0-9_.-]+):"));

fn explore_yaml(content: &str) -> String {
    let keys = unique_ordered(
        content
            .lines()
            .filter_map(|line| YAML_KEY_RE.captures(line).map(|c| c[1].to_string())),
    );
    format!(
        "Structured summary (YAML):\nTop-level keys ({}): {}.",
        keys.len(),
        if keys.is_empty() {
            "(none detected)".to_string()
        } else {
            keys.iter().take(30).cloned().collect::<Vec<_>>().join(", ")
        },
    )
}

static XML_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"<([A-Za-z0-9_:-]+)[\s>]"));

fn explore_xml(content: &str) -> String {
    let mut tags = XML_TAG_RE.captures_iter(content).map(|c| c[1].to_string());
    let Some(root) = tags.next() else {
        return "Structured summary (XML): no elements detected.".to_string();
    };
    let children = unique_ordered(tags.filter(|t| *t != root).take(30));
    format!(
        "Structured summary (XML):\nRoot element: {root}.\nChild elements seen: {}.",
        if children.is_empty() {
            "(none detected)".to_string()
        } else {
            children.join(", ")
        },
    )
}

static SQL_TABLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    crate::text::static_regex(
        r#"(?i)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z0-9_."\[\]]+)"#,
    )
});
static SQL_INSERT_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"(?i)\bINSERT\s+INTO\b"));

fn explore_sql(content: &str) -> String {
    let tables = unique_ordered(
        SQL_TABLE_RE
            .captures_iter(content)
            .map(|c| c[1].to_string()),
    );
    let insert_count = SQL_INSERT_RE.find_iter(content).count();
    format!(
        "Structured summary (SQL):\nTables ({}): {}.\nINSERT statements: {insert_count}.",
        tables.len(),
        if tables.is_empty() {
            "(none detected)".to_string()
        } else {
            tables.join(", ")
        },
    )
}

static IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    crate::text::static_regex(r"^\s*(use\s|import\s|from\s+\S+\s+import\s|#include\s|require\s*\()")
});
static SIGNATURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    crate::text::static_regex(
        r"^(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:async\s+)?(?:unsafe\s+)?(?:fn|def|class|struct|enum|trait|impl|interface|function|func|type)\s",
    )
});

fn explore_code(content: &str, path: Option<&str>) -> String {
    let imports = unique_ordered(
        content
            .lines()
            .filter(|l| IMPORT_RE.is_match(l))
            .map(|l| normalize_line(l, 180))
            .take(12),
    );
    let signatures = unique_ordered(
        content
            .lines()
            .map(str::trim)
            .filter(|l| SIGNATURE_RE.is_match(l))
            .map(|l| normalize_line(l, 200))
            .take(24),
    );
    let name = path.map_or_else(String::new, |p| format!(" ({p})"));
    format!(
        "Code exploration summary{name}:\nLines: {}.\nImports/dependencies ({}): {}.\nTop-level definitions ({}): {}.",
        content.lines().count(),
        imports.len(),
        if imports.is_empty() {
            "none detected".to_string()
        } else {
            imports.join(" | ")
        },
        signatures.len(),
        if signatures.is_empty() {
            "none detected".to_string()
        } else {
            signatures.join(" | ")
        },
    )
}

fn explore_binary(content: &str) -> String {
    let bytes = content.as_bytes();
    let dump: Vec<String> = bytes
        .iter()
        .take(BINARY_DUMP_BYTES)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "Binary payload summary:\nBytes: {}.\nFirst {} bytes (hex): {}",
        bytes.len(),
        dump.len(),
        dump.join(" "),
    )
}

// ── plain-text dispatcher (LLM with deterministic fallback) ───────

static MARKDOWN_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"^#{1,6}\s+"));
static CAPS_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::text::static_regex(r"^[A-Z0-9][A-Z0-9 :_-]{6,}$"));

fn extract_text_headers(content: &str) -> Vec<String> {
    unique_ordered(
        content
            .lines()
            .map(str::trim)
            .filter(|l| l.len() > 1)
            .filter(|l| MARKDOWN_HEADER_RE.is_match(l) || CAPS_HEADER_RE.is_match(l))
            .map(|l| normalize_line(l, 160))
            .take(TEXT_HEADER_LIMIT),
    )
}

/// Head, middle, and tail slices of the payload. Small payloads pass
/// through whole.
fn build_text_sample(content: &str) -> String {
    if content.len() <= TEXT_SLICE_BYTES * 2 {
        return content.to_string();
    }
    let head = crate::text::prefix(content, TEXT_SLICE_BYTES);
    let tail = crate::text::suffix(content, TEXT_SLICE_BYTES);
    let mid_start = (content.len() - TEXT_SLICE_BYTES) / 2;
    let mid = crate::text::prefix(
        crate::text::suffix(content, content.len() - mid_start),
        TEXT_SLICE_BYTES,
    );
    format!("[Document Start]\n\n{head}\n\n[Document Middle]\n\n{mid}\n\n[Document End]\n\n{tail}")
}

fn build_text_instructions(
    content: &str,
    path: Option<&str>,
    headers: &[String],
    summary_token_bound: u32,
) -> String {
    format!(
        "Summarize this large file for retrieval-time context references.\n\
         Path: {}\n\
         Length: {} chars, {} lines.\n\
         Detected section headers: {}\n\
         Produce at most {summary_token_bound} tokens covering:\n\
         - What the document is about\n\
         - Key sections and topics\n\
         - Important names, dates, and numbers\n\
         - Any action items or constraints\n\
         Do not quote long passages verbatim.",
        path.unwrap_or("unknown"),
        content.len(),
        content.lines().count(),
        if headers.is_empty() {
            "none".to_string()
        } else {
            headers.join(" | ")
        },
    )
}

fn explore_text_fallback(content: &str, path: Option<&str>) -> String {
    let headers = extract_text_headers(content);
    let word_count = content.split_whitespace().count();
    let name = path.map_or_else(String::new, |p| format!(" ({p})"));
    format!(
        "Text exploration summary{name}:\nCharacters: {}.\nWords: {word_count}.\nLines: {}.\nDetected section headers: {}.\nOpening excerpt: {}.\nClosing excerpt: {}.",
        content.len(),
        content.lines().count(),
        if headers.is_empty() {
            "none detected".to_string()
        } else {
            headers.join(" | ")
        },
        normalize_line(crate::text::prefix(content, 500), 500),
        normalize_line(crate::text::suffix(content, 500), 500),
    )
}

async fn explore_text(
    content: &str,
    path: Option<&str>,
    summarize: &SummarizeFn,
    summary_token_bound: u32,
) -> String {
    let headers = extract_text_headers(content);
    let instructions = build_text_instructions(content, path, &headers, summary_token_bound);
    let sample = vec![Message::User {
        content: build_text_sample(content),
    }];
    match summarize(&instructions, &sample).await {
        Ok(summary) if !summary.trim().is_empty() => summary.trim().to_string(),
        _ => explore_text_fallback(content, path),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::error::ProviderError;

    fn canned(reply: &str) -> SummarizeFn {
        let reply = reply.to_string();
        Arc::new(move |_instructions, _messages| {
            let reply = reply.clone();
            Box::pin(async move { Ok(reply) })
        })
    }

    fn failing() -> SummarizeFn {
        Arc::new(|_instructions, _messages| Box::pin(async { Err(ProviderError::RateLimited) }))
    }

    // ── file id and reference format ──────────────────────────────

    #[test]
    fn file_id_is_stable_and_well_formed() {
        let a = file_id("hello");
        let b = file_id("hello");
        assert_eq!(a, b);
        assert!(a.starts_with("file_"));
        assert_eq!(a.len(), 5 + 16);
        let hex = a.strip_prefix("file_").unwrap();
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, file_id("world"));
    }

    #[test]
    fn file_reference_includes_attributes() {
        let r = format_file_reference("file_0123456789abcdef", "data/out.json", 42, "sum", "");
        assert_eq!(
            r,
            "<file id=\"file_0123456789abcdef\" path=\"data/out.json\" tokens=\"42\">\nsum\n</file>"
        );
    }

    #[test]
    fn file_reference_carries_the_next_step_line() {
        let r = format_file_reference("file_0123456789abcdef", "src/a.rs", 42, "sum", "Next: x");
        assert!(r.ends_with("\nsum\nNext: x\n</file>"), "{r}");
    }

    // ── next-step guidance ──────────────────────────────────────────

    #[test]
    fn test_run_payload_points_at_the_stored_copy_not_rerun() {
        let step = next_step(FileKind::Text, true, false, "context/lcm/payloads/file_ab");
        assert!(step.contains("grep pattern="), "{step}");
        assert!(step.contains("path=context/lcm/payloads/file_ab"), "{step}");
        assert!(step.contains("do not re-run"), "{step}");
        assert!(!step.contains("lcm_grep"), "{step}");
    }

    #[test]
    fn source_file_payload_points_at_windowed_file_read() {
        let step = next_step(FileKind::Code, false, true, "src/tools/exec.rs");
        assert!(step.contains("file_read src/tools/exec.rs"), "{step}");
        assert!(step.contains("offset"), "{step}");
    }

    #[test]
    fn structured_payload_names_the_stored_copy() {
        let step = next_step(FileKind::Json, false, false, "context/lcm/payloads/file_ab");
        assert!(step.contains("line-oriented"), "{step}");
        assert!(step.contains("file_ab"), "{step}");
    }

    #[test]
    fn test_run_detection_spans_ecosystems() {
        assert!(looks_like_test_run(
            "test result: FAILED. 1 passed; 1 failed"
        ));
        assert!(looks_like_test_run("thread 'x' panicked at src/a.rs"));
        assert!(looks_like_test_run("=== RUN   TestFoo"));
        assert!(looks_like_test_run("2 passed, 1 failed in 0.3s"));
        assert!(!looks_like_test_run("plain build output, nothing here"));
        // The word alone is not a test run: source files and logs say
        // FAILED without being one.
        assert!(!looks_like_test_run(
            "const MSG: &str = \"FAILED to connect\";"
        ));
    }

    // ── payload normalization ─────────────────────────────────────

    #[test]
    fn normalize_pretty_prints_minified_json() {
        let raw = r#"{"a":1,"b":[2,3]}"#;
        let out = normalize_payload(raw);
        assert!(matches!(out, std::borrow::Cow::Owned(_)));
        assert_eq!(
            out.trim(),
            "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}"
        );
    }

    #[test]
    fn normalize_leaves_non_json_untouched() {
        let raw = "just a log line\nwith two lines";
        assert!(matches!(
            normalize_payload(raw),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn normalize_rebuilds_framed_line_numbering() {
        // A line-numbered file_read of a JSON file: 1\t{ ... — the
        // stored copy must keep the numbering (file_read parity) and
        // the wrapper, but pretty-print the JSON body.
        let framed = "<tool_output name=\"file_read\">\n\
                      1\t{\"k\":[1,2]}\n\n\
                      (1 lines shown, 1 total, 13 bytes)\n\
                      </tool_output>";
        let out = normalize_payload(framed);
        assert!(out.starts_with("<tool_output name=\"file_read\">\n1\t{"));
        assert!(out.contains("2\t  \"k\": ["));
        assert!(out.contains("(6 lines shown, 6 total,"), "{out}");
        assert!(out.ends_with("</tool_output>"));
        // file_read parity: the stored copy must survive the exact
        // same strip that parsed the original.
        let round = strip_tool_framing(&out);
        assert!(serde_json::from_str::<serde_json::Value>(&round).is_ok());
    }

    #[test]
    fn normalize_skips_framing_when_unnumbered() {
        // A wrapped tool output without line numbers: the rebuild
        // path would inject bogus numbers, so framing is dropped
        // and the pretty JSON is stored bare.
        let wrapped = "<tool_output name=\"github_api\">\n{\"a\":1}\n</tool_output>";
        let out = normalize_payload(wrapped);
        assert!(out.trim_start().starts_with('{'));
        assert!(!out.contains("<tool_output"));
        assert!(out.contains("  \"a\": 1"));
    }

    #[test]
    fn normalize_skips_empty_frame() {
        let out = normalize_payload("");
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn extract_file_ids_dedupes_and_normalizes() {
        let content = "see file_0123456789ABCDEF and <file id=\"file_00000000000000aa\">x</file> \
                       plus file_0123456789abcdef again";
        assert_eq!(
            extract_file_ids(content),
            vec!["file_0123456789abcdef", "file_00000000000000aa"],
        );
    }

    #[test]
    fn extract_file_ids_rejects_malformed_ids() {
        // Too short, too long, and non-hex must not match.
        let content = "file_0123456789abcde file_0123456789abcdef0 file_0123456789abcdeg";
        assert_eq!(extract_file_ids(content), Vec::<String>::new());
    }

    // ── mechanical excerpt ────────────────────────────────────────

    #[test]
    fn mechanical_excerpt_keeps_head_and_tail() {
        use std::fmt::Write as _;
        let mut content = String::new();
        for i in 0..200 {
            writeln!(content, "line {i}").unwrap();
        }
        let excerpt = mechanical_excerpt(&content);
        assert!(excerpt.starts_with("Tool output excerpt (200 lines,"));
        assert!(excerpt.contains("line 0\n"));
        assert!(excerpt.contains("line 29\n"));
        assert!(excerpt.contains("line 170\n"));
        assert!(excerpt.contains("line 199\n"));
        assert!(!excerpt.contains("line 100\n"));
        assert!(excerpt.contains("[140 lines / "));
        assert!(excerpt.contains(" bytes omitted]"));
    }

    #[test]
    fn mechanical_excerpt_passes_small_content_through() {
        let content = "one\ntwo\nthree";
        assert_eq!(mechanical_excerpt(content), content);
    }

    #[test]
    fn mechanical_excerpt_byte_caps_few_line_content() {
        // One giant line: the line limit never kicks in, the byte cap
        // must.
        let content = "x".repeat(100_000);
        let excerpt = mechanical_excerpt(&content);
        assert!(excerpt.len() < 5_000);
        assert!(excerpt.contains("bytes omitted]"));
    }

    #[test]
    fn mechanical_excerpt_is_multibyte_safe() {
        let content = "€".repeat(50_000);
        let excerpt = mechanical_excerpt(&content);
        assert!(excerpt.len() < content.len());
    }

    // ── tool framing ──────────────────────────────────────────────

    #[test]
    fn strip_tool_framing_recovers_file_content() {
        let framed = "1\t{\n2\t  \"a\": 1\n3\t}\n\n(3 lines shown, 3 total, 20 bytes)";
        assert_eq!(strip_tool_framing(framed), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn strip_tool_framing_preserves_offset_reads() {
        let framed = "500\tfoo\n501\tbar\n\n(2 lines shown, 900 total, 9000 bytes)";
        assert_eq!(strip_tool_framing(framed), "foo\nbar");
    }

    #[test]
    fn strip_tool_framing_is_noop_without_trailer() {
        let content = "1\t{\n2\t}";
        assert_eq!(strip_tool_framing(content), content);
    }

    #[test]
    fn strip_tool_framing_is_noop_on_nonsequential_numbers() {
        // Numeric-first-column data that happens to end with a
        // trailer-shaped line must pass through untouched.
        let content = "1\ta\n5\tb\n\n(2 lines shown, 2 total, 8 bytes)";
        assert_eq!(strip_tool_framing(content), content);
    }

    #[test]
    fn strip_tool_framing_is_noop_on_unnumbered_lines() {
        let content = "plain text\n(1 lines shown, 1 total, 11 bytes)";
        assert_eq!(strip_tool_framing(content), content);
    }

    #[test]
    fn strip_tool_framing_peels_wrapped_file_read_output() {
        // The full live shape: safety wrapper around numbered lines.
        let content = "<tool_output name=\"file_read\">\n\
                       1\t{\n2\t  \"a\": 1\n3\t}\n\n\
                       (3 lines shown, 3 total, 20 bytes)\n\
                       </tool_output>";
        assert_eq!(strip_tool_framing(content), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn strip_tool_framing_peels_wrapper_without_line_numbers() {
        // e.g. an exec tool result: wrapper comes off, body stays.
        let content = "<tool_output name=\"exec\">\n{\"a\": 1}\n</tool_output>";
        assert_eq!(strip_tool_framing(content), "{\"a\": 1}");
    }

    #[test]
    fn strip_tool_framing_keeps_unterminated_wrapper() {
        let content = "<tool_output name=\"exec\">\ntruncated";
        assert_eq!(strip_tool_framing(content), content);
    }

    // ── kind detection ────────────────────────────────────────────

    #[test]
    fn detect_by_extension() {
        assert_eq!(detect_kind(Some("a/b.json"), "x"), FileKind::Json);
        assert_eq!(detect_kind(Some("b.csv"), "x"), FileKind::Csv);
        assert_eq!(detect_kind(Some("b.tsv"), "x"), FileKind::Tsv);
        assert_eq!(detect_kind(Some("b.yml"), "x"), FileKind::Yaml);
        assert_eq!(detect_kind(Some("b.xml"), "x"), FileKind::Xml);
        assert_eq!(detect_kind(Some("dump.sql"), "x"), FileKind::Sql);
        assert_eq!(detect_kind(Some("main.rs"), "x"), FileKind::Code);
        assert_eq!(detect_kind(Some("notes.md"), "x"), FileKind::Text);
        assert_eq!(detect_kind(Some("README"), "x"), FileKind::Text);
        assert_eq!(detect_kind(Some(".gitignore"), "x"), FileKind::Text);
    }

    #[test]
    fn detect_by_sniffing_without_path() {
        assert_eq!(detect_kind(None, "  {\"a\": 1}"), FileKind::Json);
        assert_eq!(detect_kind(None, "[1, 2]"), FileKind::Json);
        assert_eq!(detect_kind(None, "<root><a/></root>"), FileKind::Xml);
        assert_eq!(detect_kind(None, "plain prose"), FileKind::Text);
    }

    #[test]
    fn detect_binary_overrides_extension() {
        assert_eq!(detect_kind(Some("a.json"), "x\0y"), FileKind::Binary);
    }

    #[test]
    fn ansi_escapes_are_not_binary() {
        assert_eq!(detect_kind(None, "\u{1b}[31mred\u{1b}[0m"), FileKind::Text);
    }

    // ── dispatchers ───────────────────────────────────────────────

    #[test]
    fn json_shape() {
        let s = explore_json(r#"{"users": [{"id": 1}, {"id": 2}], "total": 2}"#).unwrap();
        assert!(s.contains("Top-level type: object"));
        assert!(s.contains("keys=2"));
        assert!(s.contains("users: array(len=2"));
        assert!(s.contains("total: number"));
    }

    #[test]
    fn json_parse_failure() {
        assert_eq!(explore_json("{not json"), None);
    }

    #[test]
    fn csv_columns_and_rows() {
        let s = explore_delimited("id,name,age\n1,alice,30\n2,bob,25\n", ',', "CSV");
        assert!(s.contains("Rows: 2."));
        assert!(s.contains("Columns (3): id, name, age."));
        assert!(s.contains("1, alice") || s.contains("1,alice"));
    }

    #[test]
    fn csv_empty() {
        assert!(explore_delimited("", ',', "CSV").contains("no rows found"));
    }

    #[test]
    fn yaml_top_level_keys() {
        let s = explore_yaml("name: test\nversion: 1\n  nested: skipped\nname: dup\n");
        assert!(s.contains("Top-level keys (2): name, version."));
    }

    #[test]
    fn xml_root_and_children() {
        let s = explore_xml("<root>\n<item a=\"1\">x</item>\n<meta>y</meta>\n</root>");
        assert!(s.contains("Root element: root."));
        assert!(s.contains("item"));
        assert!(s.contains("meta"));
    }

    #[test]
    fn sql_tables_and_inserts() {
        let s = explore_sql(
            "CREATE TABLE users (id INT);\ncreate table if not exists posts (id INT);\nINSERT INTO users VALUES (1);\n",
        );
        assert!(s.contains("Tables (2): users, posts."));
        assert!(s.contains("INSERT statements: 1."));
    }

    #[test]
    fn code_imports_and_signatures() {
        let src = "use std::fs;\nuse serde::Serialize;\n\npub struct Config {\n    x: u32,\n}\n\npub async fn load(path: &str) -> Config {\n    todo!()\n}\n";
        let s = explore_code(src, Some("src/config.rs"));
        assert!(s.contains("(src/config.rs)"));
        assert!(s.contains("use std::fs;"));
        assert!(s.contains("pub struct Config {"));
        assert!(s.contains("pub async fn load(path: &str) -> Config {"));
    }

    #[test]
    fn binary_hex_dump() {
        let s = explore_binary("AB\0");
        assert!(s.contains("Bytes: 3."));
        assert!(s.contains("41 42 00"));
    }

    // ── text dispatcher ───────────────────────────────────────────

    #[tokio::test]
    async fn text_uses_llm_summary() {
        let summarize = canned("A fine document about ducks.");
        let s = exploration_summary("Ducks are great.\n", None, &summarize, 400).await;
        assert_eq!(s, "A fine document about ducks.");
    }

    #[tokio::test]
    async fn text_falls_back_on_provider_error() {
        let summarize = failing();
        let s = exploration_summary("# Ducks\n\nDucks are great.\n", None, &summarize, 400).await;
        assert!(s.contains("Text exploration summary"));
        assert!(s.contains("# Ducks"));
        assert!(s.contains("Opening excerpt: # Ducks Ducks are great.."));
    }

    #[tokio::test]
    async fn invalid_json_falls_back_to_text_exploration() {
        // A truncated partial read of a JSON file: extension says
        // JSON, content doesn't parse.
        let summarize = canned("Partial JSON dump of user records.");
        let s = exploration_summary(
            "{\"users\": [{\"id\": 1}, {\"id\":",
            Some("big-data.json"),
            &summarize,
            400,
        )
        .await;
        assert_eq!(s, "Partial JSON dump of user records.");
    }

    #[tokio::test]
    async fn text_falls_back_on_empty_summary() {
        let summarize = canned("   ");
        let s = exploration_summary("hello world\n", None, &summarize, 400).await;
        assert!(s.contains("Text exploration summary"));
        assert!(s.contains("Words: 2."));
    }

    #[test]
    fn text_sample_slices_large_content() {
        let content = "a".repeat(10_000);
        let sample = build_text_sample(&content);
        assert!(sample.contains("[Document Start]"));
        assert!(sample.contains("[Document Middle]"));
        assert!(sample.contains("[Document End]"));
        assert!(sample.len() < content.len());
    }

    #[test]
    fn multibyte_content_is_sliced_safely() {
        let content = "€".repeat(5_000);
        // Must not panic on char boundaries.
        let _ = build_text_sample(&content);
        let _ = explore_text_fallback(&content, None);
        assert_eq!(detect_kind(None, &content), FileKind::Text);
    }

    #[test]
    fn instructions_carry_token_bound_and_headers() {
        let content = "# Intro\n\nbody\n";
        let headers = extract_text_headers(content);
        let i = build_text_instructions(content, Some("notes.md"), &headers, 400);
        assert!(i.contains("at most 400 tokens"));
        assert!(i.contains("# Intro"));
        assert!(i.contains("Path: notes.md"));
    }
}
