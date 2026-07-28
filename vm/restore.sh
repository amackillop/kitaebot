#!/usr/bin/env bash
# Restore durable workspace state from a tarball at /tmp/kitaebot-restore.tar.gz.
#
# Runs inside the VM, fed to `bash -s` over ssh by `just vm-restore`,
# which puts the artifact in place first.
set -euo pipefail

W=/var/lib/kitaebot
ARCHIVE=/tmp/kitaebot-restore.tar.gz
test -f "$ARCHIVE"

# The daemon would otherwise be writing the files being replaced.
systemctl stop kitaebot

# Replace rather than merge: a merged state/ could keep payload blobs
# whose large_files rows are gone, or cursors newer than the databases.
rm -rf "$W/state" "$W/memory"
tar -C "$W" -xzf "$ARCHIVE"
rm -f "$ARCHIVE"

chown -R kitaebot:kitaebot "$W/state" "$W/memory"

systemctl start kitaebot
systemctl is-active kitaebot
