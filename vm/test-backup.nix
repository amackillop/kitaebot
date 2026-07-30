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
#   - the workspace directory keeps its own owner and mode, which an
#     archive containing "./" would otherwise overwrite
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
        # The real binary, not a stub: backup.sh calls `kitaebot
        # backup` (selection lives in the binary, spec 05), and the
        # daemon is what creates the state database this test
        # snapshots. Fake credentials are fine — nothing here makes a
        # provider call.
        package = self.packages.${pkgs.system}.default;
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
        machine.succeed(f"mkdir -p {W}/context/lcm/payloads {W}/memory/topics {W}/projects/o/r")
        # The daemon owns kitaebot.db's schema (versioned migration);
        # wait for it, then seed rows through the real table.
        machine.wait_until_succeeds(
            f"sqlite3 {W}/state/kitaebot.db \"SELECT 1 FROM turns LIMIT 0;\""
        )
        machine.succeed(
            f"sqlite3 {W}/state/kitaebot.db "
            "\"INSERT INTO turns (session, source, model, calls, prompt_tokens, completion_tokens) "
            "VALUES ('s','t','m',1,1,1),('s','t','m',1,1,1),('s','t','m',1,1,1);\""
        )
        machine.succeed(f"echo keep > {W}/context/lcm/payloads/file_keep")
        machine.succeed(f"echo index > {W}/memory/MEMORY.md")
        machine.succeed(f"echo '[t] [duty] an entry' > {W}/state/JOURNAL.md")
        machine.succeed(f"echo derived > {W}/projects/o/r/file")
        machine.succeed(f"chown -R kitaebot:kitaebot {W}/state {W}/context {W}/memory {W}/projects")

    with subtest("backup takes durable state and leaves derived out"):
        machine.succeed("bash ${./backup.sh} > /tmp/backup.tar.gz")
        listing = machine.succeed("tar tzf /tmp/backup.tar.gz")
        for want in [
            "state/kitaebot.db",
            "context/lcm/payloads/file_keep",
            "state/JOURNAL.md",
            "memory/MEMORY.md",
        ]:
            assert want in listing, f"backup omits {want}:\n{listing}"
        assert "projects" not in listing, f"backup swept in derived state:\n{listing}"

    with subtest("restore replaces state rather than merging into it"):
        machine.succeed(f"echo stale > {W}/context/lcm/payloads/file_stale")
        machine.succeed(f"rm {W}/memory/MEMORY.md")
        machine.succeed("cp /tmp/backup.tar.gz /tmp/kitaebot-restore.tar.gz")
        machine.succeed("bash ${./restore.sh}")

        machine.succeed(f"test -e {W}/context/lcm/payloads/file_keep")
        machine.fail(f"test -e {W}/context/lcm/payloads/file_stale")
        machine.succeed(f"test -e {W}/memory/MEMORY.md")
        # The restarted daemon may hold the write lock (WAL conversion,
        # startup writes); retry rather than racing it.
        rows = machine.wait_until_succeeds(
            f"sqlite3 -cmd '.timeout 10000' {W}/state/kitaebot.db 'select count(*) from turns;'"
        ).strip()
        assert rows == "3", f"expected 3 rows after restore, got {rows}"

    with subtest("restored state is owned by the daemon user"):
        for path in ["state/kitaebot.db", "context/lcm/payloads/file_keep", "memory/MEMORY.md"]:
            owner = machine.succeed(f"stat -c %U {W}/{path}").strip()
            assert owner == "kitaebot", f"{path} owned by {owner}"

    with subtest("the workspace directory itself survives extraction"):
        # An archive containing "./" stamps that entry's owner and mode
        # onto $W. root:root 0700 there locks the daemon out of its own
        # WorkingDirectory, which fails as CHDIR and says nothing about
        # ownership. This escaped the first version of this test.
        got = machine.succeed(f"stat -c '%U:%G %a' {W}").strip()
        assert got == "kitaebot:kitaebot 750", f"workspace root is {got}"
        # state/ is 0750 per tmpfiles; archiving root's umask widened it.
        got = machine.succeed(f"stat -c %a {W}/state").strip()
        assert got == "750", f"state/ came back {got}"

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
        # Same check after a legacy archive, which is where "./" comes from.
        got = machine.succeed(f"stat -c '%U:%G %a' {W}").strip()
        assert got == "kitaebot:kitaebot 750", f"workspace root is {got}"

    with subtest("the daemon is running again afterwards"):
        machine.succeed("systemctl is-active kitaebot")
  '';
}
