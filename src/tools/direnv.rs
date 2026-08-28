//! In-process direnv cache.
//!
//! Runs `direnv export json` once per working directory, caches the resulting
//! environment variables, and injects them into subprocesses via `Command::envs()`.
//!
//! # Cache invalidation
//!
//! Two `stat()` calls per lookup: `.envrc` mtime and `flake.lock` mtime.
//! A changed mtime triggers re-evaluation. Fast failures (blocked, parse
//! error) are never cached — the next caller retries. Timeouts are cached
//! with a short TTL so repeated operations during a hang degrade to
//! no-devshell immediately instead of each blocking for the full 900s
//! evaluation timeout.
//!
//! # Concurrency
//!
//! A [`tokio::sync::Notify`] per directory prevents thundering herd: only one
//! evaluation runs at a time per directory, and waiters are woken when it
//! completes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Notify, RwLock};
use tracing::debug;

use super::cli_runner::{self, SubprocessCall};

/// Cached environment variables from a direnv evaluation.
pub type DirenvEnv = Arc<HashMap<String, String>>;

/// Why a direnv evaluation did not yield an environment.
#[derive(Clone, Debug, thiserror::Error)]
pub enum DirenvError {
    /// `.envrc` exists but is not allowed: never allowed, or its content
    /// changed since it was, so direnv revoked trust. Recoverable by
    /// re-running `direnv allow` (see [`DirenvCache::allow`]) for a trusted repo.
    #[error(".envrc is not allowed")]
    Blocked,
    /// direnv failed for some other reason.
    #[error("{0}")]
    Failed(String),
    /// `direnv export json` exceeded its time budget. Cached with a short
    /// TTL so repeated operations during a hang degrade to no-devshell
    /// immediately instead of each blocking for the full timeout.
    #[error("direnv export json timed out after {secs}s")]
    Timeout {
        /// The budget that was exceeded, in seconds.
        secs: u64,
    },
}

/// Filesystem fingerprint for cache invalidation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    envrc_mtime: Option<SystemTime>,
    flake_lock_mtime: Option<SystemTime>,
}

