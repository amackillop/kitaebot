# Local development deployment
#
# Add your SSH public key below to enable access.
#
# Secrets: one file per credential in secrets/
#   echo 'sk-or-...' > secrets/provider-api-key
#   echo '0000000000:...' > secrets/telegram-bot-token
#   echo 'ghp_...'    > secrets/github-token  (when git.enabled or github.enabled)
#   gpg --export-secret-keys --armor KEY_ID > secrets/gpg-signing-key
#
# Update the sharedDirectories source path to match your checkout.
{ pkgs, ... }:
let
  lightpanda = pkgs.callPackage ../nix/lightpanda.nix { };
in
{
  kitaebot = {
    dev = false;
    sshKeys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKj473/+eAlgy1rQwuO+nCRrqhiPAWEgYPIn5j/NdN1Q desktop"
    ];
    secretsDir = "/mnt/kitaebot-secrets";
    logLevel = "kitaebot=debug";
    tools = with pkgs; [
      coreutils
      findutils
      gnugrep
      gnused
      curl
      git
      gh
      which
      lightpanda
      nix
    ];
    gitConfig = {
      name = "kitaebot";
      email = "kitaebot@pm.me";
      signingKey = "D90B07BF61863EA1";
    };
    settings = {
      provider = {
        model = "moonshotai/kimi-k2.5";
      };

      telegram = {
        enabled = true;
        chat_id = 7658696350;
      };
      git.enabled = true;
      git.co_authors = [ "Austin Mackillop <github.roundworm216@passmail.net>" ];
      github.enabled = true;
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
