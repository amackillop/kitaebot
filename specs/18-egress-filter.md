# Spec 18: Egress Filter

## Motivation

Restrict outbound network access from the `kitaebot` uid to a domain
allowlist. Prevents prompt-injection-driven exfiltration — a compromised agent
cannot reach attacker-controlled infrastructure.

## Behavior

### Architecture

Two enforcement layers, each sufficient independently:

**Layer 1 — forward proxy (tinyproxy).** Listens on `127.0.0.1:8888`. The
kitaebot service carries `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` (and
lowercase) env vars; the subprocess env scrubber (`SAFE_ENV_VARS` in
`src/tools/mod.rs`) forwards them to every exec-tool and CLI child.
`NO_PROXY=localhost,127.0.0.1` keeps loopback traffic (local dev servers)
out of the proxy filter. HTTPS clients send the target hostname in
the CONNECT request, so the proxy filters by *name* — no DNS interception, no
IP sets, no staleness. CONNECT is allowed only to allowlisted domains on port
443; everything else is refused with HTTP 403 and logged.

**Layer 2 — uid lockdown (nftables).** Output chain matches `meta skuid 900`
(static UID) and permits loopback only. Any attempt to bypass the proxy —
direct TCP, DNS (a tunneling vector), anything — is rejected and logged.

Together: the proxy decides what is reachable, nftables guarantees the proxy
is the only path out.

### Default Allowlist

| Domain | Purpose |
|--------|---------|
| `openrouter.ai` | LLM provider API |
| `api.telegram.org` | Telegram bot channel |
| `github.com` | Git clone/push, GitHub web |
| `api.github.com` | GitHub REST/GraphQL API |
| `githubusercontent.com` | GitHub raw content, git objects |
| `flakehub.com` | FlakeHub Nix registry |
| `api.perplexity.ai` | Web search tool |
| `api.linear.app` | Linear API |
| `bitcoinknowledge.dev` | bkb MCP server backend (spec 22) |
| `npmjs.org` | npm/pnpm registry (devShell installs) |
| `yarnpkg.com` | yarn registry (devShell installs) |
| `pnpm.io` | pnpm docs — the dependency-maintenance duty reads override/config references |
| `crates.io` | cargo registry (devShell installs) |
| `pypi.org` | pip index (devShell installs) |
| `pythonhosted.org` | pip package files (devShell installs) |
| `rubygems.org` | gem registry (devShell installs) |
| `doc.rust-lang.org` | rustdoc (devShell toolchain docs) |