impl Fingerprint {
    fn of(dir: &Path) -> Self {
        Self {
            envrc_mtime: mtime(&dir.join(".envrc")),
            flake_lock_mtime: mtime(&dir.join("flake.lock")),
        }
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// How long a cached timeout persists before the next caller retries.
/// Short enough that a genuinely transient hang clears quickly, long
/// enough to prevent the cascade where every operation blocks for the
/// full 900s evaluation timeout.
const TIMEOUT_TTL: Duration = Duration::from_mins(1);

enum CacheEntry {
    /// An evaluation is in progress. Waiters clone the `Notify` and await it.
    Resolving(Arc<Notify>),
    /// A completed evaluation with its fingerprint.
    Ready {
        env: DirenvEnv,
        fingerprint: Fingerprint,
    },
    /// A timeout cached until `expires_at` or a fingerprint change.
    /// Subsequent callers within the TTL and matching fingerprint get
    /// the cached error without re-running the 900s evaluation.
    Failed {
        error: DirenvError,
        fingerprint: Fingerprint,
        expires_at: Instant,
    },
}

/// Process-wide cache of direnv environments keyed by directory.
#[derive(Clone)]
pub struct DirenvCache {
    inner: Arc<RwLock<HashMap<PathBuf, CacheEntry>>>,
    /// The direnv executable. Tests substitute a fake script so no
    /// test mutates the process PATH.
    binary: &'static str,
    /// Time budget for `direnv export json`. High (900s) in production
    /// because first-time nix devshell evaluation can take minutes.
    /// Tests use a short value so timeout behavior is exercisable.
    eval_timeout_secs: u64,
}

impl DirenvCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            binary: "direnv",
            eval_timeout_secs: 900,
        }
    }

    /// A cache whose spawned direnv is `binary` — the test seam.
    #[cfg(test)]
    pub(crate) fn with_binary(binary: &'static str) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            binary,
            eval_timeout_secs: 900,
        }
    }

    /// A cache with a short evaluation timeout — for testing timeout
    /// caching without waiting 900s.
    #[cfg(test)]
    pub(crate) fn with_eval_timeout(mut self, secs: u64) -> Self {
        self.eval_timeout_secs = secs;
        self
    }

    /// Run `direnv allow` for a directory, trusting its current
    /// `.envrc` content. Must complete before a `direnv export json`
    /// call can load a devshell. Best-effort: a failure is logged, not
    /// propagated.
    pub async fn allow(&self, dir: &Path) {
        let call = SubprocessCall {
            binary: self.binary,
            args: vec!["allow".into()],
            cwd: dir.to_path_buf(),
            env: crate::tools::safe_env().collect(),
            timeout_secs: Some(10),
            stdin: None,
            // Records approval in the trust db; runs no repo code.
            confine: None,
        };
        if let Err(e) = cli_runner::exec(&call).await {
            debug!(dir = %dir.display(), error = %e, "direnv allow failed");
        }
    }

    /// Returns cached direnv env for `dir`, evaluating on cache miss.
    ///
    /// Returns `Ok(None)` if no `.envrc` exists (one `stat` call).
    /// Returns [`DirenvError::Blocked`] when the `.envrc` is not allowed,
    /// [`DirenvError::Timeout`] when the evaluation exceeded its time
    /// budget, [`DirenvError::Failed`] on any other error.
    pub async fn get(&self, dir: &Path) -> Result<Option<DirenvEnv>, DirenvError> {
        // Fast path: no .envrc means no direnv to evaluate.
        if !dir.join(".envrc").exists() {
            return Ok(None);
        }

        let fingerprint = Fingerprint::of(dir);

        loop {
            // --- Read lock: check for cache hit or in-progress evaluation ---
            {
                let cache = self.inner.read().await;
                match cache.get(dir) {
                    Some(CacheEntry::Ready {
                        env,
                        fingerprint: cached_fp,
                    }) if *cached_fp == fingerprint => {
                        return Ok(Some(Arc::clone(env)));
                    }
                    Some(CacheEntry::Failed {
                        error,
                        fingerprint: cached_fp,
                        expires_at,
                    }) if *cached_fp == fingerprint && *expires_at > Instant::now() => {
                        return Err(error.clone());
                    }
                    Some(CacheEntry::Resolving(notify)) => {
                        let notify = Arc::clone(notify);
                        drop(cache);
                        notify.notified().await;
                        // Re-check — the evaluation may have succeeded or failed.
                        continue;
                    }
                    _ => {
                        // Miss, stale, or expired failure — fall through.
                    }
                }
            }

            // --- Write lock: claim the evaluation slot ---
            let notify = {
                let mut cache = self.inner.write().await;
                // Double-check: another task may have won the race.
                match cache.get(dir) {
                    Some(CacheEntry::Ready {
                        env,
                        fingerprint: cached_fp,
                    }) if *cached_fp == fingerprint => {
                        return Ok(Some(Arc::clone(env)));
                    }
                    Some(CacheEntry::Failed {
                        error,
                        fingerprint: cached_fp,
                        expires_at,
                    }) if *cached_fp == fingerprint && *expires_at > Instant::now() => {
                        return Err(error.clone());
                    }
                    Some(CacheEntry::Resolving(notify)) => {
                        let notify = Arc::clone(notify);
                        drop(cache);
                        notify.notified().await;
                        continue;
                    }
                    _ => {}
                }
                let notify = Arc::new(Notify::new());
                cache.insert(
                    dir.to_path_buf(),
                    CacheEntry::Resolving(Arc::clone(&notify)),
                );
                notify
            };

            // --- No lock held: run direnv ---
            let result = evaluate_direnv(self.binary, self.eval_timeout_secs, dir).await;

            // --- Write lock: store result, cache timeout, or drop failure ---
            let mut cache = self.inner.write().await;
            match result {
                Ok(env) => {
                    let env = Arc::new(env);
                    cache.insert(
                        dir.to_path_buf(),
                        CacheEntry::Ready {
                            env: Arc::clone(&env),
                            fingerprint,
                        },
                    );
                    notify.notify_waiters();
                    return Ok(Some(env));
                }
                Err(e) => {
                    match e {
                        DirenvError::Timeout { .. } => {
                            // Cache timeouts with a short TTL so repeated
                            // operations during a hang don't each block
                            // for the full evaluation timeout.
                            cache.insert(
                                dir.to_path_buf(),
                                CacheEntry::Failed {
                                    error: e.clone(),
                                    fingerprint,
                                    expires_at: Instant::now() + TIMEOUT_TTL,
                                },
                            );
                        }
                        _ => {
                            // Fast failures (blocked, parse error, etc.)
                            // are not cached — next caller retries.
                            cache.remove(dir);
                        }
                    }
                    notify.notify_waiters();
                    return Err(e);
                }
            }
        }
    }
}

