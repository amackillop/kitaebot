# Credential Isolation

## Purpose

Secrets never enter the process environment. The agent cannot leak credentials via `echo $VAR` or `env` because the credentials are not environment variables — they exist only as files read once at startup, held in memory within the provider struct.

## Architecture

```
systemd LoadCredential=
    ↓
/run/credentials/kitaebot.service/provider-api-key  (0400)
    ↓
load_secret("provider-api-key")  →  Secret (opaque wrapper)
    ↓
OpenRouterProvider::new(api_key, ...)  →  held in struct, never exported
```

systemd copies credential files to a tmpfs under `/run/credentials/<unit>/`, owned by the service user, mode 0400. The `CREDENTIALS_DIRECTORY` env var points to this path. After the process reads the file, the secret lives only in the provider struct.

## `load_secret()` Interface

```rust
fn load_secret(name: &str) -> Result<String, SecretError>
```

Reads `$CREDENTIALS_DIRECTORY/<name>`, trims whitespace, returns the content. Only called at startup — not on every request.

## Error Types

```rust
enum SecretError {
    NoCredentialsDir,           // CREDENTIALS_DIRECTORY not set
    NotFound { name: String },  // File doesn't exist
    Read { name, source },      // I/O error reading file
}
```

Startup fails hard on any variant. No fallback to env vars — that would defeat the purpose.

## Defense-in-Depth Layers

| Layer | Defense | Status |
|-------|---------|--------|
| 0 | NixOS VM boundary | Done (spec 09) |
| 1 | Secrets as files, not env vars | Done (this spec) |
| 2 | systemd service hardening | Done (spec 09) |
| 3 | Landlock filesystem confinement | Done (spec 15) |
| 4 | Process-level env scrubbing | Done (spec 03) |
| 5 | Output scanning | Done (spec 11) |
| 6 | Exec deny rules (secret harvesting, GPG keyring, signing override) | Done (spec 03) |

No single layer is sufficient. Together they make credential exfiltration require escaping the VM.

**GPG key exception**: The GPG signing key is imported into the service user's keyring (`/var/lib/kitaebot/.gnupg`) so git can sign commits. Unlike other secrets, it's accessible to child processes. Mitigation: `gpg` is not on the exec tool PATH (git uses an absolute Nix store path), and deny rules block export/read attempts. See STATUS.md for the planned gpg-agent isolation improvement.

## Required Credentials

| File | Purpose | Conditional |
|------|---------|-------------|
| `provider-api-key` | LLM provider authentication | Always |
| `telegram-bot-token` | Telegram Bot API token | Always |
| `github-token` | GitHub PAT for clone/push/PRs | `git.enabled` or `github.enabled` |
| `gpg-signing-key` | GPG private key for commit signing | `gitConfig.signingKey` set |

**Note on GPG key isolation**: Unlike other secrets which remain in `CREDENTIALS_DIRECTORY` (inaccessible to child processes), the GPG key is imported into `/var/lib/kitaebot/.gnupg` at service start so that git can sign commits. This keyring is readable by exec tool commands. Heuristic deny rules block `gpg --export-secret`, `.gnupg/` access, and `commit.gpgsign=false` overrides. See STATUS.md for the planned gpg-agent isolation improvement.

## Dev Workflow

For local `cargo run` without systemd, point `CREDENTIALS_DIRECTORY` at a directory containing one file per secret:

```bash
echo 'sk-or-...' > secrets/openrouter-api-key
CREDENTIALS_DIRECTORY=./secrets cargo run -- chat
```

The `secrets/` directory is gitignored (only `.gitkeep` is tracked).

## Future Considerations

- **sops-nix** — Encrypt credential files at rest in the repo, decrypt at deploy time via sops-nix. Currently files are plaintext on the host.
- **Rotation** — Credential files can be updated and the service restarted. No code changes needed.
- **Shared hardening** — When `kitaebot run` daemon is added, factor `LoadCredential` and hardening into a shared attr set to avoid duplication between service units.
- **GPG agent isolation** — The GPG private key currently lives in `/var/lib/kitaebot/.gnupg`, readable by any exec tool command. Heuristic deny rules block obvious exfiltration (`gpg --export-secret`, `cat .gnupg/`), but these are trivially bypassable. Proper isolation: run `gpg-agent` under a separate user, expose only the signing socket to the kitaebot service via `GPG_AGENT_INFO` or `--use-agent`. The agent can request signatures but never read key material.
