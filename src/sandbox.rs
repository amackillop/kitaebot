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
use std::str::FromStr;

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

/// Confinement tier for a child process. Names the policy a `confine`
/// invocation applies; the string forms are the CLI argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Exec-tool children: builds and checkouts, no daemon state.
    Exec,
}

/// Parse error for [`Tier`]: the unrecognized tier string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tier: {0}")]
pub struct UnknownTier(pub String);

impl FromStr for Tier {
    type Err = UnknownTier;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "exec" => Ok(Self::Exec),
            other => Err(UnknownTier(other.into())),
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exec => f.write_str("exec"),
        }
    }
}

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

    /// Per-child policy for the given tier.
    pub fn child(tier: Tier, workspace: &Path) -> Self {
        match tier {
            Tier::Exec => Self::child_exec(workspace),
        }
    }

    /// Tightened per-child policy for the exec tool.
    ///
    /// The parent grant covers the whole workspace with `from_all` because
    /// the daemon itself writes `state/`, `context/`, memory, etc. This
    /// layer enumerates what an exec child legitimately needs and omits
    /// the daemon-owned paths by default. Landlock path rules are
    /// recursive, so the workspace root gets `ReadDir` only: listing
    /// works everywhere, but file *reads* under `state/`, `context/`,
    /// `memory/`, and `.gnupg` are denied along with all writes.
    ///
    /// On the VM, `HOME` is the workspace root, so builds also need the
    /// toolchain cache dirs beneath it (nix flake eval fails hard
    /// without `~/.cache/nix`). Those are granted by name; the direnv
    /// trust db (`.local/share/direnv`) and `.config` stay denied so
    /// repo code cannot self-approve an `.envrc` the daemon would later
    /// evaluate.
    pub fn child_exec(workspace: &Path) -> Self {
        use crate::workspace::{PROJECTS_DIR, REVIEW_CHECKLIST, STATE_DIR};

        let abi = ABI_VERSION;
        let all = AccessFs::from_all(abi);
        let read_files = AccessFs::ReadFile | AccessFs::ReadDir;

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

        let dev_access = AccessFs::ReadFile | AccessFs::ReadDir | AccessFs::WriteFile;

        let mut rules = vec![
            Rule {
                path: workspace.to_path_buf(),
                access: AccessFs::ReadDir.into(),
                presence: Presence::Required,
                rationale: "Workspace root — list-only; file reads need a narrower rule",
            },
            Rule {
                path: workspace.join(PROJECTS_DIR),
                access: all,
                presence: Presence::Optional,
                rationale: "Projects — full access for builds and checkouts",
            },
            Rule {
                path: workspace.join(STATE_DIR).join(REVIEW_CHECKLIST),
                access: AccessFs::ReadFile.into(),
                presence: Presence::Optional,
                rationale: "Review checklist — read-only for exec inspection",
            },
            Rule {
                path: PathBuf::from("/nix/store"),
                access: AccessFs::from_read(abi),
                presence: Presence::Optional,
                rationale: "Nix store — read + execute",
            },
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
                rationale: "System config — read-only",
            },
            Rule {
                path: PathBuf::from("/run"),
                access: read_files,
                presence: Presence::Optional,
                rationale: "Runtime state — read-only",
            },
            Rule {
                path: PathBuf::from("/dev"),
                access: dev_access,
                presence: Presence::Optional,
                rationale: "Devices — read + write",
            },
            Rule {
                path: PathBuf::from("/proc"),
                access: read_files,
                presence: Presence::Optional,
                rationale: "Procfs — read-only",
            },
        ];

        // Toolchain caches under HOME (= the workspace root on the VM).
        // Provisioned by tmpfiles; Landlock cannot grant a missing path.
        let caches = [
            (
                ".cache",
                "Build caches — nix eval/fetcher cache, pnpm cache",
            ),
            (".cargo", "Cargo home — registry cache"),
            (".npm", "npm cache"),
            (".local/share/pnpm", "pnpm content-addressed store"),
            (".local/state/pnpm", "pnpm state"),
        ];
        rules.extend(caches.map(|(dir, rationale)| Rule {
            path: workspace.join(dir),
            access: all,
            presence: Presence::Optional,
            rationale,
        }));

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
/// Returns `Ok(())` on success or if Landlock is unsupported — the
/// daemon deliberately runs best-effort (defense-in-depth, logged).
/// Returns `Err` only on unexpected failures (e.g. bad file descriptors).
pub fn apply(workspace: &Path, socket_path: &Path) -> Result<(), SandboxError> {
    enforce(&Policy::new(workspace, socket_path)).map(|_| ())
}

