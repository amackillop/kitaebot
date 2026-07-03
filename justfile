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

# Run all NixOS VM integration tests
test-nixos:
    #!/usr/bin/env bash
    set -euo pipefail
    tests=$(nix eval .#nixosTests.x86_64-linux --apply builtins.attrNames --json)
    echo "NixOS tests: $tests"
    for name in $(echo "$tests" | jq -r '.[]'); do
        echo "── $name ──"
        nix build ".#nixosTests.x86_64-linux.$name" --print-build-logs
    done

# Run a single NixOS VM test by name (e.g. just test-nixos-one egress)
test-nixos-one name:
    nix build .#nixosTests.x86_64-linux.{{name}} --print-build-logs

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
#
# The deploy flake locks the parent kitaebot input (path:..) by content
# hash, so changes to vm/ or src/ are invisible without re-locking. Update
# the kitaebot input ahead of every build so vm-build always reflects the
# current working tree.
vm-build:
    nix flake update kitaebot --flake ./deploy
    nix build ./deploy --option extra-substituters https://install.determinate.systems --option extra-trusted-public-keys cache.flakehub.com-3:hJuILl5sVK4iKm86JzgdXW12Y2Hwd5G07qKtHTOcDCM=

# Build and start the VM if not already running, wait for SSH
# (--rebuild: rebuild image and restart, --fresh: wipe qcow2 state; combinable)
vm-run *flags:
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
    # If neither flag is set and the VM already responds, do nothing.
    if ! $FRESH && ! $REBUILD; then
        if ssh -i ~/.ssh/id_ed25519 -p 2222 -o ConnectTimeout=1 {{SSH_OPTS}} root@localhost exit 2>/dev/null; then
            echo "VM already running"
            exit 0
        fi
    fi
    if $REBUILD; then
        just vm-build
    fi
    if $FRESH || $REBUILD; then
        pkill -f 'qemu-system.*-name kitaebot' 2>/dev/null && sleep 1 || true
    fi
    if $FRESH; then
        rm -f kitaebot.qcow2
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

# Shell into the VM as the kitaebot daemon user (for debugging)
vm-shell *flags: (vm-run flags)
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} -t root@localhost su -s /bin/sh - kitaebot

# Tail kitaebot logs from the VM
vm-logs:
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost journalctl --output cat -xfu kitaebot

# Tail dnsmasq (egress filter) logs — shows allowed/blocked DNS queries
vm-logs-dns:
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost journalctl --output cat -xfu dnsmasq

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

# Send one message to the daemon and print its reply, then exit
ask message: (vm-run)
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
    cargo run --bin kchat -- "$SOCK" "{{message}}"
