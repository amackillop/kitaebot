# Kitaebot VM NixOS module
#
# This module defines the base VM configuration. Import it via:
#   kitaebot.nixosModules.vm
#
# Options:
#   kitaebot.sshKeys - List of SSH public keys for root access
#   kitaebot.dev     - Enable dev mode (shares host nix store for faster builds)
#
# For local development, see deploy/configuration.nix
{
  pkgs,
  modulesPath,
  config,
  lib,
  ...
}:

{
  imports = [
    (modulesPath + "/virtualisation/qemu-vm.nix")
  ];

  options.kitaebot = {
    sshKeys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "SSH public keys for root access";
    };

    dev = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable dev mode (shares host nix store for faster builds)";
    };
  };

  config = {
    system.stateVersion = "25.11";

    networking.hostName = "kitaebot";

    services.openssh = {
      enable = true;
      settings = {
        PermitRootLogin = "prohibit-password";
        PasswordAuthentication = false;
      };
    };

    users.users.root.openssh.authorizedKeys.keys = config.kitaebot.sshKeys;

    networking.firewall.allowedTCPPorts = [ 22 ];

    virtualisation = {
      memorySize = 1024;
      cores = 2;
      # Port forwarding for SSH (host 2222 -> guest 22)
      forwardPorts = [
        {
          from = "host";
          host.port = 2222;
          guest.port = 22;
        }
      ];
    }
    // lib.optionalAttrs config.kitaebot.dev {
      mountHostNixStore = true;
      writableStoreUseTmpfs = true;
    };

    environment.systemPackages = with pkgs; [
      vim
      git
      curl
      htop
    ];
  };
}
