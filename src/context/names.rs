//! Session name sanitization, shared by every engine and by the
//! command layer (which must compare user input against stored names
//! in sanitized space).

/// Sanitize a session name for use as a filename.
///
/// `/` becomes `--` so repo-style names like `owner/repo` map to
/// `owner--repo`. Null bytes and `..` are stripped entirely.
pub(crate) fn sanitize_name(name: &str) -> String {
    name.replace('\0', "").replace("..", "").replace('/', "--")
}

/// Reverse the sanitization to recover the original name.
pub(crate) fn desanitize_name(stem: &str) -> String {
    stem.replace("--", "/")
}

/// The single display rendering of a stored (sanitized) session name
/// (spec 14): every operator-facing surface shows `owner/repo`, never
/// the storage form `owner--repo`. Ambiguous on names that genuinely
/// contain `--`, but storage never desanitizes, so it is display-only.
pub(crate) fn display_name(session: &str) -> String {
    desanitize_name(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_slashes() {
        assert_eq!(sanitize_name("owner/repo"), "owner--repo");
    }

    #[test]
    fn sanitize_double_dots() {
        assert_eq!(sanitize_name("../evil"), "--evil");
        assert_eq!(sanitize_name("a/../b"), "a----b");
    }

    #[test]
    fn sanitize_null_bytes() {
        assert_eq!(sanitize_name("foo\0bar"), "foobar");
    }

    #[test]
    fn desanitize_reverses_slashes() {
        assert_eq!(desanitize_name("owner--repo"), "owner/repo");
    }

    #[test]
    fn sanitize_roundtrip() {
        let name = "owner/repo";
        assert_eq!(desanitize_name(&sanitize_name(name)), name);
    }

    #[test]
    fn sanitize_plain_name_unchanged() {
        assert_eq!(sanitize_name("general"), "general");
    }

    #[test]
    fn display_name_desanitizes_storage_form() {
        assert_eq!(display_name("owner--repo"), "owner/repo");
        assert_eq!(display_name("general"), "general");
    }
}
