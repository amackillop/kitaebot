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
#   kitaebot.settings   - Attrset written as config.toml (uses pkgs.formats.toml)
#   kitaebot.logLevel   - RUST_LOG filter string (default: "kitaebot=info")
#   kitaebot.tools      - Packages available to the exec tool via PATH
#   kitaebot.gitConfig  - Attrset { name, email } for .gitconfig generation
#
# For local development, see deploy/configuration.nix
{
  pkgs,
  modulesPath,
  config,
  lib,
  ...
}:

let
  format = pkgs.formats.toml { };
  cfg = config.kitaebot;
  configFile = format.generate "config.toml" cfg.settings;
  toolPath = lib.makeBinPath cfg.tools;

  githubEnabled = cfg.settings.github.enabled or false;

  # Generate .gitconfig from the gitConfig option. Only produced when
  # gitConfig is non-null, symlinked into the workspace alongside config.toml.
  gitConfigFile = lib.optionalString (cfg.gitConfig != null) (
    pkgs.writeText "gitconfig" ''
      [user]
        name = ${cfg.gitConfig.name}
        email = ${cfg.gitConfig.email}
    ''
  );

  # Interactive chat via the daemon's Unix socket.
  kchat = pkgs.writeShellScriptBin "kchat" ''
    exec ${cfg.package}/bin/kchat /run/kitaebot/chat.sock
  '';
in
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

    settings = lib.mkOption {
      inherit (format) type;
      default = { };
      description = "Configuration written to config.toml in the workspace";
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "kitaebot=info";
      description = "RUST_LOG filter string";
      example = "kitaebot=debug";
    };

    tools = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = "Packages whose bin/ directories are available to the exec tool";
      example = lib.literalExpression "[ pkgs.coreutils pkgs.git pkgs.curl ]";
    };

    gitConfig = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.submodule {
          options = {
            name = lib.mkOption {
              type = lib.types.str;
              description = "Git user.name for commits";
            };
            email = lib.mkOption {
              type = lib.types.str;
              description = "Git user.email for commits";
            };
          };
        }
      );
      default = null;
      description = "Git identity. When set, generates .gitconfig in the workspace.";
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
      # Dedicated system user for the daemon service
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
        "L+ /var/lib/kitaebot/config.toml - - - - ${configFile}"
      ]
      ++ lib.optional (cfg.gitConfig != null) "L+ /var/lib/kitaebot/.gitconfig - - - - ${gitConfigFile}";

      # Kitaebot daemon
      services.kitaebot = {
        description = "Kitaebot daemon";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "simple";
          ExecStart = "${config.kitaebot.package}/bin/kitaebot run";
          Restart = "on-failure";
          RestartSec = "10s";
          User = "kitaebot";
          Group = "kitaebot";
          WorkingDirectory = "/var/lib/kitaebot";

          # Secrets as files, not env vars.
          # systemd copies these to /run/credentials/kitaebot.service/
          # with mode 0400 and sets CREDENTIALS_DIRECTORY automatically.
          LoadCredential = [
            "provider-api-key:${config.kitaebot.secretsDir}/provider-api-key"
            "telegram-bot-token:${config.kitaebot.secretsDir}/telegram-bot-token"
          ]
          ++ lib.optional githubEnabled "github-token:${config.kitaebot.secretsDir}/github-token";

          # Process isolation
          ProtectProc = "invisible";
          ProcSubset = "pid";

          # Filesystem
          ProtectSystem = "strict";
          ProtectHome = true;
          ReadWritePaths = [ "/var/lib/kitaebot" ];
          RuntimeDirectory = "kitaebot";
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
        environment = {
          KITAEBOT_WORKSPACE = "/var/lib/kitaebot";
          RUST_LOG = cfg.logLevel;
          PATH = lib.mkForce toolPath;
        };
      };
    };

    networking.firewall.allowedTCPPorts = [ 22 ];

    virtualisation = {
      memorySize = 1024;
      cores = 2;
      graphics = false;
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
      kchat
      pkgs.vim
      pkgs.git
      pkgs.curl
      pkgs.htop
    ];
  };
}
