//! Credential loading and the `Secret` newtype.
//!
//! Secrets are loaded from files provisioned by systemd `LoadCredential=`.
//! The `Secret` newtype prevents accidental logging — `Debug` and `Display`
//! both render as `[REDACTED]`.

use std::path::Path;

use crate::error::SecretError;

/// A secret value that cannot be accidentally logged.
///
/// `Debug` and `Display` both produce `[REDACTED]`. Use `.expose()` to
/// access the inner value when you actually need it (e.g., HTTP headers).
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Access the secret value. Call this only at the point of use
    /// (e.g., building an Authorization header), never for logging.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Load a secret from the credential directory provisioned by systemd `LoadCredential=`.
///
/// Reads `$CREDENTIALS_DIRECTORY/<name>` and returns the trimmed contents.
/// For local dev, set `CREDENTIALS_DIRECTORY=./secrets` and place one file per secret.
pub fn load_secret(name: &str) -> Result<Secret, SecretError> {
    let dir = std::env::var("CREDENTIALS_DIRECTORY").map_err(|_| SecretError::NoCredentialsDir)?;
    let path = Path::new(&dir).join(name);
    std::fs::read_to_string(&path)
        .map(|s| Secret(s.trim().to_string()))
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SecretError::NotFound {
                name: name.to_string(),
            },
            _ => SecretError::Read {
                name: name.to_string(),
                source: e,
            },
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_secret(value: &str) -> Secret {
        Secret(value.to_string())
    }

    #[test]
    fn debug_is_redacted() {
        let s = make_secret("sk-or-supersecret");
        assert_eq!(format!("{s:?}"), "[REDACTED]");
    }

    #[test]
    fn display_is_redacted() {
        let s = make_secret("sk-or-supersecret");
        assert_eq!(format!("{s}"), "[REDACTED]");
    }

    #[test]
    fn expose_returns_inner_value() {
        let s = make_secret("sk-or-supersecret");
        assert_eq!(s.expose(), "sk-or-supersecret");
    }

    #[test]
    fn load_reads_and_trims() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test-key"), "  secret-value \n").unwrap();

        // SAFETY: test-only, no concurrent threads depend on this var.
        unsafe { std::env::set_var("CREDENTIALS_DIRECTORY", dir.path()) };
        let secret = load_secret("test-key").unwrap();
        unsafe { std::env::remove_var("CREDENTIALS_DIRECTORY") };

        assert_eq!(secret.expose(), "secret-value");
    }

    #[test]
    fn load_missing_file_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();

        unsafe { std::env::set_var("CREDENTIALS_DIRECTORY", dir.path()) };
        let err = load_secret("nonexistent").unwrap_err();
        unsafe { std::env::remove_var("CREDENTIALS_DIRECTORY") };

        assert!(matches!(err, SecretError::NotFound { .. }));
    }

    #[test]
    fn load_missing_dir_returns_no_credentials_dir() {
        unsafe { std::env::remove_var("CREDENTIALS_DIRECTORY") };
        let err = load_secret("anything").unwrap_err();
        assert!(matches!(err, SecretError::NoCredentialsDir));
    }
}
