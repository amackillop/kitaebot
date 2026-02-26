{
  description = "Kitaebot the Autonomous Agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      crane,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        toolchain = fenix.packages.${system}.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          strictDeps = true;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      in
      {
        checks = {
          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "-- --deny warnings";
            }
          );

          clippy-tests = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--tests --features mock-network -- --deny warnings";
            }
          );

          fmt = craneLib.cargoFmt { inherit src; };

          test = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoTestExtraArgs = "--features mock-network";
            }
          );
        };

        packages.default = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            doCheck = false; # Tests run in checks.test with mock-network
          }
        );

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};

          packages = with pkgs; [
            just
            rust-analyzer
            # Nix tooling
            nixfmt-rfc-style
            statix
            deadnix
          ];

          shellHook = ''
            echo "================================================================================"
            echo "Kitaebot Development Environment"

            echo "Configuring Project..."
            git config core.hooksPath .githooks

            echo "Development Environment Ready."
            echo "================================================================================"
          '';
        };
      }
    )
    // {
      # Reusable NixOS module for kitaebot VM
      nixosModules.vm = ./vm/configuration.nix;
    };
}
