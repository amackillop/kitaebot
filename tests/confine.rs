//! Live Landlock verification of `kitaebot confine` (spec 15).
//!
//! Runs the real binary against a fixture workspace and asserts the
//! exec tier's kernel-enforced boundaries. The fixture lives under
//! `CARGO_TARGET_TMPDIR`, not `/tmp`: the exec tier grants `/tmp`
//! broadly, and Landlock access is the union of matching rules, so a
//! workspace under `/tmp` would inherit that grant and void the test.
//!
//! Skipped where the kernel does not expose Landlock (e.g. the nix
//! build sandbox); the VM smoke is the authoritative live check.

use std::path::Path;
use std::process::{Command, Output};

fn landlock_available() -> bool {
    std::fs::read_to_string("/sys/kernel/security/lsm")
        .is_ok_and(|lsm| lsm.split(',').any(|m| m.trim() == "landlock"))
}

/// True when the test must bail because the kernel lacks Landlock.
fn skip_without_landlock() -> bool {
    if landlock_available() {
        return false;
    }
    eprintln!("skipping: Landlock unavailable");
    true
}

fn fixture_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let p = dir.path();
    for sub in ["state", "projects", ".gnupg"] {
        std::fs::create_dir_all(p.join(sub)).unwrap();
    }
    std::fs::write(p.join("state/JOURNAL.md"), "real journal\n").unwrap();
    std::fs::write(p.join(".gnupg/secret.key"), "PRIVATE\n").unwrap();
    dir
}

/// Run `sh -c script` under `confine exec` with the fixture as both
/// workspace and cwd.
fn confine(ws: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kitaebot"))
        .args(["confine", "exec"])
        .arg(ws)
        .args(["--", "sh", "-c", script])
        .current_dir(ws)
        .output()
        .expect("failed to spawn kitaebot confine")
}

#[test]
fn state_write_is_denied_and_the_host_file_untouched() {
    if skip_without_landlock() {
        return;
    }
    let ws = fixture_workspace();
    let out = confine(ws.path(), "echo forged > state/JOURNAL.md");
    assert!(
        !out.status.success(),
        "state/ write must be denied: {out:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.path().join("state/JOURNAL.md")).unwrap(),
        "real journal\n",
        "host state/ must not change"
    );
}

#[test]
fn keyring_read_is_denied() {
    if skip_without_landlock() {
        return;
    }
    let ws = fixture_workspace();
    let out = confine(ws.path(), "cat .gnupg/secret.key");
    assert!(!out.status.success(), "keyring read must be denied");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("PRIVATE"),
        "the signing key must be invisible to the child"
    );
}

#[test]
fn projects_write_persists_to_the_host() {
    if skip_without_landlock() {
        return;
    }
    let ws = fixture_workspace();
    let out = confine(
        ws.path(),
        "echo work > projects/note.txt && cat projects/note.txt",
    );
    assert!(out.status.success(), "projects/ write must work: {out:?}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("projects/note.txt")).unwrap(),
        "work\n"
    );
}

#[test]
fn enforcement_failure_is_fail_closed() {
    // A missing workspace makes the one Required rule unopenable, so
    // enforcement errors before the exec. Runs on every kernel: the
    // path is opened before any Landlock syscall.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let marker = dir.path().join("ran");
    let out = Command::new(env!("CARGO_BIN_EXE_kitaebot"))
        .args(["confine", "exec"])
        .arg(dir.path().join("missing-workspace"))
        .args(["--", "sh", "-c", &format!("touch {}", marker.display())])
        .output()
        .expect("failed to spawn kitaebot confine");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("confine:"),
        "the denial must be reported on stderr: {out:?}"
    );
    assert!(!marker.exists(), "the command must not run");
}

#[test]
fn usage_error_exits_2_without_running_the_command() {
    let ws = fixture_workspace();
    let out = Command::new(env!("CARGO_BIN_EXE_kitaebot"))
        .args(["confine", "bogus-tier"])
        .arg(ws.path())
        .args(["--", "sh", "-c", "echo ran > projects/ran.txt"])
        .output()
        .expect("failed to spawn kitaebot confine");
    assert_eq!(out.status.code(), Some(2));
    assert!(!ws.path().join("projects/ran.txt").exists());
}
