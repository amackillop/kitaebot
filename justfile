default:
    @just --list

# Run all checks (flake, nix lint/fmt, clippy, fmt, tests, audit)
check:
    nix flake check
    @just check-nix
    @just audit

# Supply-chain audit: RustSec advisories, yanked crates, dep sources,
# licenses (policy in deny.toml)
audit:
    cargo deny check

# Benchmark cold and link-heavy builds with hyperfine; pass a rev to
# compare against it in a throwaway worktree (current devshell for both)
bench-build rev="":
    scripts/bench-build.sh {{rev}}

# Fast inner-loop check on the working tree (incremental cargo).
# Mirrors the flake's fmt/clippy/test checks but is NOT the commit
# gate: `just check` stays authoritative (and in the pre-commit hook).
rust-check:
    cargo fmt -- --check
    @just lint
    @just test

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

# Warm the shared cargo target dir, sweep stale artifacts, and gcroot
# the crane dep closure so nix GC can't collect it (spec 03)
warm:
    cargo build --tests --features mock-network
    cargo sweep --time 7
    cargo sweep --maxsize 12GB
    mkdir -p .gcroots && nix build .#deps --out-link .gcroots/deps

# Run tests
test:
    cargo test --features mock-network

# Run the e2e suite alone (real daemon against a loopback fixture
# server; also part of `nix flake check`)
test-e2e:
    cargo test --features mock-network --test e2e

# Run tests matching a name (e.g. just test-one report_counts)
test-one name:
    cargo test --features mock-network {{name}}

# Run all NixOS VM integration tests
test-nixos:
    #!/usr/bin/env bash
    set -euo pipefail
    tests=$(nix eval .#nixosTests.x86_64-linux --apply builtins.attrNames --json)
    echo "NixOS tests: $tests"
    for name in $(echo "$tests" | jq -r '.[]'); do
        echo "── $name ──"
        nix build ".#nixosTests.x86_64-linux.$name" --print-build-logs --no-link
    done

# Run a single NixOS VM test by name (e.g. just test-nixos-one egress)
# --no-link: a test's output is its pass/fail, and `nix build` would
# otherwise overwrite ./result, which vm-run executes the VM from.
test-nixos-one name:
    nix build .#nixosTests.x86_64-linux.{{name}} --print-build-logs --no-link

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

# -F /dev/null: ignore ~/.ssh/config and the system config, both of which
# are nix-store symlinks that map to nobody in the sandbox user namespace
# and make ssh abort ("Bad owner or permissions"). Everything the VM
# connection needs is passed explicitly below.
SSH_OPTS := "-F /dev/null -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"

# Build the VM
#
# The deploy flake locks the parent kitaebot input (path:..) by content
# hash, so changes to vm/ or src/ are invisible without re-locking. Update
# the kitaebot input ahead of every build so vm-build always reflects the
# current working tree.
vm-build:
    nix flake update kitaebot --flake ./deploy
    nix build ./deploy

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
        just vm-stop
    fi
    if $FRESH; then
        # Refuse to delete the image while anything still holds port 2222:
        # a VM surviving vm-stop keeps running on the deleted inode, and
        # its writes are lost.
        if ssh -i ~/.ssh/id_ed25519 -p 2222 -o ConnectTimeout=1 {{SSH_OPTS}} root@localhost exit 2>/dev/null; then
            echo "error: a VM still answers on port 2222; refusing to delete its disk" >&2
            exit 1
        fi
        rm -f kitaebot.qcow2
    fi
    echo "Starting VM in background..."
    BOOT_START=$SECONDS
    nohup ./result/bin/run-kitaebot-vm > /dev/null 2>&1 &
    echo "Waiting for SSH to be ready..."
    # A fresh qcow2's first boot initializes the image and store and
    # can take several minutes; a warm boot answers in seconds.
    DEADLINE=60; $FRESH && DEADLINE=300
    READY=false
    for _ in $(seq 1 $DEADLINE); do
        if ssh -i ~/.ssh/id_ed25519 -p 2222 -o ConnectTimeout=1 {{SSH_OPTS}} root@localhost exit 2>/dev/null; then
            READY=true
            break
        fi
        sleep 1
    done
    if ! $READY; then
        echo "error: VM did not answer on port 2222 within 60s" >&2
        exit 1
    fi
    echo "VM ready in $((SECONDS - BOOT_START))s"

# Stop the VM (graceful guest shutdown; pkill only as fallback)
vm-stop:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! ssh -i ~/.ssh/id_ed25519 -p 2222 -o ConnectTimeout=1 {{SSH_OPTS}} root@localhost exit 2>/dev/null; then
        pkill -f 'qemu-system.*-name kitaebot' 2>/dev/null || true
        echo "VM not running"
        exit 0
    fi
    # A hard kill mid-write corrupts guest state (an interrupted clone
    # once wedged a checkout permanently). poweroff lets systemd stop
    # the daemon and unmount, and reaches a VM pkill cannot see.
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost poweroff 2>/dev/null || true
    for _ in $(seq 1 30); do
        if ! ssh -i ~/.ssh/id_ed25519 -p 2222 -o ConnectTimeout=1 {{SSH_OPTS}} root@localhost exit 2>/dev/null; then
            # Port silence precedes qemu exit; give the image lock time
            # to release so an immediate restart cannot lose the race.
            for _ in $(seq 1 10); do
                pgrep -f 'qemu-system.*-name kitaebot' >/dev/null 2>&1 || break
                sleep 1
            done
            sleep 1
            echo "VM stopped"
            exit 0
        fi
        sleep 1
    done
    echo "guest did not power off within 30s; killing qemu" >&2
    pkill -f 'qemu-system.*-name kitaebot' || true

# SSH into running VM
vm-ssh *flags: (vm-run flags)
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost

# Shell into the VM as the kitaebot daemon user (for debugging)
vm-shell *flags: (vm-run flags)
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} -t root@localhost su -s /bin/sh - kitaebot

