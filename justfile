default:
    @just --list

# Run all checks (flake, nix lint/fmt, clippy, fmt, tests)
check:
    nix flake check
    @just check-nix

# Check Nix code formatting and lint
check-nix:
    nixfmt --check flake.nix vm/*.nix deploy/*.nix
    statix check flake.nix
    statix check vm/
    statix check deploy/
    deadnix flake.nix vm/ deploy/

# Build the project
build:
    cargo build

# Run tests
test:
    cargo test --features mock-network

# Lint with clippy
lint:
    cargo clippy -- --deny warnings

# Format code
fmt:
    cargo fmt
    @just fmt-nix

# Format Nix code
fmt-nix:
    nixfmt flake.nix vm/*.nix deploy/*.nix

# Auto-fix lint issues
fix:
    cargo clippy --fix --allow-dirty --allow-staged

# Run the binary
run:
    cargo run

# Build the VM
vm-build:
    nix build ./deploy

# Run the VM (builds first if needed)
vm-run:
    @just vm-build
    ./result/bin/run-kitaebot-vm

# SSH into running VM
vm-ssh:
    ssh -i ~/.ssh/id_ed25519 -p 2222 root@localhost

# Start VM in background and SSH in (terminates VM on exit)
vm-dev:
    #!/usr/bin/env bash
    set -euo pipefail
    just vm-build
    echo "Starting VM in background..."
    ./result/bin/run-kitaebot-vm > /dev/null 2>&1 &
    VM_PID=$!
    trap "kill $VM_PID 2>/dev/null || true" EXIT
    echo "Waiting for SSH to be ready..."
    for i in {1..30}; do
        ssh -i ~/.ssh/id_ed25519 -p 2222 -o ConnectTimeout=1 -o StrictHostKeyChecking=no root@localhost exit 2>/dev/null && break || sleep 1
    done
    echo "Connected to VM (PID: $VM_PID)"
    ssh -i ~/.ssh/id_ed25519 -p 2222 root@localhost
    echo "Shutting down VM..."
    kill $VM_PID 2>/dev/null || true
