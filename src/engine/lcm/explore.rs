//! Type-aware exploration summaries for externalized large payloads.
//!
//! Pure core of large file handling. Given payload content and an
//! optional path hint, produce a compact structural description
//! for embedding in a `<file>` reference. Only the plain-text
//! dispatcher calls the LLM (via [`SummarizeFn`]), and it falls back
//! to a deterministic summary on failure. Every other dispatcher is
//! deterministic. Mirrors `large-files.ts` in the reference
//! implementation.

use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::engine::SummarizeFn;
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
    let prefix: [u8; 8] = digest[..8].try_into().expect("sha256 digest is 32 bytes");
    format!("file_{:016x}", u64::from_be_bytes(prefix))
}

static FILE_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bfile_[a-fA-F0-9]{16}\b").expect("file id regex"));

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
/// place of the raw payload.
pub fn format_file_reference(
    file_id: &str,
    path: Option<&str>,
    token_count: usize,
    summary: &str,
) -> String {
    let path_attr = path.map_or_else(String::new, |p| format!(" path=\"{p}\""));
    format!(
        "<file id=\"{file_id}\"{path_attr} tokens=\"{token_count}\">\n{}\n</file>",
        summary.trim()
    )
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
        FileKind::Json => explore_json(content),
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
    prefix_at_char_boundary(content, BINARY_SNIFF_BYTES)
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{1b}'))
}

/// Longest prefix of at most `max_bytes` that ends on a char boundary.
fn prefix_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Longest suffix of at most `max_bytes` that starts on a char boundary.
fn suffix_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Collapse whitespace and truncate to `max_bytes`, appending `...`
/// when cut.
fn normalize_line(text: &str, max_bytes: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= max_bytes {
        return compact;
    }
    format!("{}...", prefix_at_char_boundary(&compact, max_bytes))
}

fn unique_ordered(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values
        .into_iter()
        .filter(|v| seen.insert(v.clone()))
        .collect()
}

// ── deterministic dispatchers ─────────────────────────────────────

fn explore_json(content: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) else {
        return "Structured summary (JSON): failed to parse as valid JSON.".to_string();
    };
    let top_level = match &parsed {
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
    };
    format!(
        "Structured summary (JSON):\nTop-level type: {top_level}.\nShape: {}.",
        describe_json(&parsed, 0)
    )
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
    LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_.-]+):").expect("valid regex"));

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
    LazyLock::new(|| Regex::new(r"<([A-Za-z0-9_:-]+)[\s>]").expect("valid regex"));

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
    Regex::new(r#"(?i)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z0-9_."\[\]]+)"#)
        .expect("valid regex")
});
static SQL_INSERT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bINSERT\s+INTO\b").expect("valid regex"));

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
    Regex::new(r"^\s*(use\s|import\s|from\s+\S+\s+import\s|#include\s|require\s*\()")
        .expect("valid regex")
});
static SIGNATURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:async\s+)?(?:unsafe\s+)?(?:fn|def|class|struct|enum|trait|impl|interface|function|func|type)\s",
    )
    .expect("valid regex")
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
    LazyLock::new(|| Regex::new(r"^#{1,6}\s+").expect("valid regex"));
static CAPS_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z0-9][A-Z0-9 :_-]{6,}$").expect("valid regex"));

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
    let head = prefix_at_char_boundary(content, TEXT_SLICE_BYTES);
    let tail = suffix_at_char_boundary(content, TEXT_SLICE_BYTES);
    let mut mid_start = (content.len() - TEXT_SLICE_BYTES) / 2;
    while !content.is_char_boundary(mid_start) {
        mid_start += 1;
    }
    let mid = prefix_at_char_boundary(&content[mid_start..], TEXT_SLICE_BYTES);
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
        normalize_line(prefix_at_char_boundary(content, 500), 500),
        normalize_line(suffix_at_char_boundary(content, 500), 500),
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
        assert!(a[5..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, file_id("world"));
    }

    #[test]
    fn file_reference_includes_attributes() {
        let r = format_file_reference("file_0123456789abcdef", Some("data/out.json"), 42, "sum");
        assert_eq!(
            r,
            "<file id=\"file_0123456789abcdef\" path=\"data/out.json\" tokens=\"42\">\nsum\n</file>"
        );
    }

    #[test]
    fn file_reference_omits_missing_path() {
        let r = format_file_reference("file_0123456789abcdef", None, 7, "sum");
        assert!(r.starts_with("<file id=\"file_0123456789abcdef\" tokens=\"7\">"));
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
        let s = explore_json(r#"{"users": [{"id": 1}, {"id": 2}], "total": 2}"#);
        assert!(s.contains("Top-level type: object"));
        assert!(s.contains("keys=2"));
        assert!(s.contains("users: array(len=2"));
        assert!(s.contains("total: number"));
    }

    #[test]
    fn json_parse_failure() {
        let s = explore_json("{not json");
        assert!(s.contains("failed to parse"));
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