# Show the journal; filter by topic, e.g. `just vm-journal notify`
vm-journal topic="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "{{topic}}" ]; then
        ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost \
            "grep -F '[{{topic}}]' /var/lib/kitaebot/state/JOURNAL.md"
    else
        ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost \
            cat /var/lib/kitaebot/state/JOURNAL.md
    fi

# Tail daemon, tinyproxy (refused CONNECTs), and kernel (egress drops) logs
vm-logs:
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost \
        journalctl --output cat -f _SYSTEMD_UNIT=kitaebot.service + _SYSTEMD_UNIT=tinyproxy.service + _TRANSPORT=kernel

# Back up durable workspace state (spec 05) to backups/ on the host
vm-backup *flags: (vm-run flags)
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p backups
    OUT="backups/kitaebot-$(date -u +%Y%m%dT%H%M%SZ).tar.gz"
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost 'bash -s' \
        < vm/backup.sh > "$OUT"
    echo "wrote $OUT ($(du -h "$OUT" | cut -f1))"

# Restore durable workspace state from a vm-backup artifact
vm-restore file *flags: (vm-run flags)
    #!/usr/bin/env bash
    set -euo pipefail
    test -f "{{file}}" || { echo "no such backup: {{file}}" >&2; exit 1; }
    scp -i ~/.ssh/id_ed25519 -P 2222 {{SSH_OPTS}} "{{file}}" \
        root@localhost:/tmp/kitaebot-restore.tar.gz
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost 'bash -s' \
        < vm/restore.sh
    echo "restored from {{file}}"

# Dump the last N log lines and exit (non-interactive; for scripts and tools)
vm-logs-dump lines="200":
    ssh -i ~/.ssh/id_ed25519 -p 2222 {{SSH_OPTS}} root@localhost \
        journalctl --output cat --no-pager -n {{lines}} _SYSTEMD_UNIT=kitaebot.service + _SYSTEMD_UNIT=tinyproxy.service + _TRANSPORT=kernel

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
# Show the review findings report from the running VM
findings:
    just ask /findings

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
    cargo run --bin kchat -- "$SOCK" {{quote(message)}}
