# Local development deployment
#
# Add your SSH public key below to enable access.
#
# Secrets: one file per credential in secrets/
#   echo 'sk-or-...' > secrets/openrouter-api-key
#
# Update the sharedDirectories source path to match your checkout.
{ pkgs, ... }:
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
    ];
    settings.telegram = {
      enabled = true;
      chat_id = 7658696350;
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
