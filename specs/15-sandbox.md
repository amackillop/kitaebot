# Sandbox

## Purpose

Kernel-enforced filesystem confinement via Linux Landlock LSM. Applied at process startup, irrevocable, inherited by all child processes (including `sh -c` from the exec tool).

## Why Landlock?

1. **Self-applied, unprivileged** — No root, no external dependencies, no container runtime
2. **Complements systemd** — Covers paths that bypass systemd sandboxing
3. **Graceful degradation** — Falls back to warning on unsupported kernels
4. **One-shot** — Applied at startup, cannot be removed or weakened

## Filesystem Policy

| Path                    | Access                                   |
|-------------------------|------------------------------------------|
| Workspace               | Full read-write                          |
| `/nix/store`            | Read + execute                           |
| `/tmp`                  | Working access (no device creation)            |
| `/etc`                  | Read-only (resolv.conf, CA certs)        |
| `/run`                  | Read-only (systemd runtime, resolv.conf) |
| Socket directory†       | Socket (bind, read, write, unlink)       |
| `/dev`                  | Read + write (/dev/null, /dev/urandom)   |
| `/proc`                 | Read-only (/proc/self/\*, /proc/meminfo) |
| Everything else         | Denied                                   |

† Derived from the configured `socket.path` parent. Not hardcoded.

`CREDENTIALS_DIRECTORY` is intentionally **excluded**. All secrets are loaded into memory before sandbox enforcement; credential files become inaccessible after.

NixOS note: `/usr` and `/bin` don't exist. All binaries live in `/nix/store`. `/etc` is a symlink farm into `/nix/store`.

## Graceful Degradation

If Landlock is unavailable (old kernel, non-Linux), log a warning and continue. Defense-in-depth, not a hard gate.

## Defense-in-Depth Stack

1. VM isolation (QEMU)
2. Unprivileged user (`kitaebot`)
3. systemd hardening (`ProtectSystem`, `NoNewPrivileges`, seccomp)
4. **Landlock filesystem confinement** ← this spec
5. Exec deny-list (heuristic UX layer)
6. `PathGuard` (file tool workspace confinement)
7. Output leak detection

## Future Work

- bubblewrap per-command isolation for exec tool (namespace isolation per shell invocation)
- cgroup resource limits (`TasksMax`, `MemoryMax` on systemd unit)
- seccomp-bpf tightening in-process (covers `kchat` path)
- Landlock network restrictions (kernel 6.7+, not yet stable in the crate)
