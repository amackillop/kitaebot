# NixOS VM test for egress filtering (spec 18)
#
# Verifies:
#   - tinyproxy allows CONNECT to allowlisted domains
#   - tinyproxy refuses (and logs) CONNECT to anything else,
#     including allowlisted names used as a spoofed suffix
#   - nftables rejects (and logs) direct egress from the kitaebot uid
#   - root (non-kitaebot uid) is unrestricted
#
# Test topology:
#   server (192.168.1.2):
#     - nginx on 443 (self-signed TLS, returns "ok")
#   kitaebot (192.168.1.1):
#     - tinyproxy CONNECT allowlist on 127.0.0.1:8888
#     - nftables kitaebot-egress table
#     - /etc/hosts maps the test domains to the server, so the proxy
#       can resolve them without external DNS
#
# Run:
#   nix build .#nixosTests.x86_64-linux.egress --print-build-logs
#   # or
#   just test-nixos-one egress
{
  pkgs,
  self,
  ...
}:
pkgs.testers.nixosTest {
  name = "kitaebot-egress-filter";

  nodes.server = _: {
    networking.firewall.allowedTCPPorts = [ 443 ];

    # HTTPS endpoint for connectivity testing.
    services.nginx = {
      enable = true;
      virtualHosts."server" = {
        addSSL = true;
        sslCertificate = ./test-fixtures/server.crt;
        sslCertificateKey = ./test-fixtures/server.key;
        locations."/".return = "200 'ok'";
      };
    };
  };

  nodes.kitaebot =
    { pkgs, lib, ... }:
    {
      imports = [ self.nixosModules.vm ];

      kitaebot = {
        package = pkgs.writeShellScriptBin "kitaebot" ''
          echo "stub"
          sleep infinity
        '';
        secretsDir = "/tmp/fake-secrets";
        sshKeys = [ ];
      };

      # Don't run the real daemon (no credential files in the test VM).
      systemd.services.kitaebot.enable = false;

      # Both names resolve to the server: the allowlisted domain must
      # get through the proxy, the spoofed suffix must be refused by
      # name even though it resolves fine.
      networking.hosts."192.168.1.2" = [
        "api.github.com"
        "api.github.com.evil.test"
      ];

      virtualisation = {
        memorySize = lib.mkForce 1024;
        cores = lib.mkForce 1;
      };

      environment.systemPackages = with pkgs; [
        curl
        iproute2
      ];
    };

  testScript = ''
    server.wait_for_unit("nginx.service")

    kitaebot.wait_for_unit("nftables.service")
    kitaebot.wait_for_unit("tinyproxy.service")
    kitaebot.wait_for_open_port(8888)

    proxy = "--proxy http://127.0.0.1:8888"

    # ── Proxy allowlist ───────────────────────────────────────────────
    with subtest("kitaebot uid reaches allowlisted domain via proxy"):
        out = kitaebot.succeed(
            f"sudo -u kitaebot curl -sk {proxy} --max-time 10 https://api.github.com/"
        )
        assert "ok" in out, f"Expected 'ok' from nginx, got: {out}"

    with subtest("proxy refuses non-allowlisted domain"):
        kitaebot.fail(
            f"sudo -u kitaebot curl -sk {proxy} --max-time 10 https://evil.example.com/"
        )

    with subtest("proxy refuses allowlisted name as spoofed suffix"):
        # api.github.com.evil.test resolves (to the server, even), but
        # the anchored filter regex must not match it.
        kitaebot.fail(
            f"sudo -u kitaebot curl -sk {proxy} --max-time 10 https://api.github.com.evil.test/"
        )

    with subtest("proxy refuses CONNECT to non-443 ports"):
        kitaebot.fail(
            f"sudo -u kitaebot curl -sk {proxy} --max-time 10 https://api.github.com:8443/"
        )

    with subtest("proxy logs the refusals"):
        kitaebot.succeed(
            "journalctl -u tinyproxy --no-pager | grep -i 'filtered domain'"
        )

    # ── nftables direct-egress lockdown ───────────────────────────────
    with subtest("kitaebot uid cannot bypass the proxy"):
        kitaebot.fail(
            "sudo -u kitaebot curl -sk --max-time 3 --connect-timeout 2 https://192.168.1.2/"
        )

    with subtest("nftables reject counter increments and rejects are logged"):
        out = kitaebot.succeed("nft list chain inet kitaebot-egress output")
        assert "counter packets 0" not in out, f"Expected reject counter > 0, got: {out}"
        kitaebot.succeed("journalctl -k --no-pager | grep 'kitaebot-egress-reject'")

    with subtest("blocked direct egress fails fast, not by timeout"):
        # Reject (not drop) is the contract: a silent drop once turned
        # an ssh fetch under nix evaluation into 900s direnv hangs.
        kitaebot.succeed(
            "start=$(date +%s); "
            "sudo -u kitaebot curl -sk --connect-timeout 30 https://192.168.1.2/ || true; "
            "elapsed=$(( $(date +%s) - start )); test $elapsed -lt 5"
        )

    # ── Root is unrestricted ──────────────────────────────────────────
    with subtest("root can connect to the server directly"):
        kitaebot.succeed("curl -sk --max-time 10 https://192.168.1.2/")
  '';
}
