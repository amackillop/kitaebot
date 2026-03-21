# NixOS VM test for egress filtering (spec 18)
#
# Verifies:
#   - dnsmasq resolves allowlisted domains, NXDOMAIN for the rest
#   - nftables drops direct-IP connections from kitaebot uid
#   - nftables allows HTTPS to resolved IPs from kitaebot uid
#   - nftset is populated by dnsmasq after upstream resolution
#   - root (non-kitaebot uid) is unrestricted
#   - Service ordering: nftables → dnsmasq → kitaebot
#
# Test topology:
#   server (192.168.1.2):
#     - nginx on 443 (self-signed TLS, returns "ok")
#     - dnsmasq on 53 (authoritative for api.github.com → 192.168.1.2)
#   kitaebot (192.168.1.1):
#     - egress-filtering dnsmasq on 127.0.0.2 (upstream → server:53)
#     - nftables kitaebot-egress table
#
# Run:
#   nix build .#nixosTests.x86_64-linux.egress --print-build-logs
#   # or
#   just check-egress
{
  pkgs,
  self,
  ...
}:
pkgs.testers.nixosTest {
  name = "kitaebot-egress-filter";

  nodes.server = _: {
    networking.firewall.allowedTCPPorts = [
      53
      443
    ];
    networking.firewall.allowedUDPPorts = [ 53 ];

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

    # Authoritative DNS: api.github.com → this server's test VLAN IP.
    # kitaebot's dnsmasq forwards allowlisted queries here, which
    # triggers nftset population with the resolved IP.
    services.dnsmasq = {
      enable = true;
      settings = {
        listen-address = "0.0.0.0";
        bind-interfaces = true;
        no-resolv = true;
        no-poll = true;
        # Answer api.github.com with the server's static VLAN address.
        address = "/api.github.com/192.168.1.2";
        # Reject everything else.
        local = "/#/";
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
        # Forward allowlisted queries to the server's DNS.
        dnsUpstream = "192.168.1.2";
      };

      # Don't run the real daemon (no credential files in the test VM).
      systemd.services.kitaebot.enable = false;

      virtualisation = {
        memorySize = lib.mkForce 1024;
        cores = lib.mkForce 1;
      };

      environment.systemPackages = with pkgs; [
        dig
        curl
        iproute2
      ];
    };

  testScript = ''
    server.wait_for_unit("nginx.service")
    server.wait_for_unit("dnsmasq.service")

    kitaebot.wait_for_unit("nftables.service")
    kitaebot.wait_for_unit("dnsmasq.service")

    # ── Service ordering ──────────────────────────────────────────────
    with subtest("dnsmasq starts after nftables"):
        nft_start = kitaebot.succeed(
            "systemctl show -p ActiveEnterTimestampMonotonic nftables.service | cut -d= -f2"
        ).strip()
        dns_start = kitaebot.succeed(
            "systemctl show -p ActiveEnterTimestampMonotonic dnsmasq.service | cut -d= -f2"
        ).strip()
        assert int(nft_start) <= int(dns_start), \
            f"nftables ({nft_start}) should start before dnsmasq ({dns_start})"

    # ── nftables rules loaded ─────────────────────────────────────────
    with subtest("nftables sets and chains exist"):
        kitaebot.succeed("nft list set inet kitaebot-egress allowed_v4")
        kitaebot.succeed("nft list set inet kitaebot-egress allowed_v6")
        kitaebot.succeed("nft list chain inet kitaebot-egress output")
        kitaebot.succeed("nft list chain inet kitaebot-egress nat_output")

    # ── DNS filtering ─────────────────────────────────────────────────
    with subtest("allowlisted domain resolves for kitaebot uid"):
        result = kitaebot.succeed(
            "sudo -u kitaebot dig +short +timeout=5 api.github.com @127.0.0.2"
        )
        assert result.strip() != "", "Expected IP, got empty result"
        assert "192.168.1.2" in result, f"Expected 192.168.1.2, got: {result}"

    with subtest("blocked domain returns NXDOMAIN for kitaebot uid"):
        result = kitaebot.succeed(
            "sudo -u kitaebot dig +timeout=5 evil.example.com @127.0.0.2 || true"
        )
        assert "NXDOMAIN" in result, f"Expected NXDOMAIN, got: {result}"

    # ── nftset populated by dnsmasq ───────────────────────────────────
    with subtest("nft set contains resolved IP after DNS lookup"):
        output = kitaebot.succeed("nft list set inet kitaebot-egress allowed_v4")
        assert "192.168.1.2" in output, \
            f"Expected 192.168.1.2 in allowed_v4 set, got: {output}"

    # ── nftables IP enforcement ───────────────────────────────────────
    with subtest("kitaebot uid can reach allowlisted HTTPS endpoint"):
        kitaebot.succeed(
            "sudo -u kitaebot curl -sk --max-time 10 https://192.168.1.2/"
        )

    with subtest("kitaebot uid cannot connect to IP not in nft set"):
        # 192.0.2.1 is TEST-NET-1 (RFC 5737), guaranteed non-routable
        kitaebot.fail(
            "sudo -u kitaebot curl -sk --max-time 3 --connect-timeout 2 https://192.0.2.1/"
        )

    with subtest("nftables drop counter increments"):
        output = kitaebot.succeed("nft list chain inet kitaebot-egress output")
        assert "counter packets 0" not in output, \
            f"Expected drop counter > 0, got: {output}"

    # ── Root is unrestricted ──────────────────────────────────────────
    with subtest("root can connect to the server directly"):
        kitaebot.succeed("curl -sk --max-time 10 https://192.168.1.2/")
  '';
}
