# NixOS VM test for workspace backup/restore (spec 09)
#
# Verifies:
#   - backup captures durable state (databases, payloads, memory, history)
#     and leaves derived state out
#   - restore replaces state/ rather than merging into it, so a stale
#     payload cannot survive
#   - restored files are owned by the daemon user
#   - ownership is derived from the archive, not a fixed list: an archive
#     predating HISTORY.md's move into state/ carries it at the root, and
#     it must still come back writable
#
# Not covered: that VACUUM INTO captures WAL content a plain copy would
# miss. Forcing uncheckpointed data to survive the writer exiting is not
# reliably reproducible here; it was measured by hand instead — a copy of
# a live lcm.db gave 1999 rows against 2040.
#
# Run:
#   just test-nixos-one backup
{
  pkgs,
  self,
  ...
}:
pkgs.testers.nixosTest {
  name = "kitaebot-backup-restore";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [ self.nixosModules.vm ];

      kitaebot = {
        # The scripts drive `systemctl stop/start kitaebot`, so the unit
        # has to exist and be startable; what it runs is irrelevant.
        # Absolute path: the unit's hardening leaves no PATH, so a bare
        # `sleep` exits 127 and systemd restarts it forever.
        package = pkgs.writeShellScriptBin "kitaebot" "exec ${pkgs.coreutils}/bin/sleep infinity";
        secretsDir = pkgs.runCommand "fake-secrets" { } ''
          mkdir -p $out
          echo fake > $out/provider-api-key
          echo fake > $out/telegram-bot-token
        '';
        sshKeys = [ ];
      };

      virtualisation = {
        memorySize = lib.mkForce 1024;
        cores = lib.mkForce 1;
      };

      environment.systemPackages = [ pkgs.sqlite ];
    };

  testScript = ''
    W = "/var/lib/kitaebot"

    machine.wait_for_unit("kitaebot.service")

    with subtest("seed durable and derived state"):
        machine.succeed(f"mkdir -p {W}/state/lcm/payloads {W}/memory/topics {W}/projects/o/r")
        machine.succeed(
            f"sqlite3 {W}/state/usage.db "
            "'PRAGMA journal_mode=WAL; CREATE TABLE turns(x); "
            "INSERT INTO turns VALUES (1),(2),(3);'"
        )
        machine.succeed(f"echo keep > {W}/state/lcm/payloads/file_keep")
        machine.succeed(f"echo '{{}}' > {W}/state/github_poll_state.json")
        machine.succeed(f"echo index > {W}/memory/MEMORY.md")
        machine.succeed(f"echo '[t] an entry' > {W}/state/HISTORY.md")
        machine.succeed(f"echo derived > {W}/projects/o/r/file")
        machine.succeed(f"chown -R kitaebot:kitaebot {W}/state {W}/memory {W}/projects")

    with subtest("backup takes durable state and leaves derived out"):
        machine.succeed("bash ${./backup.sh} > /tmp/backup.tar.gz")
        listing = machine.succeed("tar tzf /tmp/backup.tar.gz")
        for want in [
            "./state/usage.db",
            "./state/lcm/payloads/file_keep",
            "./state/github_poll_state.json",
            "./state/HISTORY.md",
            "./memory/MEMORY.md",
        ]:
            assert want in listing, f"backup omits {want}:\n{listing}"
        assert "projects" not in listing, f"backup swept in derived state:\n{listing}"

    with subtest("restore replaces state rather than merging into it"):
        machine.succeed(f"echo stale > {W}/state/lcm/payloads/file_stale")
        machine.succeed(f"rm {W}/memory/MEMORY.md")
        machine.succeed("cp /tmp/backup.tar.gz /tmp/kitaebot-restore.tar.gz")
        machine.succeed("bash ${./restore.sh}")

        machine.succeed(f"test -e {W}/state/lcm/payloads/file_keep")
        machine.fail(f"test -e {W}/state/lcm/payloads/file_stale")
        machine.succeed(f"test -e {W}/memory/MEMORY.md")
        rows = machine.succeed(f"sqlite3 {W}/state/usage.db 'select count(*) from turns;'").strip()
        assert rows == "3", f"expected 3 rows after restore, got {rows}"

    with subtest("restored state is owned by the daemon user"):
        for path in ["state/usage.db", "state/lcm/payloads/file_keep", "memory/MEMORY.md"]:
            owner = machine.succeed(f"stat -c %U {W}/{path}").strip()
            assert owner == "kitaebot", f"{path} owned by {owner}"

    with subtest("ownership follows the archive, not a fixed list"):
        # An archive from before HISTORY.md moved into state/ carries it
        # at the root. Chowning a hardcoded state/+memory/ leaves it
        # root-owned and the daemon silently cannot append to it.
        machine.succeed("mkdir -p /tmp/legacy/state /tmp/legacy/memory")
        machine.succeed("echo old > /tmp/legacy/HISTORY.md")
        machine.succeed("echo x > /tmp/legacy/state/keep")
        machine.succeed("echo m > /tmp/legacy/memory/MEMORY.md")
        machine.succeed("tar -C /tmp/legacy -czf /tmp/kitaebot-restore.tar.gz .")
        machine.succeed("bash ${./restore.sh}")

        owner = machine.succeed(f"stat -c %U {W}/HISTORY.md").strip()
        assert owner == "kitaebot", f"root-level HISTORY.md left owned by {owner}"

    with subtest("the daemon is running again afterwards"):
        machine.succeed("systemctl is-active kitaebot")
  '';
}
