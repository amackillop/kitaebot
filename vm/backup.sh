#!/usr/bin/env bash
# Snapshot durable workspace state (spec 05) to a tarball on stdout.
#
# Runs inside the VM, fed to `bash -s` over ssh by `just vm-backup`. Safe
# to run against a live daemon.
set -euo pipefail

W=/var/lib/kitaebot
S=$(mktemp -d)
trap 'rm -rf "$S"' EXIT
mkdir -p "$S/state"
# Carry the real mode, not root's umask: tmpfiles declares state/ 0750
# and a plain mkdir would archive 0755, widening it on every restore.
chmod --reference="$W/state" "$S/state"

# VACUUM INTO rather than cp: these are WAL databases, so copying the
# main file without the -wal beside it silently drops everything not yet
# checkpointed. This takes a consistent snapshot of a live database and
# compacts it, so the artifact needs no -wal/-shm.
for db in "$W"/state/*.db; do
    [ -e "$db" ] || continue
    sqlite3 "$db" "VACUUM INTO '$S/state/$(basename "$db")'"
done

for f in "$W"/state/*.json "$W"/state/active_session "$W"/state/HISTORY.md; do
    [ -e "$f" ] || continue
    cp -a "$f" "$S/state/"
done

# Externalized tool output. The large_files rows reference these blobs,
# so omitting them leaves lcm_grep with nothing to search.
if [ -d "$W/state/lcm" ]; then
    cp -a "$W/state/lcm" "$S/state/"
fi

cp -a "$W/memory" "$S/"

# Name the entries rather than ".": an archive containing "./" stamps
# that entry's owner and mode onto the extraction target, and this one
# would carry the mktemp -d directory's root:root 0700.
tar -C "$S" -czf - state memory
