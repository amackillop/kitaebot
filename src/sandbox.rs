//! Landlock filesystem sandboxing.
//!
//! Separates **policy** (a pure data description of allowed paths) from
//! **enforcement** (the irrevocable Landlock syscalls). This lets tests verify
//! the policy without kernel support and lets reviewers audit the access map
//! by reading [`Policy::new`] alone.
//!
//! Applied at process startup. Irrevocable. Inherited by all child processes
//! (including `sh -c` from the exec tool). On kernels without Landlock
//! support the caller logs a warning and continues (defense-in-depth).

use std::fmt;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use tracing::{info, warn};

use crate::error::SandboxError;

/// Target ABI. `set_compatibility(BestEffort)` downgrades gracefully on older
/// kernels, so we request V5 but accept whatever the running kernel supports.
const ABI_VERSION: ABI = ABI::V5;

// ── Policy data types ───────────────────────────────────────────────────

/// Whether a path must exist at enforcement time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Enforcement fails if the path cannot be opened.
    Required,
    /// Missing paths are silently skipped.
    Optional,
}

/// A single filesystem access rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Filesystem path this rule applies to.
    pub path: PathBuf,
    /// Granted access flags.
    pub access: BitFlags<AccessFs>,
    /// Whether the path must exist at enforcement time.
    pub presence: Presence,
    /// Human-readable rationale (for audit logs and documentation).
    pub rationale: &'static str,
}

/// Complete filesystem access policy.
///
/// A pure data structure describing what the sandbox allows. Constructed by
/// [`Policy::new`], consumed by [`enforce`]. Contains no I/O, makes no
/// syscalls — safe to inspect, compare, and test on any platform.
#[derive(Debug, Clone)]
pub struct Policy {
    rules: Vec<Rule>,
}

impl Policy {
    /// Build the sandbox policy for the given workspace and socket path.
    ///
    /// Pure function: no filesystem access, no syscalls.
    pub fn new(workspace: &Path, socket_path: &Path) -> Self {
        let abi = ABI_VERSION;
        let all = AccessFs::from_all(abi);
        let read_exec = AccessFs::from_read(abi);
        let read_files = AccessFs::ReadFile | AccessFs::ReadDir;

        // Build toolchains (autoconf, cmake, Go, setuptools) write temp
        // executables and symlinks to $TMPDIR. Execute and MakeSym are
        // required. Device creation remains denied.
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

        let socket_dir_access = AccessFs::MakeSock
            | AccessFs::ReadFile
            | AccessFs::WriteFile
            | AccessFs::ReadDir
            | AccessFs::RemoveFile;

        let dev_access = AccessFs::ReadFile | AccessFs::ReadDir | AccessFs::WriteFile;

        let mut rules = vec![
            Rule {
                path: workspace.to_path_buf(),
                access: all,
                presence: Presence::Required,
                rationale: "Workspace — full access for agent operations",
            },
            Rule {
                path: PathBuf::from("/nix/store"),
                access: read_exec,
                presence: Presence::Optional,
                rationale: "Nix store — read + execute (all NixOS binaries)",
            },
            // CREDENTIALS_DIRECTORY intentionally excluded. Secrets are loaded
            // before enforcement; credential files become inaccessible after.
            Rule {
                path: PathBuf::from("/tmp"),
                access: tmp_access,
                presence: Presence::Optional,
                rationale: "Temp files — working access, no device creation",
            },
            Rule {
                path: PathBuf::from("/etc"),
                access: read_files,
                presence: Presence::Optional,
                rationale: "System config — read-only (resolv.conf, CA certs)",
            },
            Rule {
                path: PathBuf::from("/run"),
                access: read_files,
                presence: Presence::Optional,
                rationale: "Runtime state — read-only (systemd, resolv.conf stub)",
            },
            Rule {
                path: PathBuf::from("/dev"),
                access: dev_access,
                presence: Presence::Optional,
                rationale: "Devices — read + write (/dev/null, /dev/urandom)",
            },
            Rule {
                path: PathBuf::from("/proc"),
                access: read_files,
                presence: Presence::Optional,
                rationale: "Procfs — read-only (/proc/self/*, /proc/meminfo)",
            },
        ];

        // Socket directory derived from configured socket path.
        if let Some(socket_dir) = socket_path.parent()
            && !socket_dir.as_os_str().is_empty()
        {
            rules.push(Rule {
                path: socket_dir.to_path_buf(),
                access: socket_dir_access,
                presence: Presence::Optional,
                rationale: "Socket directory — bind, read, write, unlink",
            });
        }

        Self { rules }
    }

    /// The ordered list of rules in this policy.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Sandbox policy ({} rules):", self.rules.len())?;
        for rule in &self.rules {
            let presence = match rule.presence {
                Presence::Required => "required",
                Presence::Optional => "optional",
            };
            writeln!(
                f,
                "  {:<30} {:?} [{}]  {}",
                rule.path.display(),
                rule.access,
                presence,
                rule.rationale,
            )?;
        }
        Ok(())
    }
}

