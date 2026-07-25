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
    sshKeys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKj473/+eAlgy1rQwuO+nCRrqhiPAWEgYPIn5j/NdN1Q desktop"
    ];
    secretsDir = "/mnt/kitaebot-secrets";
    logLevel = "kitaebot=debug";
    tools = with pkgs; [
      bash
      coreutils
      direnv
      findutils
      gnugrep
      gnused
      curl
      git
      gh
      which
      lightpanda
      nix
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

      telegram = {
        enabled = true;
        chat_id = 7658696350;
      };
      git = {
        enabled = true;
        co_authors = [ "Austin Mackillop <github.roundworm216@passmail.net>" ];
        trusted_repos = [
          "amackillop/kitaebot"
          "CumuloGlobal/open-money"
          "CumuloGlobal/unhuman"
        ];
      };
      github = {
        enabled = true;
        owner = "amackillop";
        trusted_users = [ "npslaney" ];
        trusted_bots = [ "chatgpt-codex-connector" ];
      };
      linear = {
        enabled = true;
        trusted_users = [ "austin@moneydevkit.com" ];
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
  # directory prevents the kitaebot user from traversing into it.
  systemd.tmpfiles.rules = [
    "d /mnt/kitaebot-secrets 0700 root root -"
  ];
}
