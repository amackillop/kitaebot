# Credential Isolation

## Purpose

Secrets never enter the process environment. The agent cannot leak credentials via `echo $VAR` or `env` because the credentials are not environment variables — they exist only as files read once at startup, held in memory within the provider struct.

## Architecture

```
systemd LoadCredential=
    ↓
/run/credentials/kitaebot-heartbeat.service/openrouter-api-key  (0400)
    ↓
load_secret("openrouter-api-key")  →  String in memory
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

No single layer is sufficient. All six together make credential exfiltration require escaping the VM.

## Required Credentials

| File | Purpose |
|------|---------|
| `openrouter-api-key` | LLM provider authentication |
| `telegram-bot-token` | Telegram Bot API token |
| `telegram-chat-id` | Authorized Telegram chat ID (future) |

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
