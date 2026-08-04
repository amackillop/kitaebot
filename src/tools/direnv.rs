//! In-process direnv cache.
//!
//! Runs `direnv export json` once per working directory, caches the resulting
//! environment variables, and injects them into subprocesses via `Command::envs()`.
//!
//! # Cache invalidation
//!
//! Two `stat()` calls per lookup: `.envrc` mtime and `flake.lock` mtime.
//! A changed mtime triggers re-evaluation. Failures are never cached — the
//! next caller retries.
//!
//! # Concurrency
//!
//! A [`tokio::sync::Notify`] per directory prevents thundering herd: only one
//! evaluation runs at a time per directory, and waiters are woken when it
//! completes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::{Notify, RwLock};
use tracing::debug;

use super::cli_runner::{self, SubprocessCall};

/// Cached environment variables from a direnv evaluation.
pub type DirenvEnv = Arc<HashMap<String, String>>;

/// Why a direnv evaluation did not yield an environment.
#[derive(Debug, thiserror::Error)]
pub enum DirenvError {
    /// `.envrc` exists but is not allowed: never allowed, or its content
    /// changed since it was, so direnv revoked trust. Recoverable by
    /// re-running `direnv allow` (see [`allow`]) for a trusted repo.
    #[error(".envrc is not allowed")]
    Blocked,
    /// direnv failed for some other reason.
    #[error("{0}")]
    Failed(String),
}

/// Run `direnv allow` for a directory, trusting its current `.envrc`
/// content. Must complete before a `direnv export json` call can load a
/// devshell. Best-effort: a failure is logged, not propagated.
pub async fn allow(dir: &Path) {
    let call = SubprocessCall {
        binary: "direnv",
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

enum CacheEntry {
    /// An evaluation is in progress. Waiters clone the `Notify` and await it.
    Resolving(Arc<Notify>),
    /// A completed evaluation with its fingerprint.
    Ready {
        env: DirenvEnv,
        fingerprint: Fingerprint,
    },
}

/// Process-wide cache of direnv environments keyed by directory.
#[derive(Clone)]
pub struct DirenvCache {
    inner: Arc<RwLock<HashMap<PathBuf, CacheEntry>>>,
}

impl DirenvCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns cached direnv env for `dir`, evaluating on cache miss.
    ///
    /// Returns `Ok(None)` if no `.envrc` exists (one `stat` call).
    /// Returns [`DirenvError::Blocked`] when the `.envrc` is not allowed,
    /// [`DirenvError::Failed`] on any other error.
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
                    Some(CacheEntry::Resolving(notify)) => {
                        let notify = Arc::clone(notify);
                        drop(cache);
                        notify.notified().await;
                        // Re-check — the evaluation may have succeeded or failed.
                        continue;
                    }
                    _ => {
                        // Miss or stale — fall through to write lock.
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
            let result = evaluate_direnv(dir).await;

            // --- Write lock: store result or remove on failure ---
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
                    // Don't cache failures — next caller retries.
                    cache.remove(dir);
                    notify.notify_waiters();
                    return Err(e);
                }
            }
        }
    }
}

/// Run `direnv export json` and parse the result.
async fn evaluate_direnv(dir: &Path) -> Result<HashMap<String, String>, DirenvError> {
    debug!(dir = %dir.display(), "Evaluating direnv");

    let call = SubprocessCall {
        binary: "direnv",
        args: vec!["export".into(), "json".into()],
        cwd: dir.to_path_buf(),
        env: crate::tools::safe_env().collect(),
        timeout_secs: Some(900),
        stdin: None,
        // Evaluates the flake devshell -- repo-controlled code under
        // the daemon grant. Confining it needs tier plumbing through
        // DirenvCache; planned follow-up.
        confine: None,
    };

    let output = cli_runner::exec(&call)
        .await
        .map_err(|e| DirenvError::Failed(format!("direnv exec failed: {e}")))?;

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
    let env = raw
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect();

    debug!(dir = %dir.display(), "Direnv evaluation complete");
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ENV_LOCK, FakeDirenv};

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
        let _lock = ENV_LOCK.lock().await;
        let _fake = FakeDirenv::install(r#"echo '{"FOO": "bar", "NUM": "42", "GONE": null}'"#);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = DirenvCache::new();
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
        let _lock = ENV_LOCK.lock().await;
        let _fake = FakeDirenv::install(
            // Append a line each time direnv is invoked.
            "echo 1 >> \"$PWD/.call-count\"\necho '{\"X\": \"1\"}'",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = DirenvCache::new();
        let _ = cache.get(dir.path()).await.unwrap();
        let _ = cache.get(dir.path()).await.unwrap();

        let count = std::fs::read_to_string(dir.path().join(".call-count")).unwrap();
        assert_eq!(count.lines().count(), 1, "second get() must be a cache hit");
    }

    #[tokio::test]
    async fn concurrent_calls_deduplicated() {
        let _lock = ENV_LOCK.lock().await;
        let _fake = FakeDirenv::install(
            // Sleep so concurrent callers arrive while evaluation is in flight.
            "echo 1 >> \"$PWD/.call-count\"\nsleep 0.3\necho '{\"X\": \"1\"}'",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = DirenvCache::new();
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
        let _lock = ENV_LOCK.lock().await;
        let _fake = FakeDirenv::install("echo 1 >> \"$PWD/.call-count\"\necho '{\"X\": \"1\"}'");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = DirenvCache::new();
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
        let _lock = ENV_LOCK.lock().await;
        let _fake = FakeDirenv::install("echo 'boom' >&2; exit 1");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = DirenvCache::new();

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
        let _lock = ENV_LOCK.lock().await;
        let _fake = FakeDirenv::install(
            "echo 'direnv: error /x/.envrc is blocked. Run `direnv allow`.' >&2; exit 1",
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();

        let cache = DirenvCache::new();
        let err = cache.get(dir.path()).await.unwrap_err();
        assert!(
            matches!(err, DirenvError::Blocked),
            "a blocked .envrc must report Blocked, not Failed: {err}"
        );
    }
}
