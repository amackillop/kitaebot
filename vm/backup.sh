#!/usr/bin/env bash
# Snapshot durable workspace state (spec 05) to a tarball on stdout.
#
# Runs inside the VM, fed to `bash -s` over ssh by `just vm-backup`.
# Selection lives in the daemon binary (`kitaebot backup`,
# src/backup.rs), where new state is covered by code and tests; this
# script only stages and archives. Safe against a live daemon —
# databases are snapshotted via VACUUM INTO.
set -euo pipefail

S=$(mktemp -d)
trap 'rm -rf "$S"' EXIT
chown kitaebot:kitaebot "$S"

# As the daemon user, so anything staging creates is owned correctly.
# Unclassified-entry warnings land on stderr and reach the operator.
runuser -u kitaebot -- env KITAEBOT_WORKSPACE=/var/lib/kitaebot \
    kitaebot backup "$S" 1>&2

# Name the entries rather than ".": an archive containing "./" stamps
# that entry's owner and mode onto the extraction target, and this one
# would carry the mktemp -d directory's root:root 0700.
tar -C "$S" -czf - $(ls "$S")
