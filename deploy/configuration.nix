# Local development deployment
#
# Add your SSH public key below to enable access.
# Set dev = false for production-like builds (slower, fully isolated).
_: {
  kitaebot = {
    dev = true;
    sshKeys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKj473/+eAlgy1rQwuO+nCRrqhiPAWEgYPIn5j/NdN1Q desktop"
    ];
  };
}
