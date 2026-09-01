//! In-software backup staging (spec 05).
//!
//! `kitaebot backup <dest>` stages every piece of durable workspace
//! state into `dest`; the caller archives the result. Selection lives
//! here, in code, because the shell-script version drifted: new state
//! files were silently missing from backups until someone noticed.
//!
//! Anti-drift, by construction and by check:
//! - `state/` and `memory/` are staged wholesale by [`snapshot_dir`]
//!   (databases via `VACUUM INTO`, everything else copied), so a new
//!   file there is covered without anyone remembering it.
//! - `context/` is staged by the active engine's
//!   [`ContextEngine::backup`], which every engine must implement.
//! - Any workspace-root entry that is neither staged nor listed in
//!   [`DERIVED`] is reported on stderr: drift surfaces at every
//!   backup run instead of at the restore that needed the file.

use std::fs;
use std::io;
use std::path::Path;

use crate::config::EngineKind;
use crate::context::ContextEngine;
use crate::workspace::Workspace;

/// Workspace-root entries that are deliberately not backed up:
/// re-cloned, regenerated, Nix-provisioned, or rebuilt at startup.
/// Everything else at the root must be staged or it is reported.
const DERIVED: &[&str] = &[
    // Checkouts and their artifacts — re-cloned or regenerated.
    "projects",
    "reviews",
    crate::workspace::DIFFS_DIR,
    // Legacy spelling; deployed workspaces keep it until the hygiene
    // sweep exists.
    ".diffs",
    // Nix-provisioned symlinks.
    crate::workspace::CONFIG_FILE,
    "USER.md",
    // Recreated at startup from credentials (ExecStartPre).
    ".gnupg",
    // Tool caches and homes grown by exec'd processes.
    ".cache",
    ".cargo",
    ".config",
    ".local",
    ".npm",
    ".ssh",
];

/// Stage all durable state into `dest`. `dest` must exist and should
/// be empty. Returns the number of unclassified root entries found.
pub fn stage(workspace: &Workspace, engine: EngineKind, dest: &Path) -> io::Result<usize> {
    snapshot_dir(&workspace.state_dir(), &dest.join("state"))?;
    snapshot_dir(&workspace.memory_dir(), &dest.join("memory"))?;
    match engine {
        EngineKind::Flat => {
            crate::context::flat::FlatSession::backup(
                &workspace.context_dir(),
                &dest.join("context"),
            )
            .map_err(io::Error::other)?;
        }
        EngineKind::Lcm => {
            crate::context::lcm::LcmEngine::backup(&workspace.context_dir(), &dest.join("context"))
                .map_err(io::Error::other)?;
        }
    }
    report_unclassified(workspace.path(), dest)
}

/// Snapshot one directory: databases via `VACUUM INTO`, their WAL
/// sidecars skipped (the vacuum subsumes them), everything else
/// copied recursively. Directory modes are carried so a restore does
/// not widen what tmpfiles declared.
pub fn snapshot_dir(src: &Path, dest: &Path) -> io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dest)?;
    fs::set_permissions(dest, fs::metadata(src)?.permissions())?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let target = dest.join(&name);
        let path = entry.path();
        let name = name.to_string_lossy();
        if name.ends_with(".db") {
            crate::sqlite::vacuum_into(&path, &target).map_err(io::Error::other)?;
        } else if name.ends_with(".db-wal") || name.ends_with(".db-shm") {
            // Subsumed by the vacuumed snapshot.
        } else if path.is_dir() {
            snapshot_dir(&path, &target)?;
        } else {
            fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

/// Report workspace-root entries that are neither staged nor known
/// derived. Warning, not error: a stray file must not block backups,
/// but it must not vanish silently either.
fn report_unclassified(root: &Path, dest: &Path) -> io::Result<usize> {
    let mut unclassified = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if DERIVED.contains(&name.as_ref()) || dest.join(name.as_ref()).exists() {
            continue;
        }
        eprintln!("warning: {name} is neither backed up nor classified derived (src/backup.rs)");
        unclassified += 1;
    }
    Ok(unclassified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_db::StateDb;

    fn seeded_workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        // Operational DB with a row that must survive the snapshot.
        let db = StateDb::open(&ws.state_db_path()).unwrap();
        db.put_doc("duties", r#"{"last_run":{"warm":1}}"#).unwrap();
        crate::workspace::journal(&ws.journal_path(), "duty", "warm: warm").unwrap();
        std::fs::write(ws.state_dir().join("review-checklist.md"), "check\n").unwrap();
        // Engine state: an LCM-shaped context dir with a payload blob.
        std::fs::create_dir_all(ws.context_dir().join("lcm/payloads")).unwrap();
        std::fs::write(ws.context_dir().join("lcm/payloads/blob"), "payload").unwrap();
        std::fs::write(ws.context_dir().join("lcm/active_session"), "general").unwrap();
        std::fs::write(ws.path().join("memory/MEMORY.md"), "index\n").unwrap();
        // Derived state that must stay out.
        std::fs::create_dir_all(ws.path().join("projects/o/r")).unwrap();
        std::fs::write(ws.path().join("projects/o/r/f"), "derived").unwrap();
        (dir, ws)
    }

    #[test]
    fn stage_covers_every_durable_file_and_skips_derived() {
        let (_dir, ws) = seeded_workspace();
        let dest = tempfile::tempdir().unwrap();

        let unclassified = stage(&ws, EngineKind::Flat, dest.path()).unwrap();

        assert_eq!(unclassified, 0, "every root entry must be classified");
        for path in [
            "state/kitaebot.db",
            "state/JOURNAL.md",
            "state/review-checklist.md",
            "context/lcm/payloads/blob",
            "context/lcm/active_session",
            "memory/MEMORY.md",
        ] {
            assert!(dest.path().join(path).exists(), "missing {path}");
        }
        assert!(!dest.path().join("projects").exists(), "derived staged");
        // The DB snapshot is consistent and standalone: the doc row is
        // readable with no -wal beside it.
        assert!(!dest.path().join("state/kitaebot.db-wal").exists());
        let copy = StateDb::open(&dest.path().join("state/kitaebot.db")).unwrap();
        assert_eq!(
            copy.get_doc("duties").unwrap().as_deref(),
            Some(r#"{"last_run":{"warm":1}}"#)
        );
    }

    #[test]
    fn unclassified_root_entries_are_reported_not_fatal() {
        let (_dir, ws) = seeded_workspace();
        std::fs::write(ws.path().join("mystery-file"), "?").unwrap();
        let dest = tempfile::tempdir().unwrap();

        let unclassified = stage(&ws, EngineKind::Flat, dest.path()).unwrap();

        assert_eq!(unclassified, 1, "drift must be surfaced");
        assert!(dest.path().join("state/kitaebot.db").exists());
    }
}
