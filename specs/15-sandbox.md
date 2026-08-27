# Spec 15: Sandbox

## Motivation

Kernel-enforced filesystem confinement in two levels. The daemon
confines itself with Linux Landlock at startup (broad, inherited).
Because that grant must include full workspace write — the daemon writes
`state/`, `context/`, the journal, and memory — every child inherits
full-workspace write too. So a second, tighter boundary is applied
**per child** to every same-uid spawn that runs repo-influenced code
(the exec tool, warm commands, and `GitCli`): the child re-enforces a
narrower Landlock tier on itself before running the command, moving the
`state/`/`context/`/keyring fence from the heuristic layers
([spec 03](03-tools.md) deny-list, [spec 05](05-workspace.md)
PathGuard) into the kernel. The signing keyring lives outside the
workspace entirely ([spec 13](13-credentials.md)), so no child tier
names it at all.

## Behavior

### Per-child confinement (Landlock tiers)

`exec.sandbox` selects the mechanism: `"landlock"` (the default),
`"bwrap"`, or `"off"`. In `landlock` mode every same-uid child that runs
repo-influenced code is wrapped, each in the tier that fits it:

| Spawn | Tier | Grants beyond exec |
|-------|------|--------------------|
| exec tool (`bash -c`) | `exec` | — |
| warm command (`Warmer`) | `exec` | — |
| `GitCli`: clone, fetch, push, commit + hooks | `git` | `reviews/`, the askpass helper (r+x), the signing keyring |

A wrapped command is spawned as

```
/proc/self/exe confine <tier> <workspace> -- <binary> <args...>
```

`confine` is a hidden subcommand dispatched in `main` before tracing and
the tokio runtime. It builds `Policy::child(tier, workspace)`, calls the
same `enforce()` the daemon uses (Landlock rulesets stack — the child's
effective access is the intersection with the inherited grant), and
`exec()`s the command tail. The `git` tier is a superset of `exec`, so
re-invoking `confine git` from inside an already-`exec`-confined child
cannot widen access: the intersection keeps the keyring and askpass
denied. `confine` is strictly fail-closed, unlike the
daemon's best-effort startup: an enforcement error *or* any kernel
downgrade (anything short of `FullyEnforced`) exits 1 and the command
does not run. An operator who configured the landlock tier gets the tier
or nothing. The success path writes nothing to stderr because that
stream belongs to the wrapped command.

`/proc/self/exe` is a procfs magic link (`proc_pid_exe(5)`), resolved by
the kernel at `execve` time in the forked child through its reference to
the running executable rather than a path lookup. The wrapper therefore
survives the daemon's store path being rebuilt or GC'd mid-flight and
cannot be redirected via `PATH`, `argv[0]`, or the working directory. It
also keeps the tier policy version-locked to the daemon that spawns it.
The `src/confine.rs` module docs cover the mechanism and further reading.

The `exec` tier (`Policy::child_exec`):

| Path | Access | Effect |
|------|--------|--------|
| Workspace root | list only (`ReadDir`) | Navigation works. Landlock rules are recursive, so `ReadFile` here would grant reads of everything beneath; with list-only, file reads and all writes under `state/`, `context/`, `memory/`, and `config.toml` are denied, and new root-level files cannot be created |
| `projects/` | full | Builds, checkouts, `.diffs/` all work |
| `state/review-checklist.md` | read | The one model-facing state file |
| `context/lcm/payloads/` | read | The LCM payload store: `<file>` references hand these workspace-relative paths to the model ([spec 14](14-context-engine.md)), so shell reads of them must work. Optional — created lazily on first externalization, and references only exist after that point |
| `.cache`, `.cargo`, `.npm`, `.local/{share,state}/pnpm` | full | `HOME` is the workspace root on the VM; nix flake eval fails hard without `~/.cache/nix`, and cargo/npm/pnpm need their caches. Provisioned by tmpfiles — Landlock cannot grant a path that does not exist. Grants are named, never speculative: onboarding a repo whose toolchain caches outside XDG (`.m2`, `.gradle`, `~/go/pkg/mod`, …) means adding its rule in `Policy::child_exec` plus a tmpfiles entry in the same commit |
| `.local/share/direnv`, `.config` | denied | The direnv trust db: repo code must not self-approve an `.envrc` the daemon later evaluates |
| `/nix/store` | read + execute | Binaries |
| `/tmp` | working access + `MakeSock` | Device nodes still denied. Sockets allowed: e2e/kchat test daemons bind here, `projects/` grants MakeSock anyway (from_all), abstract AF_UNIX bypasses FS rules, and PrivateTmp keeps bound sockets service-private. Anything that later *connects* to a `/tmp` socket must assume a repo-code child may have bound that path first |
| `/etc`, `/run`, `/proc` | read | resolv.conf, CA certs, procfs |
| `/lib64` | read | Build tooling scandirs it to detect libc (prisma postinstall); no execute, so the compat loader cannot run foreign binaries |
| `/dev` | read + write | `/dev/null`, `/dev/urandom` |
| Everything else | denied | Including the signing keyring at `/var/lib/kitaebot-gnupg`, which lives outside the workspace and is granted only in the daemon policy ([spec 13](13-credentials.md)) |

