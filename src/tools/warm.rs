//! Build-cache warmer (spec 03, Build Warm).
//!
//! Runs a repository's configured warm command inside its devshell so
//! the pre-commit hook never meets a cold Nix store. One runner per
//! directory with joining waiters, mirroring [`DirenvCache`]; only
//! `git_commit` ever waits on the outcome.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{RwLock, watch};
use tracing::{info, warn};

use super::cli_runner::{self, Confinement, SubprocessCall};
use super::direnv::DirenvCache;
use crate::sandbox::Tier;

/// How the last completed warm for a directory ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmOutcome {
    /// The command failed. Callers proceed and let the hook fail
    /// honestly rather than hiding a broken build behind a wait.
    Failed,
    Ready,
}

/// A warm is a build, not a command: this repo's `just check` cold is
/// a full `nix flake check`, ~40 minutes at 4 cores. Deliberately
/// separate from every tool timeout.
const WARM_TIMEOUT_SECS: u64 = 3600;

enum Entry {
    Done(WarmOutcome),
    /// Closes when the in-flight run finishes. A closed channel is
    /// state, not an edge, so a late waiter cannot miss the wake-up.
    Warming(watch::Receiver<()>),
}

/// Per-directory warm state. Cloned handles share one map, so the
/// tool-side and channel-side `GitCli` see the same in-flight runs.
#[derive(Clone)]
pub struct Warmer {
    direnv: DirenvCache,
    /// Confine warm commands to the exec Landlock tier, rooted at this
    /// workspace. `None` in unit tests (spec 15).
    confine_workspace: Option<PathBuf>,
    inner: Arc<RwLock<HashMap<PathBuf, Entry>>>,
}

impl Warmer {
    pub fn new(direnv: DirenvCache) -> Self {
        Self {
            direnv,
            confine_workspace: None,
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Run warm commands under the exec Landlock tier (spec 15). A
    /// warm command is repo-controlled code; it gets the same grant an
    /// exec child does.
    pub fn with_confinement(mut self, workspace: Option<PathBuf>) -> Self {
        self.confine_workspace = workspace;
        self
    }

    /// Run `command` in `dir`'s devshell and record the outcome.
    ///
    /// Unconditional by design — a warm-store run costs seconds, so
    /// re-running needs no bookkeeping — except that a call arriving
    /// while a run is in flight joins it instead of stacking a second
    /// build on the same derivations.
    pub async fn warm(&self, dir: &Path, command: &str) -> WarmOutcome {
        let tx = loop {
            let rx = {
                let mut map = self.inner.write().await;
                match map.get(dir) {
                    // Join a live run; claim a dead one (runner
                    // dropped without recording an outcome).
                    Some(Entry::Warming(rx)) if rx.has_changed().is_ok() => rx.clone(),
                    _ => {
                        let (tx, rx) = watch::channel(());
                        map.insert(dir.to_path_buf(), Entry::Warming(rx));
                        break tx;
                    }
                }
            };
            closed(rx).await;
            if let Some(Entry::Done(outcome)) = self.inner.read().await.get(dir) {
                return *outcome;
            }
        };
        let confine = self.confine_workspace.clone().map(|workspace| Confinement {
            tier: Tier::Exec,
            workspace,
        });
        let outcome = run(&self.direnv, dir, command, confine).await;
        self.inner
            .write()
            .await
            .insert(dir.to_path_buf(), Entry::Done(outcome));
        drop(tx);
        outcome
    }

    /// The recorded outcome for `dir`, waiting out an in-flight run.
    ///
    /// `None` means no warm ever ran here — unconfigured repo, or a
    /// restart lost the state. Absence and failure never block.
    pub async fn ready(&self, dir: &Path) -> Option<WarmOutcome> {
        loop {
            let rx = {
                let map = self.inner.read().await;
                match map.get(dir) {
                    None => return None,
                    Some(Entry::Done(outcome)) => return Some(*outcome),
                    Some(Entry::Warming(rx)) => rx.clone(),
                }
            };
            closed(rx.clone()).await;
            if let Some(Entry::Warming(current)) = self.inner.read().await.get(dir)
                && current.same_channel(&rx)
            {
                // The runner died without recording; do not spin.
                return Some(WarmOutcome::Failed);
            }
        }
    }
}

/// Resolve when the run owning `rx` finishes (sender dropped).
async fn closed(mut rx: watch::Receiver<()>) {
    while rx.changed().await.is_ok() {}
}

/// Execute the warm command with the devshell env injected.
async fn run(
    direnv: &DirenvCache,
    dir: &Path,
    command: &str,
    confine: Option<Confinement>,
) -> WarmOutcome {
    info!(dir = %dir.display(), command, "warming build cache");
    let mut env: Vec<(OsString, OsString)> = crate::tools::safe_env().collect();
    match direnv.get(dir).await {
        Ok(Some(devshell)) => env.extend(devshell.iter().map(|(k, v)| (k.into(), v.into()))),
        Ok(None) => {}
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "warm skipped: devshell unavailable");
            return WarmOutcome::Failed;
        }
    }
    let call = SubprocessCall {
        binary: "bash",
        args: vec!["-c".into(), command.into()],
        cwd: dir.to_path_buf(),
        env,
        timeout_secs: Some(WARM_TIMEOUT_SECS),
        stdin: None,
        confine,
    };
    match cli_runner::exec(&call).await {
        Ok(out) if out.exit_code == 0 => {
            info!(dir = %dir.display(), "build cache warm");
            WarmOutcome::Ready
        }
        Ok(out) => {
            let stderr = crate::tools::truncate_output(out.stderr.trim(), 500);
            warn!(dir = %dir.display(), exit = out.exit_code, %stderr, "warm command failed");
            WarmOutcome::Failed
        }
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "warm command failed");
            WarmOutcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No `.envrc` in any test dir: DirenvCache short-circuits to
    // `Ok(None)` and no direnv binary is involved.

