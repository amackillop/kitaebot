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
#   kitaebot.gitConfig  - Attrset { name, email, signingKey? } for git identity via programs.git
#   kitaebot.promptsDir - Directory of .md prompt files symlinked into the workspace
#   kitaebot.vm         - VM resource options: { memorySize, cores, diskSize } (all in MB except cores)
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

  gitEnabled = cfg.settings.git.enabled or false;
  githubEnabled = cfg.settings.github.enabled or false;
  needsGithubToken = gitEnabled || githubEnabled;
  signingEnabled = cfg.gitConfig != null && cfg.gitConfig.signingKey != null;

  # Suppress direnv's noisy "loading/unloading" output from exec tool results.
  direnvConfig = pkgs.writeText "direnv.toml" ''
    [global]
    log_filter = "^$"
  '';

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

    promptsDir = lib.mkOption {
      type = lib.types.path;
      default = ./prompts;
      description = "Directory containing prompt files (SOUL.md, AGENTS.md, USER.md, HEARTBEAT.md)";
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
            signingKey = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "GPG key fingerprint for commit signing. Requires gpg-signing-key secret.";
            };
          };
        }
      );
      default = null;
      description = "Git identity configured via programs.git.";
    };

    vm = {
      memorySize = lib.mkOption {
        type = lib.types.ints.positive;
        default = 4096;
        description = "VM memory in megabytes";
        example = 8192;
      };
      cores = lib.mkOption {
        type = lib.types.ints.positive;
        default = 4;
        description = "Number of virtual CPU cores";
        example = 8;
      };
      diskSize = lib.mkOption {
        type = lib.types.ints.positive;
        default = 20480;
        description = "VM root disk size in megabytes";
        example = 40960;
      };
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
        "L+ /var/lib/kitaebot/SOUL.md - - - - ${cfg.promptsDir}/SOUL.md"
        "L+ /var/lib/kitaebot/AGENTS.md - - - - ${cfg.promptsDir}/AGENTS.md"
        "L+ /var/lib/kitaebot/USER.md - - - - ${cfg.promptsDir}/USER.md"
        "L+ /var/lib/kitaebot/HEARTBEAT.md - - - - ${cfg.promptsDir}/HEARTBEAT.md"
        "d /var/lib/kitaebot/.config/direnv 0750 kitaebot kitaebot -"
        "L+ /var/lib/kitaebot/.config/direnv/direnv.toml - - - - ${direnvConfig}"
      ];

      # Kitaebot daemon
      services.kitaebot = {
        description = "Kitaebot daemon";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "simple";
          ExecStartPre = lib.optional signingEnabled (
            let
              gpgImport = pkgs.writeShellScript "kitaebot-gpg-import" ''
                export GNUPGHOME=/var/lib/kitaebot/.gnupg
                mkdir -p "$GNUPGHOME" && chmod 700 "$GNUPGHOME"
                ${pkgs.gnupg}/bin/gpg --batch --import "$CREDENTIALS_DIRECTORY/gpg-signing-key"
              '';
            in
            "${gpgImport}"
          );
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
          ++ lib.optional needsGithubToken "github-token:${config.kitaebot.secretsDir}/github-token"
          ++ lib.optional signingEnabled "gpg-signing-key:${config.kitaebot.secretsDir}/gpg-signing-key";

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
        }
        // lib.optionalAttrs signingEnabled {
          GNUPGHOME = "/var/lib/kitaebot/.gnupg";
        };
      };
    };

    # direnv + nix-direnv — enables `direnv exec` for entering project
    # devshells. nix-direnv caches flake evaluation to avoid re-eval on
    # every activation.
    programs.direnv = {
      enable = true;
      nix-direnv.enable = true;
    };

    # Git — configured system-wide via /etc/gitconfig so all child
    # processes (exec tool, github tools) inherit it automatically.
    # safe.directory = "*" disables the ownership check; the VM is the
    # trust boundary, not filesystem uid matching.
    programs.git = {
      enable = true;
      config = {
        safe.directory = "*";
      }
      // lib.optionalAttrs (cfg.gitConfig != null) {
        user = {
          inherit (cfg.gitConfig) name email;
        }
        // lib.optionalAttrs signingEnabled {
          signingkey = cfg.gitConfig.signingKey;
        };
      }
      // lib.optionalAttrs signingEnabled {
        commit.gpgsign = true;
        gpg.program = "${pkgs.gnupg}/bin/gpg";
      };
    };

    networking.firewall.allowedTCPPorts = [ 22 ];

    virtualisation = {
      inherit (cfg.vm) memorySize cores diskSize;
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