Reads of `state/` and `context/` are denied because no rule names them
and the workspace-root rule is not recursive-write: Landlock denies
whatever no rule grants. The payload store is the one named exception
under `context/`. Unix-socket **connects are not mediated**
by this ruleset — the chat socket stays reachable path-wise, which is why
the socket's SO_PEERCRED uid gate is load-bearing.

The `git` tier (`Policy::child_git`) is the exec tier plus three rules,
for the credential-bearing paths and repo hooks:

| Path | Access | Effect |
|------|--------|--------|
| `reviews/` | full | The GitHub channel prepares review worktrees here |
| `state/askpass/` | read + execute | git runs the `GIT_ASKPASS` token helper; `state/` is otherwise denied, so exec children cannot read the token during the git window |
| `/var/lib/kitaebot-gnupg` | full | `git commit` signs; gpg auto-spawns its agent in the keyring dir |

The askpass helper moved out of the shared `/tmp` (which the exec tier
grants broadly) into `state/askpass/`, so only the git tier — never a
concurrent exec child — can read the token script.

The daemon's own policy (`Policy::new`) additionally grants read +
execute on its own binary, resolved via `current_exe()` in `apply()`.
The daemon re-execs `/proc/self/exe` to launch every `confine` child;
without this grant that re-exec fails `EACCES` on any thread already
under the ruleset. On the VM the binary is under `/nix/store` (already
granted); the explicit rule covers dev and test builds under `target/`.

Live tests (`tests/confine.rs`) run the real binary and assert a `state/`
write is denied and leaves the host untouched, keyring reads are denied,
and `projects/` writes persist; they skip where the kernel lacks
Landlock. The authoritative check is the VM smoke, which gates flipping
the default on.

**Alternative: bubblewrap** (`"bwrap"`). Kept as a non-default option: a
mount-namespace view that masks the daemon-owned paths with tmpfs
(re-binding `context/lcm/payloads/` read-only on top, mirroring the
Landlock carve-out) and additionally unshares pid/ipc and hides `/run`. Stronger in those corners,
but requires loosening `RestrictNamespaces` and the mount syscalls on the
trusted daemon unit, which is why Landlock-in-child is the default
mechanism. Argv construction lives in `tools::bwrap::wrap_argv` and stays
unit-tested.

In `landlock` mode the confined spawns are: the exec tool and warm
commands (exec tier), and every `GitCli` call including hooks (git
tier). Fixed-argv, hook-free git calls (origin lookup, current-branch)
stay unconfined by design — they run no repo code. **Not yet covered:**
direnv flake-devshell evaluation still runs under the daemon's
inherited grant; confining it needs the tier threaded through
`DirenvCache`, a planned follow-up.

### Architecture

The implementation separates **policy** (pure data) from **enforcement**
(Landlock syscalls):

- `Policy::new(workspace, socket_path, gnupg_home)` — pure function,
  builds the daemon `Vec<Rule>`. Testable on any platform.
- `Policy::child(tier, workspace, gnupg_home)` — the per-child tiers
  (`Policy::child_exec`, `Policy::child_git`), also pure.
- `enforce(policy)` — creates a Landlock ruleset, adds rules, calls
  `restrict_self()`, returns the `RulesetStatus`.
- `apply(workspace, socket_path, gnupg_home)` — the daemon wrapper;
  resolves `current_exe()` and grants it before enforcing so the
  `/proc/self/exe` re-exec works.