// ── Enforcement ─────────────────────────────────────────────────────────

/// Apply a Landlock filesystem sandbox scoped to `workspace`.
///
/// Convenience wrapper: builds the policy and enforces it in one call.
/// Returns `Ok(())` on success or if Landlock is unsupported (best-effort).
/// Returns `Err` only on unexpected failures (e.g. bad file descriptors).
pub fn apply(workspace: &Path, socket_path: &Path) -> Result<(), SandboxError> {
    enforce(&Policy::new(workspace, socket_path))
}

/// Enforce a [`Policy`] by creating and activating a Landlock ruleset.
///
/// Logs the policy at `info` level before enforcement. After `restrict_self`
/// the ruleset is irrevocable for this process and all children.
pub fn enforce(policy: &Policy) -> Result<(), SandboxError> {
    info!("{policy}");

    let abi = ABI_VERSION;
    let all = AccessFs::from_all(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(all)
        .map_err(|e| SandboxError::Ruleset(e.to_string()))?
        .create()
        .map_err(|e| SandboxError::Ruleset(e.to_string()))?;

    for rule in policy.rules() {
        ruleset = match rule.presence {
            Presence::Required => add_path_rule(ruleset, &rule.path, rule.access)?,
            Presence::Optional => try_add_path_rule(ruleset, &rule.path, rule.access)?,
        };
    }

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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> Policy {
        Policy::new(
            Path::new("/home/agent/workspace"),
            Path::new("/run/kitaebot/kitaebot.sock"),
        )
    }

    #[test]
    fn workspace_gets_full_access_and_is_required() {
        let policy = test_policy();
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/home/agent/workspace"))
            .expect("workspace rule must exist");
        assert_eq!(rule.access, AccessFs::from_all(ABI_VERSION));
        assert_eq!(rule.presence, Presence::Required);
    }

    #[test]
    fn nix_store_is_read_execute() {
        let policy = test_policy();
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/nix/store"))
            .expect("/nix/store rule must exist");
        assert_eq!(rule.access, AccessFs::from_read(ABI_VERSION));
        assert_eq!(rule.presence, Presence::Optional);
    }

    #[test]
    fn tmp_excludes_device_creation() {
        let policy = test_policy();
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/tmp"))
            .expect("/tmp rule must exist");
        assert!(!rule.access.contains(AccessFs::MakeChar));
        assert!(!rule.access.contains(AccessFs::MakeBlock));
        assert!(!rule.access.contains(AccessFs::MakeSock));
        assert!(!rule.access.contains(AccessFs::MakeFifo));
        // Execute and MakeSym intentionally allowed — build toolchains
        // (autoconf, cmake, Go, setuptools) require them.
        assert!(rule.access.contains(AccessFs::Execute));
        assert!(rule.access.contains(AccessFs::MakeSym));
    }

    #[test]
    fn etc_is_read_only_no_execute() {
        let policy = test_policy();
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/etc"))
            .expect("/etc rule must exist");
        assert_eq!(rule.access, AccessFs::ReadFile | AccessFs::ReadDir);
        assert!(!rule.access.contains(AccessFs::Execute));
    }

    #[test]
    fn socket_dir_derived_from_path() {
        let policy = Policy::new(
            Path::new("/workspace"),
            Path::new("/custom/socket/dir/bot.sock"),
        );
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/custom/socket/dir"))
            .expect("socket dir rule must exist");
        assert!(rule.access.contains(AccessFs::MakeSock));
        assert_eq!(rule.presence, Presence::Optional);
    }

    #[test]
    fn bare_socket_filename_produces_no_socket_dir_rule() {
        let policy = Policy::new(Path::new("/workspace"), Path::new("bot.sock"));
        let socket_rule = policy
            .rules()
            .iter()
            .find(|r| r.rationale.contains("Socket"));
        assert!(
            socket_rule.is_none(),
            "bare filename must not produce a socket dir rule"
        );
    }

    #[test]
    fn credentials_directory_absent() {
        let policy = test_policy();
        let has_creds = policy
            .rules()
            .iter()
            .any(|r| r.path.to_string_lossy().contains("credentials"));
        assert!(
            !has_creds,
            "CREDENTIALS_DIRECTORY must not appear in policy"
        );
    }

    #[test]
    fn expected_rule_count() {
        let policy = test_policy();
        // workspace, /nix/store, /tmp, /etc, /run, /dev, /proc, socket_dir
        assert_eq!(policy.rules().len(), 8);
    }

    #[test]
    fn only_workspace_is_required() {
        let policy = test_policy();
        for rule in policy.rules() {
            if rule.path == Path::new("/home/agent/workspace") {
                assert_eq!(rule.presence, Presence::Required);
            } else {
                assert_eq!(
                    rule.presence,
                    Presence::Optional,
                    "{:?} should be Optional",
                    rule.path
                );
            }
        }
    }
}