/// Enforce a [`Policy`] by creating and activating a Landlock ruleset.
///
/// Logs the policy at `info` level before enforcement. After `restrict_self`
/// the ruleset is irrevocable for this process and all children.
///
/// Returns the kernel's enforcement status so callers pick their own
/// strictness: the daemon tolerates a downgrade, `confine` does not.
pub fn enforce(policy: &Policy) -> Result<RulesetStatus, SandboxError> {
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

    Ok(status.ruleset)
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

    #[test]
    fn tier_round_trips_through_its_cli_form() {
        assert_eq!("exec".parse::<Tier>(), Ok(Tier::Exec));
        assert_eq!(Tier::Exec.to_string(), "exec");
        assert!("root".parse::<Tier>().is_err());
    }

    #[test]
    fn child_dispatches_the_exec_tier() {
        let ws = Path::new("/home/agent/workspace");
        let by_tier = Policy::child(Tier::Exec, ws);
        assert_eq!(by_tier.rules(), Policy::child_exec(ws).rules());
    }

    fn child_policy() -> Policy {
        Policy::child_exec(Path::new("/home/agent/workspace"))
    }

    #[test]
    fn child_workspace_root_is_list_only_and_required() {
        let policy = child_policy();
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/home/agent/workspace"))
            .expect("workspace root must exist");
        // ReadFile here would recursively grant reads of state/,
        // context/, and the keyring — the rule is list-only.
        assert_eq!(rule.access, BitFlags::from(AccessFs::ReadDir));
        assert_eq!(rule.presence, Presence::Required);
    }

    #[test]
    fn child_projects_gets_full_access() {
        let policy = child_policy();
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/home/agent/workspace/projects"))
            .expect("projects rule must exist");
        assert_eq!(rule.access, AccessFs::from_all(ABI_VERSION));
        assert_eq!(rule.presence, Presence::Optional);
    }

    #[test]
    fn child_review_checklist_is_read_only() {
        let policy = child_policy();
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/home/agent/workspace/state/review-checklist.md"))
            .expect("checklist rule must exist");
        assert_eq!(rule.access, AccessFs::ReadFile);
        assert_eq!(rule.presence, Presence::Optional);
    }

    #[test]
    fn child_daemon_owned_paths_are_absent() {
        let policy = child_policy();
        let paths: Vec<_> = policy.rules().iter().map(|r| r.path.as_path()).collect();
        assert!(!paths.contains(&Path::new("/home/agent/workspace/state")));
        assert!(!paths.contains(&Path::new("/home/agent/workspace/context")));
        assert!(!paths.contains(&Path::new("/home/agent/workspace/config.toml")));
        assert!(!paths.contains(&Path::new("/home/agent/workspace/.gnupg")));
    }

    #[test]
    fn child_memory_is_absent() {
        let policy = child_policy();
        let paths: Vec<_> = policy.rules().iter().map(|r| r.path.as_path()).collect();
        assert!(!paths.contains(&Path::new("/home/agent/workspace/memory")));
    }

    #[test]
    fn child_tmp_excludes_device_creation() {
        let policy = child_policy();
        let rule = policy
            .rules()
            .iter()
            .find(|r| r.path == Path::new("/tmp"))
            .expect("/tmp rule must exist");
        assert!(!rule.access.contains(AccessFs::MakeChar));
        assert!(!rule.access.contains(AccessFs::MakeBlock));
        assert!(!rule.access.contains(AccessFs::MakeSock));
        assert!(!rule.access.contains(AccessFs::MakeFifo));
    }

    #[test]
    fn child_build_caches_get_full_access() {
        let policy = child_policy();
        for cache in [
            ".cache",
            ".cargo",
            ".npm",
            ".local/share/pnpm",
            ".local/state/pnpm",
        ] {
            let path = Path::new("/home/agent/workspace").join(cache);
            let rule = policy
                .rules()
                .iter()
                .find(|r| r.path == path)
                .unwrap_or_else(|| panic!("{cache} rule must exist"));
            assert_eq!(rule.access, AccessFs::from_all(ABI_VERSION));
            assert_eq!(rule.presence, Presence::Optional);
        }
    }

    #[test]
    fn child_direnv_trust_db_and_config_are_absent() {
        // Writable .local/share/direnv would let repo code self-approve
        // an .envrc the daemon later evaluates.
        let policy = child_policy();
        for rule in policy.rules() {
            let p = rule.path.to_string_lossy();
            assert!(!p.contains("direnv"), "{p} must not be granted");
            assert!(!p.contains(".config"), "{p} must not be granted");
        }
    }

    #[test]
    fn child_expected_rule_count() {
        let policy = child_policy();
        // workspace root, projects, review checklist, 5 build caches,
        // /nix/store, /tmp, /etc, /run, /dev, /proc
        assert_eq!(policy.rules().len(), 14);
    }

    #[test]
    fn child_only_workspace_root_is_required() {
        let policy = child_policy();
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
