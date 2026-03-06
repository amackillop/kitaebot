//! Landlock filesystem sandboxing.
//!
//! Applies an irrevocable Landlock ruleset at process startup. All child
//! processes (including `sh -c` from the exec tool) inherit the restrictions.
//! On kernels without Landlock support the caller logs a warning and continues.

use std::path::Path;

use landlock::{
    ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use tracing::{info, warn};

use crate::error::SandboxError;

/// Target ABI. `set_compatibility(BestEffort)` downgrades gracefully on older
/// kernels, so we request V5 but accept whatever the running kernel supports.
const ABI_VERSION: ABI = ABI::V5;

/// Apply a Landlock filesystem sandbox scoped to `workspace`.
///
/// Returns `Ok(())` on success or if Landlock is unsupported (best-effort).
/// Returns `Err` only on unexpected failures (e.g. bad file descriptors).
pub fn apply(workspace: &Path, socket_path: &Path) -> Result<(), SandboxError> {
    let abi = ABI_VERSION;
    let all = AccessFs::from_all(abi);
    let read_only = AccessFs::from_read(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(all)
        .map_err(|e| SandboxError::Ruleset(e.to_string()))?
        .create()
        .map_err(|e| SandboxError::Ruleset(e.to_string()))?;

    // Workspace — full access.
    ruleset = add_path_rule(ruleset, workspace, all)?;

    // /nix/store — read + execute (all binaries live here on NixOS).
    // from_read(abi) already includes Execute | ReadFile | ReadDir.
    ruleset = try_add_path_rule(ruleset, Path::new("/nix/store"), read_only)?;

    // CREDENTIALS_DIRECTORY is intentionally excluded. All secrets are
    // loaded before sandbox enforcement; credential files are inaccessible
    // after this point.

    // /tmp — working access for temp files, no device creation.
    // The daemon gets PrivateTmp via systemd so this /tmp is already isolated.
    let tmp_access = AccessFs::ReadFile
        | AccessFs::ReadDir
        | AccessFs::WriteFile
        | AccessFs::MakeReg
        | AccessFs::MakeDir
        | AccessFs::MakeSym
        | AccessFs::RemoveFile
        | AccessFs::RemoveDir
        | AccessFs::Execute
        | AccessFs::Truncate;
    ruleset = try_add_path_rule(ruleset, Path::new("/tmp"), tmp_access)?;

    // /etc — read-only (resolv.conf, CA certs). NixOS /etc is a symlink farm
    // into /nix/store which already has execute; no execute needed here.
    let read_files = AccessFs::ReadFile | AccessFs::ReadDir;
    ruleset = try_add_path_rule(ruleset, Path::new("/etc"), read_files)?;

    // Socket directory — bind + cleanup. Derived from configured socket path
    // so custom paths work. More specific than the general /run rule below.
    let socket_dir_access = AccessFs::MakeSock
        | AccessFs::ReadFile
        | AccessFs::WriteFile
        | AccessFs::ReadDir
        | AccessFs::RemoveFile;
    if let Some(socket_dir) = socket_path.parent() {
        ruleset = try_add_path_rule(ruleset, socket_dir, socket_dir_access)?;
    }

    // /run — read-only (systemd runtime state, resolv.conf stub).
    ruleset = try_add_path_rule(ruleset, Path::new("/run"), read_files)?;

    // /dev — read + write for /dev/null, /dev/urandom, /dev/zero, etc.
    // /proc — read-only for /proc/self/*, /proc/meminfo, etc.
    // Landlock may or may not restrict pseudo-filesystems (procfs, devtmpfs).
    // These rules are defensive: no-ops if the kernel doesn't enforce them,
    // prevent cryptic EACCES from child processes if it does.
    let dev_access = AccessFs::ReadFile | AccessFs::ReadDir | AccessFs::WriteFile;
    ruleset = try_add_path_rule(ruleset, Path::new("/dev"), dev_access)?;
    ruleset = try_add_path_rule(ruleset, Path::new("/proc"), read_files)?;

    let status = ruleset
        .restrict_self()
        .map_err(|e| SandboxError::Ruleset(e.to_string()))?;

    match status.ruleset {
        RulesetStatus::FullyEnforced => {
            info!("Landlock sandbox applied (fully enforced)");
        }
        RulesetStatus::PartiallyEnforced => {
            warn!("Landlock sandbox applied (partially enforced — kernel too old for full ABI)");
        }
        RulesetStatus::NotEnforced => {
            warn!("Landlock not supported by running kernel — sandbox not enforced");
        }
    }

    Ok(())
}

/// Add a Landlock path rule. Fails if the path cannot be opened.
fn add_path_rule(
    ruleset: landlock::RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<landlock::RulesetCreated, SandboxError> {
    let fd = PathFd::new(path).map_err(|e| SandboxError::OpenPath {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    let rule = PathBeneath::new(fd, access).set_compatibility(CompatLevel::BestEffort);
    ruleset
        .add_rule(rule)
        .map_err(|e| SandboxError::Ruleset(e.to_string()))
}

/// Try to add a Landlock path rule. Skips if the path doesn't exist,
/// propagates other errors.
fn try_add_path_rule(
    ruleset: landlock::RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<landlock::RulesetCreated, SandboxError> {
    match PathFd::new(path) {
        Ok(fd) => {
            let rule = PathBeneath::new(fd, access).set_compatibility(CompatLevel::BestEffort);
            ruleset
                .add_rule(rule)
                .map_err(|e| SandboxError::Ruleset(e.to_string()))
        }
        Err(landlock::PathFdError::OpenCall { ref source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(ruleset)
        }
        Err(e) => Err(SandboxError::OpenPath {
            path: path.display().to_string(),
            reason: e.to_string(),
        }),
    }
}