/// Check whether stderr contains a nix error marker. Nix prints errors
/// as lines starting with `error:`; ANSI color codes are stripped first
/// so colored output does not defeat the match. Direnv's own log
/// messages start with `direnv:` and never trigger this.
fn has_nix_error(stderr: &str) -> bool {
    stderr.lines().any(line_starts_with_error)
}

/// Whether a line, after stripping ANSI escape sequences and leading
/// whitespace, starts with `error:`. Nix's error marker is `error:` at
/// the start of a line; direnv's log messages start with `direnv:`.
fn line_starts_with_error(line: &str) -> bool {
    let target = b"error:";
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut ti = 0;
    while i < bytes.len() && ti < target.len() {
        // Skip ANSI CSI sequences: ESC [ ... (0x40..=0x7e).
        if i + 1 < bytes.len() && bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        // Skip leading whitespace before the target.
        if ti == 0 && bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i] == target[ti] {
            i += 1;
            ti += 1;
        } else {
            return false;
        }
    }
    ti == target.len()
}

/// Run `direnv export json` and parse the result.
async fn evaluate_direnv(
    binary: &'static str,
    timeout_secs: u64,
    dir: &Path,
) -> Result<HashMap<String, String>, DirenvError> {
    debug!(dir = %dir.display(), "Evaluating direnv");

    let call = SubprocessCall {
        binary,
        args: vec!["export".into(), "json".into()],
        cwd: dir.to_path_buf(),
        env: crate::tools::safe_env().collect(),
        timeout_secs: Some(timeout_secs),
        stdin: None,
        // Evaluates the flake devshell -- repo-controlled code under
        // the daemon grant. Confining it needs tier plumbing through
        // DirenvCache; planned follow-up.
        confine: None,
    };

    let output = cli_runner::exec(&call).await.map_err(|e| match e {
        crate::error::ToolError::Timeout { secs, .. } => DirenvError::Timeout { secs },
        other => DirenvError::Failed(format!("direnv exec failed: {other}")),
    })?;

    if output.exit_code != 0 {
        let stderr = output.stderr.trim();
        // direnv reports a revoked/never-granted approval as "is blocked".
        if stderr.contains("is blocked") {
            return Err(DirenvError::Blocked);
        }
        return Err(DirenvError::Failed(format!(
            "direnv export json exited {}: {stderr}",
            output.exit_code,
        )));
    }

    // direnv outputs nothing when there's no .envrc or it's not allowed.
    let stdout = output.stdout.trim();
    if stdout.is_empty() {
        return Ok(HashMap::new());
    }

    // direnv export json returns { "VAR": "value", "UNSET_VAR": null }
    let raw: HashMap<String, Option<String>> = serde_json::from_str(stdout)
        .map_err(|e| DirenvError::Failed(format!("direnv json parse failed: {e}")))?;

    // Filter out nulls (variables direnv wants to unset — irrelevant since
    // we build the subprocess env from scratch, not from the current process).
    let env: HashMap<String, String> = raw
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect();

    // A devshell signature var proves the flake evaluated: nix always
    // exports IN_NIX_SHELL and NIX_STORE from a live devshell, and a
    // failed evaluation has no devshell environment to export. The
    // subprocess env is env_clear()ed to SAFE_ENV_VARS, which contains
    // neither var, so the export diff can never omit them as already
    // present in the parent.
    if !has_devshell_signature(&env) {
        // direnv swallows `use flake` failures, exporting a bare
        // environment; nix's `error:` marker on stderr is the tell.
        let stderr = output.stderr.trim();
        if has_nix_error(stderr) {
            return Err(DirenvError::Failed(format!(
                "direnv export json reported an error: {stderr}"
            )));
        }
    }

    debug!(dir = %dir.display(), "Direnv evaluation complete");
    Ok(env)
}

