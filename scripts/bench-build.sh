#!/usr/bin/env bash
# Build-performance harness: times the cold test build and the
# link-dominated rebuild with hyperfine. With a rev argument, also
# benchmarks that revision in a throwaway worktree for comparison.
# Both trees run under the CURRENT devshell, so a comparison isolates
# repo-side changes (profiles, flags, deps) from toolchain drift.
set -euo pipefail

REV="${1:-}"
ROOT="$(git rev-parse --show-toplevel)"

bench_tree() {
    local dir="$1" label="$2"
    local target
    target="$(mktemp -d)"
    (
        cd "$dir"
        cargo fetch --quiet
        echo "== ${label}: cold test build (compile + link everything)"
        hyperfine --runs 3 --prepare "rm -rf '${target:?}'/*" \
            "CARGO_TARGET_DIR='${target}' cargo test --no-run --offline"
        echo "== ${label}: relink-only (touch binary roots, rebuild)"
        CARGO_TARGET_DIR="${target}" cargo test --no-run --offline >/dev/null 2>&1
        hyperfine --warmup 1 --runs 5 \
            --prepare "touch src/main.rs src/bin/*.rs tests/*.rs 2>/dev/null || true" \
            "CARGO_TARGET_DIR='${target}' cargo test --no-run --offline"
    )
    rm -rf "${target}"
}

bench_tree "${ROOT}" "HEAD (working tree)"

if [ -n "${REV}" ]; then
    WT="$(mktemp -d)/tree"
    git -C "${ROOT}" worktree add --quiet "${WT}" "${REV}"
    trap 'git -C "${ROOT}" worktree remove --force "${WT}"' EXIT
    bench_tree "${WT}" "${REV}"
fi
