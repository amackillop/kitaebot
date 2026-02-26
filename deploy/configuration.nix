# Local development deployment
#
# Add your SSH public key below to enable access.
#
# Secrets: echo 'OPENROUTER_API_KEY=sk-or-...' > secrets/.env
# Update the sharedDirectories source path to match your checkout.
_: {
  kitaebot = {
    dev = false;
    sshKeys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKj473/+eAlgy1rQwuO+nCRrqhiPAWEgYPIn5j/NdN1Q desktop"
    ];
    secretsFile = "/mnt/kitaebot-secrets/.env";
  };

  # 9p shared directory for secrets. "none" skips POSIX ownership mapping
  # via xattrs — unnecessary for read-only secrets and avoids host fs issues.
  virtualisation.sharedDirectories.kitaebot-secrets = {
    source = "/home/unknown/Development/kitaebot/secrets";
    target = "/mnt/kitaebot-secrets";
    securityModel = "none";
  };
}
