# Spec 13: Credential Isolation

## Motivation

Secrets never enter the process environment. The agent cannot leak credentials
via `echo $VAR` or `env` because they are not environment variables — they
exist only as files read once at startup, held in memory behind an opaque
wrapper.

## Behavior

### Loading

`load_secret(name)` reads `$CREDENTIALS_DIRECTORY/<name>`, trims whitespace,
and returns a `Secret` newtype. `Secret` implements `Debug` and `Display` as
`[REDACTED]`; access only via `.expose()`.

Called once at startup during `runtime::build()`, **before** Landlock
enforcement. After sandboxing, credential files become kernel-denied.

### systemd Provisioning

`LoadCredential=` copies secret files from `secretsDir` to a tmpfs at
`/run/credentials/kitaebot.service/`, mode 0400, owned by the service user.
systemd sets `CREDENTIALS_DIRECTORY` automatically. The Rust code reads the
env var — it never hardcodes the path.

### Required Credentials

| File | Purpose | Conditional |
|------|---------|-------------|
| `provider-api-key` | LLM provider authentication | Always (fatal if missing) |
| `telegram-bot-token` | Telegram Bot API token | Always in `LoadCredential`; only read when `telegram.enabled` |
| `github-token` | GitHub PAT for clone/push/PRs | `git.enabled` or `github.enabled` |
| `gpg-signing-key` | GPG private key for commit signing | `gitConfig.signingKey` set |

Missing required secrets at startup cause a fatal exit (no fallback to env
vars).

### GPG Key Exception

Unlike other secrets which remain in `CREDENTIALS_DIRECTORY` (inaccessible
after sandboxing), the GPG key is imported into `/var/lib/kitaebot/.gnupg` at
service start via `ExecStartPre` so git can sign commits. This keyring is
readable by exec tool commands. Mitigations:

- `gpg` is not on the exec tool's PATH (git uses an absolute Nix store path)
- Deny rules block `gpg --export-secret`, `.gnupg/` access, and
  `commit.gpgsign=false` overrides

### Defense-in-Depth Stack

| Layer | Defense |
|-------|---------|
| 0 | NixOS VM boundary ([spec 09](09-vm.md)) |
| 1 | Secrets as files, not env vars (this spec) |
| 2 | systemd service hardening ([spec 09](09-vm.md)) |
| 3 | Landlock filesystem confinement ([spec 15](15-sandbox.md)) |
| 4 | Process-level env scrubbing ([spec 03](03-tools.md)) |
| 5 | Output scanning ([spec 11](11-safety.md)) |
| 6 | Exec deny rules — secret harvesting, GPG keyring, signing override ([spec 03](03-tools.md)) |

No single layer is sufficient. Together they make credential exfiltration
require escaping the VM.

## Boundaries

### Owns

- `load_secret()` function and `Secret` newtype
- `SecretError` error type
- The contract that secrets are read once at startup and never exported

### Does Not Own

- systemd `LoadCredential` configuration — the NixOS module handles that
- Landlock enforcement — the sandbox module handles that
- Environment scrubbing — the exec tool handles that
- Output scanning — the safety module handles that

## Failure Modes

| Failure | Error | Behavior |
|---------|-------|----------|
| `CREDENTIALS_DIRECTORY` not set | `SecretError::NoCredentialsDir` | Fatal exit |
| Secret file missing | `SecretError::NotFound` | Fatal exit |
| Secret file unreadable | `SecretError::Read` | Fatal exit |

No fallback to environment variables. No retry.

## Constraints

- Secrets are loaded before Landlock enforcement — ordering is critical
- `CREDENTIALS_DIRECTORY` is excluded from `SAFE_ENV_VARS` — child processes
  cannot discover the credential path
- `Secret` wrapper prevents accidental logging

## Open Questions

- `telegram-bot-token` is always required by `LoadCredential` even when
  Telegram is disabled. Should the NixOS module gate it on
  `settings.telegram.enabled`?