### Filesystem Policy

The daemon policy (`Policy::new`). Targets Landlock ABI V5 (Linux 6.7+)
with `BestEffort` compatibility for graceful downgrade on older kernels.
The per-child tiers are tabulated under Per-child confinement above.

| Path | Access | Presence |
|------|--------|----------|
| Workspace (dynamic) | Full access (`AccessFs::from_all`) | **Required** |
| `/nix/store` | Read + execute | Optional |
| `/tmp` | Read, write, mkdir, symlink, unlink, execute, truncate. No `MakeChar`, `MakeBlock`, `MakeSock`, `MakeFifo`. | Optional |
| `/etc` | `ReadFile`, `ReadDir` | Optional |
| `/run` | `ReadFile`, `ReadDir` | Optional |
| `/dev` | `ReadFile`, `ReadDir`, `WriteFile` | Optional |
| `/proc` | `ReadFile`, `ReadDir` | Optional |
| `/sys/fs/cgroup` | `ReadFile`, `ReadDir` | Optional (timeout evidence reads pressure stats, issue #74) |
| Socket parent dir (dynamic) | `MakeSock`, `ReadFile`, `WriteFile`, `ReadDir`, `RemoveFile` | Optional |
| GPG keyring (`GNUPGHOME`, when outside the workspace) | Full access | Optional |
| Daemon binary (`current_exe()`) | Read + execute | Optional |
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

`enforce()` logs the kernel's status and returns it, so each caller
picks its own strictness:

| Status | Daemon (`apply`) | Child (`confine`) |
|--------|------------------|-------------------|
| `FullyEnforced` | `info!` log, continue | run the command |
| `PartiallyEnforced` | `warn!` log, continue (kernel too old for full ABI) | exit 1, command does not run |
| `NotEnforced` | `warn!` log, continue (Landlock unsupported) | exit 1, command does not run |
| `Err` | logged as warning in `main.rs`, continue | exit 1, command does not run |

Daemon sandbox failure is never fatal — it is one layer of many and a
refusal to start would take the whole agent down. The child tier is the
opposite: it exists only when the operator asked for it, so silent
degradation is a bug, not resilience.

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
2. Egress filter (tinyproxy + nftables)
3. Unprivileged user (`kitaebot`)
4. systemd hardening (`ProtectSystem`, `NoNewPrivileges`, seccomp)
5. **Landlock filesystem confinement** (this spec): the daemon-wide
   ruleset, plus the tighter per-child tiers (`exec`, `git`) that deny
   the daemon-owned paths and the keyring to spawned code. Bubblewrap
   is a documented alternative for the exec tool, stronger in a few
   corners but requiring the trusted unit to loosen namespace and
   mount syscalls.
6. Exec deny-list (heuristic UX layer)
7. `PathGuard` (file tool workspace confinement)
8. Output leak detection

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Required path doesn't exist | `SandboxError::OpenPath`; daemon logs a warning and continues unsandboxed, `confine` exits 1 |
| Optional path doesn't exist | Rule silently skipped |
| Landlock unsupported | Daemon logs a warning and continues; `confine` exits 1 |
| Kernel too old for ABI V5 | `BestEffort` downgrades access flags in the daemon; `confine` treats the downgrade as fatal |

## Constraints

- Targets Landlock ABI V5 (Linux 6.7+)
- `BestEffort` compatibility on both ruleset and individual rules
- Enforcement is irrevocable — no runtime modification
- Secrets must be loaded before `apply()` is called

## Open Questions

- **Per-thread enforcement.** `restrict_self()` restricts the calling
  thread and its future children, not the whole process. `apply()` runs
  inside `#[tokio::main]`, so the worker pool already exists and only
  the thread that runs `apply()` gets the daemon ruleset. This does not
  weaken the per-child tiers (they re-enforce from scratch on whatever
  thread they land on), but the daemon's own confinement is not
  uniform. Fix: apply before the runtime starts, or restrict every
  worker. Tracked in [FUTURE.md](FUTURE.md).
- The daemon deliberately runs best-effort (a failed Required rule or an
  unsupported kernel logs and continues) rather than refusing to start;
  `confine` is the opposite and fail-closed. See Enforcement Status.
