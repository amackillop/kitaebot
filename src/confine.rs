//! Hidden `kitaebot confine <tier> <workspace> -- <command...>` subcommand.
//!
//! Applies the per-child Landlock tier from [`crate::sandbox`] and then
//! `exec`s the command, replacing this process. The exec tool spawns it
//! via `/proc/self/exe` so children get a tighter grant than the daemon's
//! inherited one without any `pre_exec` unsafety.
//!
//! Runs before tracing is initialized: everything this process writes to
//! stderr lands in the wrapped command's output, so the success path must
//! stay silent. Strictly fail-closed: an enforcement error or a kernel
//! that cannot fully enforce the tier exits without running the command.
//!
//! # Self-re-exec via `/proc/self/exe`
//!
//! Landlock has no "apply to child" API: `landlock_restrict_self(2)`
//! restricts the *calling* process, so the restriction must run inside
//! the child, after `fork` and before the untrusted command. A
//! `pre_exec` closure could do that but is unsound in a threaded
//! runtime — between `fork` and `execve` only async-signal-safe calls
//! are allowed, and ruleset construction allocates and opens fds. So
//! the child instead re-executes this same binary with the `confine`
//! argv and does the work in a fresh single-threaded process.
//!
//! `Command::new("/proc/self/exe")` stores a string; the kernel
//! resolves it at `execve` time in the forked child, whose image is
//! still the daemon. The path is a procfs *magic link* that resolves
//! through the kernel's reference to the running executable (the open
//! file, not a path lookup), which gives three guarantees a plain path
//! cannot:
//!
//! - survives the binary being deleted or replaced on disk — on NixOS
//!   a rebuild plus store GC can remove the running daemon's store
//!   path, which would leave a startup `current_exe()` snapshot
//!   dangling;
//! - ignores `PATH`, `argv[0]`, and the working directory, so nothing
//!   in the (attacker-influenced) exec environment can redirect it;
//! - version-locks helper and daemon: the tier policy is compiled into
//!   the same artifact that spawns it, so they cannot skew across an
//!   upgrade the way a separate helper binary could.
//!
//! Re-invoking `confine` from inside the sandbox is harmless: Landlock
//! rulesets stack, so a nested enforcement can only intersect further.
//!
//! The same idiom drives runc/containerd re-exec (packaged as
//! `github.com/moby/sys/reexec`) and systemd's re-execution. To learn
//! more:
//!
//! - `proc_pid_exe(5)` — magic-link semantics
//! - <https://docs.kernel.org/userspace-api/landlock.html> — ruleset
//!   stacking, inheritance, `no_new_privs`
//! - CVE-2019-5736 — the runc breakout that abused the *inverse*
//!   direction of this link (a privileged host process opening a
//!   hostile container process's exe); a good study of the link's
//!   semantics under adversarial conditions

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use landlock::RulesetStatus;

use crate::sandbox::{self, Policy, Tier, UnknownTier};

/// A parsed `confine` invocation.
#[derive(Debug, PartialEq, Eq)]
struct Confine {
    tier: Tier,
    workspace: PathBuf,
    argv: Vec<String>,
}

/// Ways a `confine` argv can be malformed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum UsageError {
    #[error("missing command")]
    EmptyCommand,
    #[error("expected `--` before the command")]
    MissingSeparator,
    #[error("missing tier")]
    MissingTier,
    #[error("missing workspace")]
    MissingWorkspace,
    #[error("workspace must be absolute: {}", .0.display())]
    RelativeWorkspace(PathBuf),
    #[error(transparent)]
    UnknownTier(#[from] UnknownTier),
}

/// Parse the arguments after `confine`. Pure.
fn parse(mut tail: impl Iterator<Item = String>) -> Result<Confine, UsageError> {
    let tier: Tier = tail.next().ok_or(UsageError::MissingTier)?.parse()?;
    let workspace = PathBuf::from(tail.next().ok_or(UsageError::MissingWorkspace)?);
    if !workspace.is_absolute() {
        return Err(UsageError::RelativeWorkspace(workspace));
    }
    if tail.next().as_deref() != Some("--") {
        return Err(UsageError::MissingSeparator);
    }
    let argv: Vec<String> = tail.collect();
    if argv.is_empty() {
        return Err(UsageError::EmptyCommand);
    }
    Ok(Confine {
        tier,
        workspace,
        argv,
    })
}

/// Enforce the tier policy and exec the command. Never returns.
///
/// Strict, unlike the daemon's best-effort startup: anything short of
/// `FullyEnforced` exits without running the command. An operator who
/// configured the landlock tier gets the tier or nothing.
pub fn run() -> ! {
    let confine = parse(std::env::args().skip(2)).unwrap_or_else(|e| {
        eprintln!("confine: {e}");
        eprintln!("usage: kitaebot confine <tier> <workspace> -- <command...>");
        std::process::exit(2);
    });
    // GNUPGHOME travels in the env the daemon set for this child
    // (SAFE_ENV_VARS). Only the git tier grants it; reading it here
    // from an exec child changes nothing because rulesets intersect.
    let gnupg_home = std::env::var_os("GNUPGHOME").map(PathBuf::from);
    let policy = Policy::child(confine.tier, &confine.workspace, gnupg_home.as_deref());
    match sandbox::enforce(&policy) {
        Ok(RulesetStatus::FullyEnforced) => {}
        Ok(status) => {
            eprintln!("confine: landlock not fully enforced by this kernel ({status:?})");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("confine: sandbox enforcement failed: {e}");
            std::process::exit(1);
        }
    }
    let err = Command::new(&confine.argv[0])
        .args(&confine.argv[1..])
        .exec();
    eprintln!("confine: exec {}: {err}", confine.argv[0]);
    std::process::exit(127);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> impl Iterator<Item = String> {
        s.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_a_full_invocation() {
        let parsed = parse(args(&["exec", "/ws", "--", "bash", "-c", "ls"])).unwrap();
        assert_eq!(
            parsed,
            Confine {
                tier: Tier::Exec,
                workspace: PathBuf::from("/ws"),
                argv: vec!["bash".into(), "-c".into(), "ls".into()],
            }
        );
    }

    #[test]
    fn rejects_unknown_tier() {
        assert!(matches!(
            parse(args(&["root", "/ws", "--", "ls"])),
            Err(UsageError::UnknownTier(_))
        ));
    }

    #[test]
    fn rejects_relative_workspace() {
        assert!(matches!(
            parse(args(&["exec", "ws", "--", "ls"])),
            Err(UsageError::RelativeWorkspace(_))
        ));
    }

    #[test]
    fn rejects_missing_separator() {
        assert_eq!(
            parse(args(&["exec", "/ws", "ls"])),
            Err(UsageError::MissingSeparator)
        );
    }

    #[test]
    fn rejects_empty_command() {
        assert_eq!(
            parse(args(&["exec", "/ws", "--"])),
            Err(UsageError::EmptyCommand)
        );
        assert_eq!(
            parse(args(&["exec", "/ws"])),
            Err(UsageError::MissingSeparator)
        );
        assert_eq!(parse(args(&[])), Err(UsageError::MissingTier));
    }
}