/// Whether the export carries proof that a nix devshell evaluated.
fn has_devshell_signature(env: &HashMap<String, String>) -> bool {
    env.contains_key("IN_NIX_SHELL") || env.contains_key("NIX_STORE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeDirenv;

    // ── Fingerprint unit tests ──────────────────────────────────────

    #[test]
    fn fingerprint_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let fp = Fingerprint::of(dir.path());
        assert_eq!(fp.envrc_mtime, None);
        assert_eq!(fp.flake_lock_mtime, None);
    }

    #[test]
    fn fingerprint_with_envrc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();
        let fp = Fingerprint::of(dir.path());
        assert!(fp.envrc_mtime.is_some());
        assert_eq!(fp.flake_lock_mtime, None);
    }

    #[test]
    fn fingerprint_with_both_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();
        std::fs::write(dir.path().join("flake.lock"), "{}").unwrap();
        let fp = Fingerprint::of(dir.path());
        assert!(fp.envrc_mtime.is_some());
        assert!(fp.flake_lock_mtime.is_some());
    }

    // ── Cache fast-path ─────────────────────────────────────────────

    #[tokio::test]
    async fn cache_returns_none_without_envrc() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DirenvCache::new();
        let result = cache.get(dir.path()).await.unwrap();
        assert!(result.is_none());
    }

    // ── Integration tests (fake direnv binary) ──────────────────────

    #[tokio::test]
    async fn cache_parses_direnv_json() {
        let fake = FakeDirenv::install(r#"echo '{"FOO": "bar", "NUM": "42", "GONE": null}'"#);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let env = cache.get(dir.path()).await.unwrap().unwrap();

        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(env.get("NUM").map(String::as_str), Some("42"));
        assert!(
            !env.contains_key("GONE"),
            "null values must be filtered out"
        );
    }

    #[tokio::test]
    async fn cache_hit_skips_evaluation() {
        let fake = FakeDirenv::install(
            // Append a line each time direnv is invoked.
            "echo 1 >> \"$PWD/.call-count\"\necho '{\"X\": \"1\"}'",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let _ = cache.get(dir.path()).await.unwrap();
        let _ = cache.get(dir.path()).await.unwrap();

        let count = std::fs::read_to_string(dir.path().join(".call-count")).unwrap();
        assert_eq!(count.lines().count(), 1, "second get() must be a cache hit");
    }

    #[tokio::test]
    async fn concurrent_calls_deduplicated() {
        let fake = FakeDirenv::install(
            // Sleep so concurrent callers arrive while evaluation is in flight.
            "echo 1 >> \"$PWD/.call-count\"\nsleep 0.3\necho '{\"X\": \"1\"}'",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let path = dir.path().to_path_buf();

        let mut handles = Vec::new();
        for _ in 0..5 {
            let c = cache.clone();
            let p = path.clone();
            handles.push(tokio::spawn(async move { c.get(&p).await }));
        }

        for h in handles {
            let result = h.await.unwrap();
            assert!(result.unwrap().is_some());
        }

        let count = std::fs::read_to_string(dir.path().join(".call-count")).unwrap();
        assert_eq!(
            count.lines().count(),
            1,
            "5 concurrent callers must produce exactly 1 direnv invocation",
        );
    }

    #[tokio::test]
    async fn stale_fingerprint_re_evaluates() {
        let fake = FakeDirenv::install("echo 1 >> \"$PWD/.call-count\"\necho '{\"X\": \"1\"}'");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let _ = cache.get(dir.path()).await.unwrap();

        // Bump .envrc mtime to invalidate the fingerprint.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join(".envrc"), "use flake .").unwrap();

        let _ = cache.get(dir.path()).await.unwrap();

        let count = std::fs::read_to_string(dir.path().join(".call-count")).unwrap();
        assert_eq!(
            count.lines().count(),
            2,
            "stale fingerprint must trigger re-evaluation",
        );
    }

    #[tokio::test]
    async fn failed_evaluation_not_cached() {
        let fake = FakeDirenv::install("echo 'boom' >&2; exit 1");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();

        // First call fails.
        let err = cache.get(dir.path()).await.unwrap_err();
        assert!(
            matches!(&err, DirenvError::Failed(m) if m.contains("boom")),
            "error should carry stderr: {err}"
        );

        // Second call must retry (not return a cached failure).
        let err2 = cache.get(dir.path()).await.unwrap_err();
        assert!(
            matches!(&err2, DirenvError::Failed(m) if m.contains("boom")),
            "retry should re-invoke direnv: {err2}"
        );
    }

    #[tokio::test]
    async fn blocked_envrc_is_distinguished() {
        let fake = FakeDirenv::install(
            "echo 'direnv: error /x/.envrc is blocked. Run `direnv allow`.' >&2; exit 1",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let err = cache.get(dir.path()).await.unwrap_err();
        assert!(
            matches!(err, DirenvError::Blocked),
            "a blocked .envrc must report Blocked, not Failed: {err}"
        );
    }

    #[tokio::test]
    async fn timeout_is_cached_within_ttl() {
        // Sleep longer than the 1s eval timeout so direnv times out.
        let fake =
            FakeDirenv::install("echo 1 >> \"$PWD/.call-count\"\nsleep 2\necho '{\"X\": \"1\"}'");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache().with_eval_timeout(1);

        // First call times out (waits 1s).
        let err = cache.get(dir.path()).await.unwrap_err();
        assert!(
            matches!(err, DirenvError::Timeout { secs: 1 }),
            "first call should time out: {err}"
        );

        // Second call within TTL returns cached timeout without
        // re-invoking direnv — it must return immediately.
        let start = std::time::Instant::now();
        let err2 = cache.get(dir.path()).await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(err2, DirenvError::Timeout { secs: 1 }),
            "second call should return cached timeout: {err2}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "cached timeout must return in milliseconds, took {elapsed:?}"
        );

        // Only one direnv invocation: the second call hit the cache.
        let count = std::fs::read_to_string(dir.path().join(".call-count")).unwrap();
        assert_eq!(
            count.lines().count(),
            1,
            "cached timeout must not re-invoke direnv",
        );
    }

    #[tokio::test]
    async fn timeout_cache_invalidated_by_fingerprint_change() {
        let fake =
            FakeDirenv::install("echo 1 >> \"$PWD/.call-count\"\nsleep 2\necho '{\"X\": \"1\"}'");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache().with_eval_timeout(1);

        // First call times out.
        let err = cache.get(dir.path()).await.unwrap_err();
        assert!(matches!(err, DirenvError::Timeout { secs: 1 }));

        // Second call within TTL returns cached timeout.
        let err2 = cache.get(dir.path()).await.unwrap_err();
        assert!(matches!(err2, DirenvError::Timeout { secs: 1 }));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".call-count"))
                .unwrap()
                .lines()
                .count(),
            1,
        );

        // Fix the .envrc — fingerprint change must invalidate the
        // cached timeout and trigger a re-evaluation.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join(".envrc"), "use flake .").unwrap();

        let err3 = cache.get(dir.path()).await.unwrap_err();
        assert!(matches!(err3, DirenvError::Timeout { secs: 1 }));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".call-count"))
                .unwrap()
                .lines()
                .count(),
            2,
            "fingerprint change must re-evaluate despite cached timeout",
        );
    }

    // ── Silent failure detection ───────────────────────────────────

    #[test]
    fn line_starts_with_error_plain() {
        assert!(line_starts_with_error("error: Failed to fetch"));
    }

    #[test]
    fn line_starts_with_error_ansi_colored() {
        assert!(line_starts_with_error(
            "\x1b[31;1merror:\x1b[0m Failed to fetch"
        ));
    }

    #[test]
    fn line_starts_with_error_leading_whitespace() {
        assert!(line_starts_with_error("  error: something"));
    }

    #[test]
    fn line_starts_with_error_ansi_then_whitespace() {
        assert!(line_starts_with_error("\x1b[0m  error: something"));
    }

    #[test]
    fn line_does_not_match_direnv_log() {
        assert!(!line_starts_with_error("direnv: loading flake"));
        assert!(!line_starts_with_error("direnv: export GEM_HOME"));
    }

    #[test]
    fn line_does_not_match_indented_error() {
        assert!(!line_starts_with_error("  some context: error: nested"));
    }

    #[test]
    fn line_does_not_match_empty() {
        assert!(!line_starts_with_error(""));
    }

    #[test]
    fn has_nix_error_detects_in_multiline_stderr() {
        let stderr = "direnv: loading flake\nerror: Failed to fetch git repository\n";
        assert!(has_nix_error(stderr));
    }

    #[test]
    fn has_nix_error_false_on_direnv_logs_only() {
        let stderr = "direnv: loading flake\ndirenv: export PATH\n";
        assert!(!has_nix_error(stderr));
    }

    #[tokio::test]
    async fn cache_detects_silent_flake_failure() {
        // Simulate direnv exit 0 with a nix error on stderr and an
        // empty JSON export — the real-world `use flake` failure.
        let fake =
            FakeDirenv::install("echo 'error: Failed to fetch git repository' >&2\necho '{}'");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let err = cache.get(dir.path()).await.unwrap_err();
        assert!(
            matches!(&err, DirenvError::Failed(m) if m.contains("Failed to fetch")),
            "silent flake failure must surface as Failed with stderr: {err}"
        );
    }

    #[tokio::test]
    async fn cache_succeeds_with_direnv_logs_on_stderr() {
        // Simulate direnv exit 0 with log messages on stderr and a
        // valid JSON export — normal successful operation.
        let fake = FakeDirenv::install(
            "echo 'direnv: loading flake' >&2\necho 'direnv: export PATH' >&2\necho '{\"PATH\": \"/nix/store/bin\"}'",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let env = cache.get(dir.path()).await.unwrap().unwrap();
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some("/nix/store/bin"),
            "direnv log messages on stderr must not cause a false failure"
        );
    }

    #[tokio::test]
    async fn cache_trusts_devshell_export_despite_hook_error_line() {
        // The #114 incident: a devshell evaluates fully (signature
        // vars exported) but its shellHook contains a non-fatal
        // failing step whose tool prints an `error:`-prefixed line
        // (just's recipe format). The working export must win.
        let fake = FakeDirenv::install(
            "echo 'error: recipe `pnpm-install` failed on line 19 with exit code 1' >&2\necho '{\"IN_NIX_SHELL\": \"impure\", \"NIX_STORE\": \"/nix/store\", \"PATH\": \"/nix/store/bin\"}'",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let env = cache.get(dir.path()).await.unwrap().unwrap();
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some("/nix/store/bin"),
            "a signature-bearing export must not be failed on shellHook noise"
        );
    }

    #[tokio::test]
    async fn cache_fails_manual_export_without_signature_and_nix_error() {
        // An .envrc that sets variables before a failing `use flake`
        // (the `export NIX_CONFIG=...` pattern) exports non-empty but
        // carries no devshell signature — the nix error must still
        // classify it Failed (#41 semantics preserved).
        let fake = FakeDirenv::install(
            "echo 'error: Failed to fetch git repository' >&2\necho '{\"NIX_CONFIG\": \"extra-experimental-features =\"}'",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = fake.cache();
        let err = cache.get(dir.path()).await.unwrap_err();
        assert!(
            matches!(&err, DirenvError::Failed(m) if m.contains("Failed to fetch")),
            "signature-less export with a nix error must stay Failed: {err}"
        );
    }
}
