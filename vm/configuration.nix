# Kitaebot VM NixOS module
#
# This module defines the base VM configuration. Import it via:
#   kitaebot.nixosModules.vm
#
# Options:
#   kitaebot.package    - The kitaebot package (required)
#   kitaebot.sshKeys    - List of SSH public keys for root access
#   kitaebot.dev        - Enable dev mode (shares host nix store for faster builds)
#   kitaebot.secretsDir - Directory containing one file per credential
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

    package = lib.mkOption {
      type = lib.types.package;
      description = "The kitaebot package";
    };

    secretsDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/kitaebot-secrets";
      description = "Directory containing secret files (one per credential)";
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

    users = {
      users.root.openssh.authorizedKeys.keys = config.kitaebot.sshKeys;
      # Dedicated system user for the heartbeat service
      users.kitaebot = {
        isSystemUser = true;
        group = "kitaebot";
        home = "/var/lib/kitaebot";
      };
      groups.kitaebot = { };
    };

    systemd = {
      # Workspace directories
      tmpfiles.rules = [
        "d /var/lib/kitaebot 0750 kitaebot kitaebot -"
        "d /var/lib/kitaebot/memory 0750 kitaebot kitaebot -"
        "d /var/lib/kitaebot/projects 0750 kitaebot kitaebot -"
      ];

      # Heartbeat service (oneshot, triggered by timer)
      services.kitaebot-heartbeat = {
        description = "Kitaebot heartbeat";
        serviceConfig = {
          Type = "oneshot";
          ExecStart = "${config.kitaebot.package}/bin/kitaebot heartbeat";
          User = "kitaebot";
          Group = "kitaebot";
          WorkingDirectory = "/var/lib/kitaebot";

          # Secrets as files, not env vars.
          # systemd copies these to /run/credentials/kitaebot-heartbeat.service/
          # with mode 0400 and sets CREDENTIALS_DIRECTORY automatically.
          LoadCredential = [
            "openrouter-api-key:${config.kitaebot.secretsDir}/openrouter-api-key"
          ];

          # Process isolation
          ProtectProc = "invisible";
          ProcSubset = "pid";

          # Filesystem
          ProtectSystem = "strict";
          ProtectHome = true;
          ReadWritePaths = [ "/var/lib/kitaebot" ];
          PrivateTmp = true;

          # Privilege
          NoNewPrivileges = true;
          CapabilityBoundingSet = "";
          AmbientCapabilities = "";

          # Syscalls
          SystemCallFilter = [
            "@system-service"
            "~@privileged"
            "~@resources"
          ];
          SystemCallArchitectures = "native";

          # Network
          RestrictAddressFamilies = [
            "AF_INET"
            "AF_INET6"
            "AF_UNIX"
          ];

          # Kernel
          ProtectKernelTunables = true;
          ProtectKernelModules = true;
          ProtectKernelLogs = true;
          ProtectControlGroups = true;
          ProtectClock = true;
          LockPersonality = true;
          RestrictNamespaces = true;
          RestrictRealtime = true;
          RestrictSUIDSGID = true;
          MemoryDenyWriteExecute = true;
        };
        environment.KITAEBOT_WORKSPACE = "/var/lib/kitaebot";
      };

      # Heartbeat timer (5min after boot, then every 30min)
      timers.kitaebot-heartbeat = {
        description = "Kitaebot heartbeat timer";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "5min";
          OnUnitActiveSec = "30min";
          Persistent = true;
        };
      };
    };

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

    environment.systemPackages = [
      config.kitaebot.package
      pkgs.vim
      pkgs.git
      pkgs.curl
      pkgs.htop
    ];
  };
}