Plus one anchored ERE in `egressAllowlistPatterns` (GitHub Actions
log/artifact downloads, see the option's comment). Each domain matches
itself and all subdomains.

### tinyproxy Configuration

Generated from `egressAllowlist`:

- `Listen 127.0.0.1`, `Port 8888`, `Allow 127.0.0.1` — loopback only
- `ConnectPort 443` — HTTPS tunnels only, no other CONNECT ports
- `Filter` file with one anchored extended regex per domain:
  `^(.*\.)?domain\.tld$`. Anchoring is load-bearing: an unanchored
  `api\.github\.com` would also match `api.github.com.evil.net`
- `FilterType ere`, `FilterDefaultDeny yes` — deny anything unmatched
- `LogLevel Notice` — refused CONNECTs are logged
  ("Proxying refused on filtered domain") without per-request noise

tinyproxy runs in the foreground under systemd, so its log lands in the
journal (`journalctl -u tinyproxy`, or `just vm-logs-proxy` from the host).

### nftables Table

```nft
table inet kitaebot-egress {
  chain output {
    type filter hook output priority 0; policy accept;
    meta skuid != 900 accept              # only restrict kitaebot uid
    oifname "lo" accept                   # Unix sockets, forward proxy
    ct state established,related accept   # socketless kernel packets
    log prefix "kitaebot-egress-reject: " counter reject  # everything else
  }
}
```

The `ct state` rule exists because `meta skuid` is undefined for packets
with no owning socket (kernel-generated RSTs for closed sockets, TIME_WAIT
ACKs): the `skuid != 900` match cannot exclude them, so without it they
fall through to the reject rule and spam the log. It opens nothing — the
kitaebot uid can never establish a non-loopback flow, since the initial
SYN is rejected.

Rejected packets appear in the kernel log (`journalctl -k`) with the
`kitaebot-egress-reject:` prefix, so surprising failures are always
visible. Reject rather than drop: a silent drop turns a misdirected
client into a connect-timeout hang (an ssh fetch under nix evaluation
once rode one to direnv's 900s ceiling, twice in a turn); reject fails
it in milliseconds with a diagnosable error. This filter polices our
own process, so there is nothing to be stealthy about.

### IPv6

`networking.enableIPv6 = false` guest-wide. QEMU user-mode networking does
not forward IPv6, and with no v6 route the proxy's `connect()` to an AAAA
address fails instantly instead of stalling before the v4 fallback.

### Service Ordering

```
tinyproxy.service → kitaebot.service
```

The proxy must accept connections before the daemon starts.

### Module Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `egressAllowlist` | list of str | (the default allowlist above) | Domains the kitaebot process may connect to |

### Side Effects

- **`web_fetch` tool**: restricted to allowlisted domains only
- **DNS**: the kitaebot uid has no DNS at all. Proxy-aware clients don't
  need it (the hostname travels in CONNECT); anything that resolves first
  fails fast and the reject is logged
- **Nix operations**: nix-daemon (root) is unaffected. Client-side fetches
  (flake inputs) run as the kitaebot uid and honor `https_proxy`
- **Git**: HTTPS clone/push works via the proxy. SSH git (port 22) is not
  possible — the proxy only tunnels to port 443
- **Plain HTTP**: proxied GETs to allowlisted domains work; everything else
  is refused
- **Other system services**: unaffected (only uid 900 is filtered)

## Boundaries

### Owns

- tinyproxy configuration and the generated filter file
- nftables table definition
- Domain allowlist
- Proxy env vars on the kitaebot service
- Service ordering constraints

### Does Not Own

- The kitaebot binary — it is unaware of egress filtering beyond honoring
  standard proxy env vars via its HTTP client
- systemd hardening — complements `RestrictAddressFamilies` but is independent
- VM-level networking — orthogonal to guest-level filtering

## Failure Modes

| Failure | Behavior |
|---------|----------|
| nftables fails to load | Firewall inactive; proxy still filters by domain |
| tinyproxy fails to start | kitaebot starts but every request fails (connection refused to 127.0.0.1:8888); nftables still blocks direct egress |
| Blocked domain requested | HTTP 403 from proxy, refusal logged in tinyproxy journal |
| Direct egress attempted | Packet rejected, logged in kernel journal |
| Allowlisted domain unreachable | Normal upstream connection failure, surfaced through the proxy |

## Constraints

- Static UID 900 required (nftables matches by numeric UID, no NSS lookup)
- `FilterType` requires tinyproxy >= 1.11
- All allowlisted traffic must use HTTPS (port 443) — no other ports allowed
- Clients must honor proxy env vars; anything that doesn't is blocked by
  nftables (fail closed, logged)

## Verification

Automated NixOS VM test in `vm/test-egress.nix` (run via
`just test-nixos-one egress`). Two QEMU VMs on a shared VLAN:

- **server** — nginx on 443 (self-signed TLS)
- **kitaebot** — full egress filter stack, test domains mapped to the
  server via `/etc/hosts`

Test coverage:

| Subtest | Validates |
|---------|-----------|
| Allowlisted CONNECT succeeds | Proxy tunnels to allowlisted domain |
| Blocked domain refused | `FilterDefaultDeny` rejects unlisted hosts |
| Spoofed suffix refused | Anchored regex rejects `api.github.com.evil.test` |
| Non-443 CONNECT refused | `ConnectPort` restriction holds |
| Refusals logged | tinyproxy journal contains the filter refusal |
| Direct egress dropped | nftables blocks proxy bypass from uid 900 |
| Drop counter + kernel log | Drops are counted and logged |
| Root unrestricted | Non-kitaebot uid bypasses all rules |

## Known Limitations

1. **No TLS inspection** — the proxy sees only the CONNECT hostname. A
   compromised agent can exfiltrate *to allowlisted domains* (e.g. a GitHub
   repo it can write to). Rate limiting is a possible future mitigation
   (see FUTURE.md)
2. **Hostname is client-asserted** — the proxy connects to whatever the
   allowlisted name resolves to; it does not verify the TLS SNI matches.
   Irrelevant here since DNS resolution happens in the proxy, outside the
   kitaebot uid's control

## Open Questions

None currently.
