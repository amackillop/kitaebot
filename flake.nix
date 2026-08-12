{
  description = "Kitaebot the Autonomous Agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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

        # Source filter composed from per-kind predicates. Crane's
        # default keeps Rust artifacts only; we also need `.sql` files
        # and the `src/prompts/*.md` prompt files so `include_str!`
        # resolves inside the sandbox.
        cargoSrcFilter = path: type: craneLib.filterCargoSources path type;
        sqlSrcFilter = path: _type: builtins.match ".*\\.sql$" path != null;
        promptSrcFilter = path: _type: builtins.match ".*/prompts/.*\\.md$" path != null;
        srcFilter =
          path: type: (cargoSrcFilter path type) || (sqlSrcFilter path type) || (promptSrcFilter path type);

        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = srcFilter;
          name = "source";
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          # Link with mold in the nix sandbox.
          RUSTFLAGS = [
            "-C"
            "link-arg=-fuse-ld=mold"
            "-C"
            "link-arg=-Wl,-rpath,${pkgs.sqlite.out}/lib"
          ];
          nativeBuildInputs = [
            pkgs.mold
            # libsqlite3-sys links the nixpkgs sqlite via pkg-config.
            pkgs.pkg-config
          ];
          buildInputs = [ pkgs.sqlite ];
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
              # e2e is excluded to keep `nix flake check` fast; run it
              # with `just test-e2e`.
              cargoTestExtraArgs = "--features mock-network --bins --test kchat";
              # review_checkout tests spawn real git against a fixture repo.
              nativeCheckInputs = [ pkgs.git ];
            }
          );
        };

        # NixOS VM integration tests — separated from checks so
        # `nix flake check` stays fast. CI runs these in a dedicated
        # job with KVM access. Add new VM tests here.
        nixosTests = nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
          backup = import ./vm/test-backup.nix { inherit pkgs self; };
          egress = import ./vm/test-egress.nix { inherit pkgs self; };
        };

        packages = {
          lightpanda = pkgs.callPackage ./nix/lightpanda.nix { };

          bkb-mcp = pkgs.callPackage ./nix/bkb-mcp.nix { };

          default = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              doCheck = false; # Tests run in checks.test with mock-network
              # Stamp the build's git revision into the binary (usage ledger).
              # dirtyRev is stable per-commit; source edits force a rebuild anyway.
              GIT_SHA = self.rev or self.dirtyRev or "unknown";
            }
          );
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};

          # Host-linked binaries need the same explicit sqlite runpath
          # as the sandbox builds; cargo's test runner won't cover for
          # a missing one outside the sandbox either.
          inherit (commonArgs) RUSTFLAGS;

          packages = with pkgs; [
            just
            jq
            rust-analyzer
            # Linker
            mold
            # Cache hygiene
            cargo-sweep
            # Supply chain: advisories, sources, bans, licenses
            cargo-deny
            # Build benchmarking (just bench-build)
            hyperfine
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
      nixosModules.vm = import ./vm/configuration.nix;
    };
}
