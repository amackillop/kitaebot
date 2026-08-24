# NixOS VM test for the daemon's nix toolchain environment
#
# Verifies, as the kitaebot uid:
#   - the experimental features `nix-command` and `flakes` are enabled
#     (every devshell is `use flake` and every repo gate is
#     `nix flake check`; a nix.conf regression breaks them all at once)
#   - a flake actually evaluates end to end, offline
#   - direnv, the devshell loader, is on PATH
#
# Run:
#   nix build .#nixosTests.x86_64-linux.flakes --print-build-logs
#   # or
#   just test-nixos-one flakes
{
  pkgs,
  self,
  ...
}:
pkgs.testers.nixosTest {
  name = "kitaebot-flakes";

  nodes.kitaebot = _: {
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
  };

  testScript = ''
    kitaebot.wait_for_unit("multi-user.target")

    run = "sudo -Hu kitaebot env HOME=/var/lib/kitaebot "

    with subtest("experimental features are enabled for the daemon uid"):
        out = kitaebot.succeed(run + "nix config show experimental-features")
        for feature in ["nix-command", "flakes"]:
            assert feature in out, f"{feature} missing from: {out}"

    with subtest("a flake evaluates end to end, offline"):
        kitaebot.succeed("install -d -o kitaebot -g kitaebot /tmp/probe")
        kitaebot.succeed(
            run
            + """sh -c 'printf "{ outputs = _: { ok = \\"ok\\"; }; }" > /tmp/probe/flake.nix'"""
        )
        out = kitaebot.succeed(run + "nix eval --raw path:/tmp/probe#ok")
        assert out == "ok", f"flake eval returned: {out}"

    with subtest("direnv is on PATH for the daemon uid"):
        kitaebot.succeed(run + "direnv version")
  '';
}
