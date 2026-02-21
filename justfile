default:
    @just --list

# Run all checks (flake, nix lint/fmt, clippy, fmt, tests)
check:
    nix flake check
    @just check-nix

# Check Nix code formatting and lint
check-nix:
    nixfmt --check flake.nix nix/*.nix
    statix check flake.nix
    statix check nix/
    deadnix flake.nix nix/

# Build the project
build:
    cargo build

# Run tests
test:
    cargo test

# Lint with clippy
lint:
    cargo clippy -- --deny warnings

# Format code
fmt:
    cargo fmt
    @just fmt-nix

# Format Nix code
fmt-nix:
    nixfmt flake.nix nix/*.nix

# Auto-fix lint issues
fix:
    cargo clippy --fix --allow-dirty --allow-staged

# Run the binary
run:
    cargo run