    fn warmer() -> Warmer {
        Warmer::new(DirenvCache::new())
    }

    #[tokio::test]
    async fn warm_records_ready_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let w = warmer();
        assert_eq!(w.warm(dir.path(), "true").await, WarmOutcome::Ready);
        assert_eq!(w.ready(dir.path()).await, Some(WarmOutcome::Ready));
    }

    #[tokio::test]
    async fn warm_records_failed_on_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let w = warmer();
        assert_eq!(w.warm(dir.path(), "exit 3").await, WarmOutcome::Failed);
        assert_eq!(w.ready(dir.path()).await, Some(WarmOutcome::Failed));
    }

    #[tokio::test]
    async fn ready_is_none_when_never_warmed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(warmer().ready(dir.path()).await, None);
    }

    #[tokio::test]
    async fn concurrent_warms_join_one_run() {
        let dir = tempfile::tempdir().unwrap();
        let w = warmer();
        let cmd = "echo 1 >> \"$PWD/.count\"; sleep 0.3";
        let handles: Vec<_> = (0..5)
            .map(|_| {
                let (w, p) = (w.clone(), dir.path().to_path_buf());
                tokio::spawn(async move { w.warm(&p, cmd).await })
            })
            .collect();
        for h in handles {
            assert_eq!(h.await.unwrap(), WarmOutcome::Ready);
        }
        let count = std::fs::read_to_string(dir.path().join(".count")).unwrap();
        assert_eq!(count.lines().count(), 1, "5 warms must share 1 run");
    }

    #[tokio::test]
    async fn ready_waits_out_an_in_flight_run() {
        let dir = tempfile::tempdir().unwrap();
        let w = warmer();
        let runner = {
            let (w, p) = (w.clone(), dir.path().to_path_buf());
            tokio::spawn(async move { w.warm(&p, "sleep 0.3").await })
        };
        // Let the runner claim the slot before asking.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(w.ready(dir.path()).await, Some(WarmOutcome::Ready));
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn rewarm_runs_again() {
        let dir = tempfile::tempdir().unwrap();
        let w = warmer();
        let cmd = "echo 1 >> \"$PWD/.count\"";
        w.warm(dir.path(), cmd).await;
        w.warm(dir.path(), cmd).await;
        let count = std::fs::read_to_string(dir.path().join(".count")).unwrap();
        assert_eq!(count.lines().count(), 2, "warm is unconditional");
    }
}
