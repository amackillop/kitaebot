# Spec 18: Egress Filter

## Motivation

Restrict outbound network access from the `kitaebot` uid to a DNS-based domain
allowlist. Prevents prompt-injection-driven exfiltration — a compromised agent
cannot reach attacker-controlled infrastructure.

## Behavior

### Architecture

Two enforcement layers, each sufficient independently:

**Layer 1 — DNS filtering (dnsmasq).** Local DNS proxy on `127.0.0.2` resolves
only allowlisted domains. All DNS queries from the kitaebot uid are redirected
via nftables DNAT. Unlisted domains return NXDOMAIN.

**Layer 2 — IP enforcement (nftables).** Output chain matches `meta skuid 900`
(static UID). Only allows TCP 443 to IPs that dnsmasq resolved and injected
into nftables sets via the `nftset` directive. Direct-IP connections are
dropped.

Together: DNS prevents resolution, nftables prevents direct-IP bypass.

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

dnsmasq's `server=/domain/` matches the domain and all subdomains.

### dnsmasq Configuration

Generated from `egressAllowlist` and `dnsUpstream`:

- `listen-address=127.0.0.2`, `bind-dynamic` (allows late binding after
  nftables)
- `no-resolv`, `no-poll` (no external resolv.conf)
- `local=/#/` (NXDOMAIN for all non-forwarded domains)
- `server=/<domain>/<upstream>` per allowlisted domain
- `nftset=/<domain>/4#inet#kitaebot-egress#allowed_v4,6#inet#kitaebot-egress#allowed_v6`
  per domain (injects resolved IPs into nft sets)
- `log-queries=true` (operational visibility)
- `resolveLocalQueries = false` (prevents dnsmasq from becoming the system
  resolver — root and nix-daemon must not be filtered)

### nftables Table

```nft
table inet kitaebot-egress {
  set allowed_v4 { type ipv4_addr; flags timeout; timeout 1h }
  set allowed_v6 { type ipv6_addr; flags timeout; timeout 1h }

  chain output {
    type filter hook output priority 0; policy accept;
    meta skuid != 900 accept              # only restrict kitaebot uid
    oifname "lo" accept                   # loopback always allowed
    ct state established,related accept   # response traffic
    meta l4proto { tcp, udp } th dport 53 ip daddr 127.0.0.2 accept  # DNS to proxy
    tcp dport 443 ip daddr @allowed_v4 accept   # HTTPS to resolved IPs
    tcp dport 443 ip6 daddr @allowed_v6 accept
    log prefix "kitaebot-egress-drop: " counter drop  # everything else
  }

  chain nat_output {
    type nat hook output priority -100; policy accept;
    meta skuid 900 meta l4proto { tcp, udp } th dport 53 dnat ip to 127.0.0.2
  }
}
```

The nat chain (priority -100) rewrites DNS before the filter chain (priority 0)
evaluates the packet.

### Service Ordering

```
nftables.service → dnsmasq.service → kitaebot.service
```

nft sets must exist before dnsmasq writes to them. DNS must be available before
kitaebot connects.

### Module Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `egressAllowlist` | list of str | (7 domains above) | Domains the kitaebot process may connect to |
| `dnsUpstream` | str | `"9.9.9.9"` | Upstream DNS resolver (Quad9) |

### Side Effects

- **`web_fetch` tool**: restricted to allowlisted domains only
- **Nix operations**: unaffected (nix daemon runs as root, not kitaebot uid).
  If kitaebot invokes nix commands that do HTTP in-process, those would be
  blocked. Add `cache.nixos.org` to `egressAllowlist` if needed.
- **Git**: `github.com` and `api.github.com` are allowlisted. HTTPS
  clone/push works. SSH git (port 22) is not allowed.
- **Other system services**: unaffected (only uid 900 is filtered)

## Boundaries

### Owns

- nftables table definition (sets, chains, rules)
- dnsmasq configuration for DNS-based filtering
- Domain allowlist and upstream DNS config
- Service ordering constraints

### Does Not Own

- The kitaebot binary — it is unaware of egress filtering
- systemd hardening — complements `RestrictAddressFamilies` but is independent
- VM-level networking — orthogonal to guest-level filtering

## Failure Modes

| Failure | Behavior |
|---------|----------|
| nftables fails to load | dnsmasq and kitaebot don't start (ordering dependency) |
| dnsmasq fails to start | kitaebot doesn't start (ordering dependency) |
| Allowlisted domain unreachable | Normal DNS/connection failure |
| nft set entry expires (1h TTL) | Next DNS lookup re-populates the set |

## Constraints

- Static UID 900 required (nftables matches by numeric UID, no NSS lookup)
- `nftset` directive requires dnsmasq >= 2.87
- All allowlisted traffic must use HTTPS (port 443) — no other ports allowed
- nft set entries expire after 1 hour

## Verification

Automated NixOS VM test in `vm/test-egress.nix` (run via
`just test-nixos-one egress`). Two QEMU VMs on a shared VLAN:

- **server** — nginx on 443 (self-signed TLS) + dnsmasq on 53 (authoritative
  for test domain)
- **kitaebot** — full egress filter stack

Test coverage:

| Subtest | Validates |
|---------|-----------|
| Service ordering | nftables starts before dnsmasq |
| Sets and chains exist | nft table structure loaded |
| Allowlisted domain resolves | dnsmasq forwards, returns IP |
| Blocked domain NXDOMAIN | `local=/#/` returns NXDOMAIN |
| nft set populated | `nftset` injects resolved IP |
| Allowlisted HTTPS reachable | curl to resolved IP succeeds |
| Blocked IP dropped | curl to non-allowlisted IP fails |
| Drop counter increments | Drop rule is hit |
| Root unrestricted | Non-kitaebot uid bypasses all rules |

## Known Limitations

1. **Shared IP ranges** — if an allowlisted domain shares a CDN IP with a
   malicious service, that service becomes reachable on port 443
2. **No per-domain port granularity** — all allowlisted IPs share port 443
3. **DNS-over-HTTPS bypass** — mitigated by nftables IP enforcement (DoH
   resolver IP won't be in the allowed set)

## Open Questions

None currently.
