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
    statix check nix/
    deadnix flake.nix vm/ deploy/ nix/

# Build the project
build:
    cargo build

# Run tests
test:
    cargo test --features mock-network

# Lint with clippy (both real and test builds)
lint:
    cargo clippy -- --deny warnings
    cargo clippy --tests --features mock-network -- --deny warnings

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

SSH_OPTS := "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"

# Build the VM (uses Determinate Nix binary cache)
vm-build:
    nix build ./deploy --option extra-substituters https://install.determinate.systems --option extra-trusted-public-keys cache.flakehub.com-3:hJuILl5sVK4iKm86JzgdXW12Y2Hwd5G07qKtHTOcDCM=

# Build and start the VM if not already running, wait for SSH (--rebuild: restart VM, --fresh: wipe state)
vm-run *flags: vm-build
    #!/usr/bin/env bash
    set -euo pipefail
    FRESH=false
    REBUILD=false
    for flag in {{flags}}; do
        case "$flag" in
            --fresh) FRESH=true ;;
            --rebuild) REBUILD=true ;;
            *) echo "Unknown flag: $flag" >&2; exit 1 ;;
        esac
    done
    if $FRESH; then
        pkill -f 'qemu-system.*-name kitaebot' 2>/dev/null && sleep 1 || true
        rm -f kitaebot.qcow2
    elif $REBUILD; then
        pkill -f 'qemu-system.*-name kitaebot' 2>/dev/null && sleep 1 || true
    elif ssh -i ~/.ssh/id_ed25519 -p 2222 -o ConnectTimeout=1 {{SSH_OPTS}} root@localhost exit 2>/dev/null; then
        echo "VM already running"
        exit 0
    fi
    echo "Starting VM in background..."
    BOOT_START=$SECONDS
    nohup ./result/bin/run-kitaebot-vm > /dev/null 2>&1 &
    echo "Waiting for SSH to be ready..."
    for i in {1..30}; do
        ssh -i ~/.ssh/id_ed25519 -p 2222 -o ConnectTimeout=1 {{SSH_OPTS}} root@localhost exit 2>/dev/null && break || sleep 1
    done
    echo "VM ready in $((SECONDS - BOOT_START))s"

# Stop the VM
vm-stop:
    pkill -f 'qemu-system.*-name kitaebot' || echo "VM not running"

# SSH into running VM
vm-ssh *flags: (vm-run flags)
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost

# Tail kitaebot logs from the VM
vm-logs:
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost journalctl --output cat -xfu kitaebot

# Chat with the daemon via SSH socket forwarding
chat *flags: (vm-run flags)
    #!/usr/bin/env bash
    set -euo pipefail
    SOCK=$(mktemp -d)/chat.sock
    trap 'kill $SSH_PID 2>/dev/null || true; rm -rf "$(dirname "$SOCK")"' EXIT
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} \
        -L "$SOCK":/run/kitaebot/chat.sock -N root@localhost &
    SSH_PID=$!
    for i in {1..30}; do
        [ -S "$SOCK" ] && break || sleep 0.1
    done
    cargo run --bin kchat -- "$SOCK"
