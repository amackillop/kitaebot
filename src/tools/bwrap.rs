//! Per-child bubblewrap confinement for the exec tool.
//!
//! The daemon's Landlock grant covers the whole workspace with
//! `from_all` (it writes state, context, memory), and Landlock is
//! inherited — so an exec child would get that same full-workspace
//! write. This builds a tighter view for each exec spawn: the
//! workspace is bound writable, but the daemon-owned paths are masked
//! with empty tmpfs, so a spawned binary (including one supplied by a
//! devshell) cannot write daemon state. The signing keyring lives
//! outside the workspace and is never bound into the view.
//!
//! Masking, not reconstruction: the workspace is bound whole so build
//! caches under HOME keep working, and only the sensitive paths are
//! hidden. `/run` is left unbound so the chat socket is unreachable,
//! and the network namespace is shared so the egress proxy on
//! loopback still routes traffic.
//!
//! Pure argv construction lives here; [`crate::tools::exec`] performs
//! the spawn. The mask set derives from the workspace layout consts,
//! so a directory rename moves the mask with it.

use std::path::Path;

use crate::workspace::{CONFIG_FILE, CONTEXT_DIR, STATE_DIR};

/// Build the `bwrap` argv that precedes the `bash -c <command>` tail.
///
/// `workspace` and `cwd` are absolute; `cwd` is under `workspace`
/// (guaranteed by the caller's working-dir validation). The signing
/// keyring needs no mask: it lives outside the workspace
/// (`/var/lib/kitaebot-gnupg`) and is simply not bound into the view.
pub fn wrap_argv(workspace: &Path, cwd: &Path) -> Vec<String> {
    let ws = workspace.to_string_lossy().into_owned();
    let mut argv: Vec<String> = Vec::new();
    let mut push = |s: &str| argv.push(s.to_string());

    // Read-only system view: all binaries live in the nix store; /etc
    // carries resolv.conf, CA certs, and nsswitch; /lib64 is scandir'd
    // by build tooling detecting libc (prisma). `--ro-bind-try`
    // tolerates the path missing on minimal roots.
    push("--ro-bind");
    push("/nix/store");
    push("/nix/store");
    push("--ro-bind-try");
    push("/etc");
    push("/etc");
    push("--ro-bind-try");
    push("/lib64");
    push("/lib64");

    // Fresh pseudo-filesystems and a private /tmp. The private /tmp
    // also denies a same-uid read of the git askpass file, which the
    // authenticated git path writes under the host /tmp.
    push("--dev");
    push("/dev");
    push("--proc");
    push("/proc");
    push("--tmpfs");
    push("/tmp");

    // The workspace, writable, with the daemon-owned paths masked by
    // empty tmpfs (writes land in throwaway memory, reads see nothing)
    // and the operator config masked by an empty read-only file.
    push("--bind");
    push(&ws);
    push(&ws);
    for dir in [STATE_DIR, CONTEXT_DIR] {
        push("--tmpfs");
        push(&format!("{ws}/{dir}"));
    }
    push("--ro-bind");
    push("/dev/null");
    push(&format!("{ws}/{CONFIG_FILE}"));

    // No network namespace: the egress proxy listens on loopback and
    // proxied traffic is addressed by IP, so DNS and /run are unneeded
    // — and leaving /run unbound keeps the chat socket unreachable.
    push("--unshare-pid");
    push("--unshare-ipc");
    push("--die-with-parent");
    push("--new-session");

    push("--chdir");
    push(&cwd.to_string_lossy());

    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Positions of `--flag VALUE...` runs, for order-independent asserts.
    fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
        argv.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    fn has_triple(argv: &[String], flag: &str, a: &str, b: &str) -> bool {
        argv.windows(3)
            .any(|w| w[0] == flag && w[1] == a && w[2] == b)
    }

    fn argv() -> Vec<String> {
        wrap_argv(Path::new("/ws"), Path::new("/ws/projects/o/r"))
    }

    #[test]
    fn masks_the_daemon_owned_paths() {
        let a = argv();
        assert!(
            has_pair(&a, "--tmpfs", "/ws/state"),
            "state/ must be masked"
        );
        assert!(
            has_pair(&a, "--tmpfs", "/ws/context"),
            "context/ must be masked"
        );
        assert!(
            has_triple(&a, "--ro-bind", "/dev/null", "/ws/config.toml"),
            "config.toml must be masked by an empty file"
        );
    }

    #[test]
    fn workspace_is_bound_writable() {
        assert!(has_triple(&argv(), "--bind", "/ws", "/ws"));
    }

    #[test]
    fn store_is_read_only() {
        assert!(has_triple(&argv(), "--ro-bind", "/nix/store", "/nix/store"));
    }

    #[test]
    fn lib64_is_bound_read_only() {
        assert!(has_triple(&argv(), "--ro-bind-try", "/lib64", "/lib64"));
    }

    #[test]
    fn network_namespace_stays_shared() {
        // The egress proxy is on loopback; an unshared netns would give
        // the child an empty loopback and cut it off.
        let a = argv();
        assert!(!a.iter().any(|s| s == "--unshare-net"));
        assert!(!a.iter().any(|s| s == "--unshare-all"));
    }

    #[test]
    fn run_is_never_bound() {
        // Leaving /run out is what keeps the chat socket unreachable.
        let a = argv();
        assert!(!a.iter().any(|s| s == "/run"));
    }

    #[test]
    fn pid_namespace_is_unshared() {
        assert!(argv().iter().any(|s| s == "--unshare-pid"));
    }

    #[test]
    fn tmp_is_private() {
        assert!(has_pair(&argv(), "--tmpfs", "/tmp"));
    }

    #[test]
    fn chdir_targets_the_working_dir() {
        assert!(has_pair(&argv(), "--chdir", "/ws/projects/o/r"));
    }

    // ── Live verification against a real bwrap ──────────────────────
    // Skips where bwrap or unprivileged userns is unavailable (some
    // CI), so the pure asserts above are the portable guarantee. The
    // VM smoke is the authoritative check.

    /// Run `sh -c script` under the wrapped view. `None` if bwrap could
    /// not set up the namespace here.
    fn run_wrapped(ws: &Path, cwd: &Path, script: &str) -> Option<std::process::Output> {
        let mut argv = wrap_argv(ws, cwd);
        argv.extend(["sh".into(), "-c".into(), script.into()]);
        let out = std::process::Command::new("bwrap")
            .args(&argv)
            .output()
            .ok()?;
        // bwrap exits 1 with this stderr when userns/mount is denied.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("Creating new namespace failed")
            || stderr.contains("bwrap: No permissions")
        {
            return None;
        }
        Some(out)
    }

    fn fixture_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for sub in ["state", "context", "projects"] {
            std::fs::create_dir_all(p.join(sub)).unwrap();
        }
        std::fs::write(p.join("state/JOURNAL.md"), "real journal\n").unwrap();
        std::fs::write(p.join("config.toml"), "[secret]\n").unwrap();
        dir
    }

    #[test]
    fn masked_state_write_does_not_reach_the_host() {
        let ws = fixture_workspace();
        let Some(out) = run_wrapped(
            ws.path(),
            ws.path(),
            "echo forged > state/JOURNAL.md && cat state/JOURNAL.md",
        ) else {
            return; // userns unavailable — pure asserts stand
        };
        // The write succeeds inside the ephemeral tmpfs...
        assert!(out.status.success(), "write to masked tmpfs should work");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "forged",
            "the child sees its own tmpfs write"
        );
        // ...but the real host file is untouched.
        assert_eq!(
            std::fs::read_to_string(ws.path().join("state/JOURNAL.md")).unwrap(),
            "real journal\n",
            "host state/ must not change"
        );
    }

    #[test]
    fn workspace_projects_stay_writable() {
        let ws = fixture_workspace();
        let Some(out) = run_wrapped(
            ws.path(),
            ws.path(),
            "echo work > projects/note.txt && cat projects/note.txt",
        ) else {
            return;
        };
        assert!(out.status.success());
        // A real write to projects/ persists to the host (bound rw).
        assert_eq!(
            std::fs::read_to_string(ws.path().join("projects/note.txt")).unwrap(),
            "work\n"
        );
    }
}
