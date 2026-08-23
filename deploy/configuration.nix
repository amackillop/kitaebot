# Local development deployment
#
# Add your SSH public key below to enable access.
#
# Secrets: one file per credential in secrets/
#   echo 'sk-or-...' > secrets/provider-api-key
#   echo '0000000000:...' > secrets/telegram-bot-token
#   echo 'ghp_...'    > secrets/github-token  (when git.enabled or github.enabled)
#   echo 'lin_api_...' > secrets/linear-api-key  (when linear.enabled)
#   gpg --export-secret-keys --armor KEY_ID > secrets/gpg-signing-key
#
# Update the sharedDirectories source path to match your checkout.
{ pkgs, ... }:
let
  lightpanda = pkgs.callPackage ../nix/lightpanda.nix { };
  bkb-mcp = pkgs.callPackage ../nix/bkb-mcp.nix { };
in
{
  kitaebot = {
    dev = true;
    # 40G filled within days of fleet onboarding (devshell closures,
    # eval caches, the shared cargo target). qcow2 is sparse; the host
    # pays only for actual use.
    vm.diskSize = 65536;
    # 8G forced CARGO_BUILD_JOBS=4 + tight cgroup caps, and kitaebot's
    # own commit hook stopped fitting the git tool's 900s budget.
    # loupe's retirement freed 16G on the host; spend some here.
    vm.memorySize = 12288;
    sshKeys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKj473/+eAlgy1rQwuO+nCRrqhiPAWEgYPIn5j/NdN1Q desktop"
    ];
    secretsDir = "/mnt/kitaebot-secrets";
    logLevel = "kitaebot=debug";
    tools = with pkgs; [
      curl
      diffutils
      fd
      findutils
      gnugrep
      gnupatch
      gnused
      gnutar
      gzip
      jq
      procps
      python3
      ripgrep
      which
      xz
      lightpanda
      # MCP servers (spec 22): spawned by the daemon, resolved via PATH.
      bkb-mcp
    ];
    gitConfig = {
      name = "kitaebot";
      email = "kitaebot@pm.me";
      signingKey = "D90B07BF61863EA1";
    };
    settings = {
      provider = {
        model = "z-ai/glm-5.2";
        max_tokens = 32768;
        model_overrides = {
          explore = "deepseek/deepseek-v4-pro";
          worker = "deepseek/deepseek-v4-pro";
          reviewer = "moonshotai/kimi-k3";
          summarizer = "deepseek/deepseek-v4-flash";
        };
      };

      context.engine = "lcm";

      # WORKAROUND (kitaebot#47): 30 was too small for distillation to
      # clear a 110k-token failure-day backlog, and the retry math
      # diverges. Revert when chunked distillation lands.
      sub_agents.max_iterations = 60;

      telegram = {
        enabled = true;
        chat_id = 7658696350;
      };
      git = {
        enabled = true;
        co_authors = [ "Austin Mackillop <github.roundworm216@passmail.net>" ];
        # Listing = trust grant (direnv allow on clone).
        repositories = {
          "amackillop/kitaebot" = {
            check = "just check; just warm";
            proposals = "github";
          };
          "CumuloGlobal/lightning-node" = {
            check = "just check";
          };
          "CumuloGlobal/open-money" = {
            check = "just check";
          };
          "CumuloGlobal/unhuman" = {
            check = "just check";
          };
          "moneydevkit/ldk-node" = {
            check = "just check";
          };
          "moneydevkit/lightning-js" = {
            check = "just check";
          };
          "moneydevkit/mdk-recovery" = {
            check = "just check";
          };
          "moneydevkit/mdkd" = {
            check = "just check";
          };
          "moneydevkit/rust-lightning" = {
            check = "just check";
          };
        };
      };
      github = {
        enabled = true;
        owner = "amackillop";
        # Humans only: agent accounts (cursoragent) and bots stay out —
        # another AI driving this one's turns is a decision, not a default.
        trusted_users = [
          "ezefrd-mdk"
          "martinsaposnic"
          "NatElkins"
          "npslaney"
          "sbddesign"
        ];
        trusted_bots = [ "chatgpt-codex-connector" ];
        issues.enabled = true;
      };
      linear = {
        enabled = true;
        trusted_users = [
          "austin@moneydevkit.com"
          "eze@moneydevkit.com"
          "martin@moneydevkit.com"
          "nat@moneydevkit.com"
          "nick@moneydevkit.com"
          "stephen@moneydevkit.com"
        ];
      };
      duties = {
        # Weekly: the shared cargo target dir persists and devshell
        # closures are GC-rooted, so daily re-warming re-verified a
        # cache that no longer goes cold. Weekly keeps cargo-sweep
        # running and still surfaces drift.
        warm.every = "7d";

        self_analysis = {
          # Hourly: the token gate makes quiet runs free, and a daily
          # cadence meant deploy-induced incidents were analyzed long
          # after the human had already found them.
          every = "1h";
          repo = "amackillop/kitaebot";
        };

        # Workaround audit (weekly watch-task): self-analysis mines
        # incidents, but a workaround is the incident that stops
        # producing incidents — once distilled into memory as "X breaks,
        # do Y instead", the bot routes around the defect silently and
        # the error tee goes quiet. This duty mines the knowledge
        # instead. Prompt duty rather than built-in machinery: memory is
        # state, not delta, so the incident gates don't fit; graduate it
        # to code only if it earns that (spec 24: with data, not in
        # advance).
        prompt = [
          # Two-lane dependency duty: security fixes authored natively
          # from the alert queue, freshness via weekly cooled-down
          # Dependabot PRs repaired with fix-deps. Both procedures
          # live in lightning-node's AGENTS.md (its PR #891).
          {
            name = "lightning-node-dep-queues";
            every = "1d";
            repo = "CumuloGlobal/lightning-node";
            prompt = ''
              Work the dependency queues of CumuloGlobal/lightning-node.
              Read the "Vulnerability & Dependency Remediation"
              section of that checkout's AGENTS.md; both procedures
              below live there — follow them exactly.

              Freshness first: if an open Dependabot version-update PR
              has failing checks, apply the freshness-lane procedure
              to the oldest one (fix-deps rung, rebase-comment only if
              stale and carrying no fix commits, superseding PR for
              real breakage), then stop.

              Otherwise security: fetch this repository's open
              Dependabot alerts via the GitHub API. If none are open,
              reply with one line and stop. Group alerts by manifest
              directory, pick the directory containing the highest
              severity alert, and apply the security-lane procedure:
              fix every alert in that directory with native pnpm or
              cargo bumps, run just fix-deps and just check, and open
              ONE pull request for the directory listing the alert
              numbers it fixes. Push nothing that fails just check
              locally. End your reply with the number of alerts still
              open repo-wide.

              Hard rules: one PR or one directory per run. Never touch
              anything under .github/workflows. Never force-push.
              Before pushing a fix commit to a Dependabot PR, check the
              PR's net diff against its base; if your fix would leave
              that diff empty (the fix reverts the bump itself), do not
              push — comment explaining why the bump is not applicable
              and recommend closing the PR instead. Never dismiss
              alerts. Never regenerate a lockfile whose manifest you
              did not just author.
            '';
          }
          {
            name = "workaround-audit";
            every = "7d";
            repo = "amackillop/kitaebot";
            prompt = ''
              Audit your memory for workarounds of kitaebot's own defects.
              Read memory/MEMORY.md and every file under memory/topics/,
              looking for knowledge that teaches routing around this
              repository's bad behavior: tool quirks with a "do this
              instead", manual command sequences that exist because a tool
              or config is missing or wrong, rules of the form "never do X,
              it breaks". Knowledge about EXTERNAL systems' quirks is not a
              finding; only kitaebot's own.

              First list the open bot-authored issues on the repo so you do
              not refile one. Then file at most ONE issue with
              github_issue_create for the workaround whose underlying
              defect is most worth fixing: name the memory entry (file and
              section), the defect it papers over, and state as an
              explicit acceptance criterion that the fix must delete or
              update that memory entry — the workaround steers you away
              from ever re-testing the broken path, so no automatic
              process will prune it; the deletion has to be part of the
              reviewed work. Leave the issue unassigned — assignment is
              the human's decision. If nothing qualifies or everything is
              already filed, reply with one line saying so.
            '';
          }
        ];
      };

      # MCP servers (spec 22). bkb is pure knowledge lookup: no side
      # effects, safe for the read-only sub-agent sets.
      mcp.servers.bkb = {
        command = "bkb-mcp";
        env.BKB_API_URL = "https://bitcoinknowledge.dev";
        explore = true;
      };
    };
  };

  # 9p shared directory for secrets. "none" skips POSIX ownership mapping
  # via xattrs — unnecessary for read-only secrets and avoids host fs issues.
  virtualisation.sharedDirectories.kitaebot-secrets = {
    source = "/home/unknown/Development/kitaebot/secrets";
    target = "/mnt/kitaebot-secrets";
    securityModel = "none";
  };

  # Lock down the mount point so only root (and thus LoadCredential) can read it.
  # The 9p mount itself ignores POSIX permissions, but restricting the mount point
  # directory prevents the kitaebot user from traversing into it. Ownership is
  # left unmanaged ("-"): tmpfiles runs as root so creation is root-owned anyway,
  # and an explicit chown fails against the already-mounted share every boot.
  systemd.tmpfiles.rules = [
    "d /mnt/kitaebot-secrets 0700 - - -"
  ];
}
