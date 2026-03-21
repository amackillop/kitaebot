# Spec 15: Sandbox

## Motivation

Kernel-enforced filesystem confinement via Linux Landlock LSM. Applied at
process startup, irrevocable, inherited by all child processes (including
`bash -c` from the exec tool).

## Behavior

### Architecture

The implementation separates **policy** (pure data) from **enforcement**
(Landlock syscalls):

- `Policy::new(workspace, socket_path)` — pure function, builds a `Vec<Rule>`.
  Testable on any platform.
- `enforce(policy)` — creates a Landlock ruleset, adds rules, calls
  `restrict_self()`.
- `apply(workspace, socket_path)` — convenience wrapper composing the two.

### Filesystem Policy

Targets Landlock ABI V5 (Linux 6.7+) with `BestEffort` compatibility for
graceful downgrade on older kernels.

| Path | Access | Presence |
|------|--------|----------|
| Workspace (dynamic) | Full access (`AccessFs::from_all`) | **Required** |
| `/nix/store` | Read + execute | Optional |
| `/tmp` | Read, write, mkdir, symlink, unlink, execute, truncate. No `MakeChar`, `MakeBlock`, `MakeSock`, `MakeFifo`. | Optional |
| `/etc` | `ReadFile`, `ReadDir` | Optional |
| `/run` | `ReadFile`, `ReadDir` | Optional |
| `/dev` | `ReadFile`, `ReadDir`, `WriteFile` | Optional |
| `/proc` | `ReadFile`, `ReadDir` | Optional |
| Socket parent dir (dynamic) | `MakeSock`, `ReadFile`, `WriteFile`, `ReadDir`, `RemoveFile` | Optional |
| Everything else | Denied | — |

Only the workspace rule is **Required** (failure to add it is a hard error).
All other rules are **Optional** — if the path doesn't exist, the rule is
silently skipped.

`CREDENTIALS_DIRECTORY` is intentionally **excluded**. All secrets are loaded
into memory before sandbox enforcement; credential files become inaccessible
after.

NixOS note: `/usr` and `/bin` don't exist. All binaries live in `/nix/store`.
`/etc` is a symlink farm into `/nix/store`.

### Enforcement

1. Create a ruleset handling all filesystem access types
2. For Required rules: open `PathFd`, add `PathBeneath`. Failure is `SandboxError`.
3. For Optional rules: same, but `NotFound` is silently skipped.
4. `restrict_self()` — irrevocable, inherited by all children.

### Enforcement Status

| Status | Action |
|--------|--------|
| `FullyEnforced` | `info!` log |
| `PartiallyEnforced` | `warn!` log (kernel too old for full ABI) |
| `NotEnforced` | `warn!` log (Landlock unsupported entirely) |

All three return `Ok(())`. In `main.rs`, any `Err` from `apply()` is logged
as a warning — the process continues regardless. Sandbox failure is never
fatal.

## Boundaries

### Owns

- Filesystem policy definition (paths, access flags, required vs optional)
- Landlock ruleset creation and enforcement
- `SandboxError` type

### Does Not Own

- Workspace path — provided by the workspace module
- Socket path — provided by config
- When to apply the sandbox — `main.rs` handles the call ordering
- Secret loading — must happen before enforcement (handled by `main.rs`)

### Defense-in-Depth Stack

1. VM isolation (QEMU)
2. Egress filter (nftables + dnsmasq)
3. Unprivileged user (`kitaebot`)
4. systemd hardening (`ProtectSystem`, `NoNewPrivileges`, seccomp)
5. **Landlock filesystem confinement** (this spec)
6. Exec deny-list (heuristic UX layer)
7. `PathGuard` (file tool workspace confinement)
8. Output leak detection

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Required path doesn't exist | `SandboxError::OpenPath`, logged as warning, process continues unsandboxed |
| Optional path doesn't exist | Rule silently skipped |
| Landlock unsupported | Warning logged, process continues |
| Kernel too old for ABI V5 | `BestEffort` downgrades access flags |

## Constraints

- Targets Landlock ABI V5 (Linux 6.7+)
- `BestEffort` compatibility on both ruleset and individual rules
- Enforcement is irrevocable — no runtime modification
- Secrets must be loaded before `apply()` is called

## Open Questions

- Should a failed Required rule (e.g. workspace path doesn't exist) be fatal
  rather than a warning? Currently the process runs unsandboxed.
