# Kitaebot VM NixOS module
#
# This module defines the base VM configuration. Import it via:
#   kitaebot.nixosModules.vm
#
# Options:
#   kitaebot.package    - The kitaebot package (required)
#   kitaebot.sshKeys    - List of SSH public keys for root access
#   kitaebot.dev        - Enable dev mode (adds debugging tools: vim, curl, dig, htop, kchat)
#   kitaebot.secretsDir - Directory containing one file per credential
#   kitaebot.settings   - Attrset written as config.toml (uses pkgs.formats.toml)
#   kitaebot.logLevel   - RUST_LOG filter string (default: "kitaebot=info")
#   kitaebot.tools      - Packages available to the exec tool via PATH
#   kitaebot.gitConfig  - Attrset { name, email, signingKey? } for git identity via programs.git
#   kitaebot.promptsDir - Directory of .md prompt files symlinked into the workspace
#   kitaebot.vm              - VM resource options: { memorySize, cores, diskSize } (all in MB except cores)
#   kitaebot.egressAllowlist - Domains the kitaebot uid may connect to (all others blocked)
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

  # Static UID so nftables rules can reference it at load time without
  # depending on user-creation ordering.
  kitaebotUid = 900;

  # Egress filter — the daemon reaches the network only through a
  # local forward proxy that allowlists CONNECT hosts by name.
  egressProxyPort = 8888;
  egressProxyUrl = "http://127.0.0.1:${toString egressProxyPort}";

  # One anchored ERE per allowlisted domain, matching the domain and
  # its subdomains. Anchoring matters: an unanchored `api\.github\.com`
  # would also match `api.github.com.evil.net`.
  egressFilterFile = pkgs.writeText "kitaebot-egress-filter" (
    lib.concatMapStrings (
      d: "^(.*\\.)?${lib.replaceStrings [ "." ] [ "\\." ] d}$\n"
    ) cfg.egressAllowlist
  );

  gitEnabled = cfg.settings.git.enabled or false;
  githubEnabled = cfg.settings.github.enabled or false;
  needsGithubToken = gitEnabled || githubEnabled;
  linearEnabled = cfg.settings.linear.enabled or false;
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

    package = lib.mkOption {
      type = lib.types.package;
      description = "The kitaebot package";
    };

    dev = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable dev mode (adds debugging tools to system packages)";
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

    egressAllowlist = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "openrouter.ai"
        "api.telegram.org"
        "github.com"
        "api.github.com"
        "githubusercontent.com"
        "flakehub.com"
        "api.perplexity.ai"
        "api.linear.app"
      ];
      description = "Domains the kitaebot process may connect to. All others blocked.";
    };
  };

  config = {
    system.stateVersion = "25.11";

    networking = {
      hostName = "kitaebot";
      firewall.allowedTCPPorts = [ 22 ];

      # QEMU user-mode networking has no IPv6 egress. With no v6 route
      # the proxy's connect() to AAAA records fails instantly instead of
      # stalling before the v4 fallback.
      enableIPv6 = false;

      # ── Egress filter: nftables lockdown (spec 18) ──────────────────
      #
      # The kitaebot uid may only talk to loopback — everything it needs
      # goes through the tinyproxy allowlist. Any direct egress attempt
      # (including DNS, a tunneling vector) is dropped and logged.
      nftables = {
        enable = true;
        tables."kitaebot-egress" = {
          family = "inet";
          content = ''
            chain output {
              type filter hook output priority 0; policy accept;

              # Only restrict kitaebot uid — root, sshd, nix-daemon unaffected
              meta skuid != ${toString kitaebotUid} accept

              # Loopback only (Unix sockets, forward proxy)
              oifname "lo" accept

              # Kernel-generated packets (RSTs for closed sockets,
              # TIME_WAIT ACKs) have no owning socket, so the skuid
              # match above cannot exclude them. Let tracked flows
              # finish; kitaebot can't abuse this because it can never
              # establish a non-loopback flow (the SYN is dropped).
              ct state established,related accept

              # Everything else from kitaebot uid is dropped
              log prefix "kitaebot-egress-drop: " counter drop
            }
          '';
        };
      };
    };

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
        uid = kitaebotUid;
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
        "d /var/lib/kitaebot/state 0750 kitaebot kitaebot -"
        "L+ /var/lib/kitaebot/config.toml - - - - ${configFile}"
        "L+ /var/lib/kitaebot/SOUL.md - - - - ${cfg.promptsDir}/SOUL.md"
        "L+ /var/lib/kitaebot/AGENTS.md - - - - ${cfg.promptsDir}/AGENTS.md"
        "L+ /var/lib/kitaebot/USER.md - - - - ${cfg.promptsDir}/USER.md"
        "L+ /var/lib/kitaebot/HEARTBEAT.md - - - - ${cfg.promptsDir}/HEARTBEAT.md"
        "d /var/lib/kitaebot/.config/direnv 0750 kitaebot kitaebot -"
        "L+ /var/lib/kitaebot/.config/direnv/direnv.toml - - - - ${direnvConfig}"
      ];

      services = {
        # Kitaebot daemon
        kitaebot = {
          description = "Kitaebot daemon";
          wantedBy = [ "multi-user.target" ];
          after = [
            "network-online.target"
            "tinyproxy.service"
          ];
          wants = [
            "network-online.target"
            "tinyproxy.service"
          ];
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

            # The exec tool spawns nix builds (evaluator + builders + fetchers)
            # and arbitrary dev toolchains (Go, Rust, etc). Raise both the
            # cgroup task limit and the per-UID process/thread limit — they
            # are independent enforcement points and both default too low
            # for nix build workloads in a small VM.
            TasksMax = 4096;
            LimitNPROC = 4096;

            # Secrets as files, not env vars.
            # systemd copies these to /run/credentials/kitaebot.service/
            # with mode 0400 and sets CREDENTIALS_DIRECTORY automatically.
            LoadCredential = [
              "provider-api-key:${config.kitaebot.secretsDir}/provider-api-key"
              "telegram-bot-token:${config.kitaebot.secretsDir}/telegram-bot-token"
            ]
            ++ lib.optional needsGithubToken "github-token:${config.kitaebot.secretsDir}/github-token"
            ++ lib.optional linearEnabled "linear-api-key:${config.kitaebot.secretsDir}/linear-api-key"
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
            # W^X breaks every JIT (V8 in node/bun) — the agent's own repos
            # include a JS project whose tests need it. seccomp is inherited
            # by exec children and cannot be relaxed per-child, so it is all
            # or nothing for the unit.
            MemoryDenyWriteExecute = false;
          };
          environment = {
            KITAEBOT_WORKSPACE = "/var/lib/kitaebot";
            RUST_LOG = cfg.logLevel;
            PATH = lib.mkForce toolPath;
            # All egress via the allowlisting forward proxy. Both cases
            # because tooling disagrees: reqwest/gh read the uppercase
            # names, git/curl/nix read the lowercase ones. Inherited by
            # exec tool children.
            HTTP_PROXY = egressProxyUrl;
            HTTPS_PROXY = egressProxyUrl;
            http_proxy = egressProxyUrl;
            https_proxy = egressProxyUrl;
            # Loopback stays direct — local dev servers must not be
            # routed into the proxy's domain filter.
            NO_PROXY = "localhost,127.0.0.1";
            no_proxy = "localhost,127.0.0.1";
          }
          // lib.optionalAttrs signingEnabled {
            GNUPGHOME = "/var/lib/kitaebot/.gnupg";
          };
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

    # ── Egress filter: forward proxy allowlist (spec 18) ──────────────
    #
    # tinyproxy on loopback allows CONNECT only to allowlisted hostnames
    # on port 443, refusing (and logging) everything else. The hostname
    # travels in the CONNECT request, so filtering happens by name —
    # no DNS/IP-set machinery, no stale-IP races, no CDN co-tenancy.
    services.tinyproxy = {
      enable = true;
      settings = {
        Listen = "127.0.0.1";
        Port = egressProxyPort;
        Allow = "127.0.0.1";
        # HTTPS only — no plaintext CONNECT targets.
        ConnectPort = 443;
        # Deny any host not matching the allowlist regexes. Refusals
        # are logged at Notice ("Proxying refused on filtered domain").
        Filter = egressFilterFile;
        FilterType = "ere";
        FilterDefaultDeny = true;
        LogLevel = "Notice";
      };
    };

    virtualisation = {
      inherit (cfg.vm) memorySize cores diskSize;
      graphics = false;
      # The default qemu-vm.nix behavior mounts the host nix store
      # read-only and overlays a tmpfs for writes. That tmpfs is half
      # of RAM (2G with 4G VM), which fills up the moment the agent
      # runs `nix develop` to build a devshell. Put the writable store
      # on disk instead so it has the full diskSize to work with.
      writableStoreUseTmpfs = false;
      # Port forwarding for SSH (host 2222 -> guest 22)
      forwardPorts = [
        {
          from = "host";
          host.port = 2222;
          guest.port = 22;
        }
      ];
    };

    environment.systemPackages = [
      config.kitaebot.package
    ]
    ++ lib.optionals cfg.dev [
      kchat
      pkgs.vim
      pkgs.curl
      pkgs.dig
      pkgs.htop
      pkgs.sqlite
    ];
  };
}
