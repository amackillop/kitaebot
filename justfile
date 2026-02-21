default:
    @just --list

# Run all checks (clippy, fmt, tests)
check:
    nix flake check

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

# Auto-fix lint issues
fix:
    cargo clippy --fix --allow-dirty --allow-staged

# Run the binary
run:
    cargo run
