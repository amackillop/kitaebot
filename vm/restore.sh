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

# Replace rather than merge: a merged context/ could keep payload blobs
# whose large_files rows are gone, or cursors newer than the databases.
rm -rf "$W/state" "$W/memory" "$W/context"
tar -C "$W" -xzf "$ARCHIVE"

# Own whatever the archive placed, rather than a hardcoded list that has
# to be kept in step with backup.sh. An older archive carries HISTORY.md
# at its root, from before that log moved under state/.
tar -tzf "$ARCHIVE" | sed 's|^\./||' | cut -d/ -f1 | grep -v '^$' | sort -u |
    while read -r top; do
        chown -R kitaebot:kitaebot "$W/$top"
    done
rm -f "$ARCHIVE"

# Archives predating the fix above contain "./", whose owner and mode
# land on $W itself. root:root 0700 there locks the daemon out of its own
# WorkingDirectory, and the failure reads as a confusing CHDIR error
# rather than anything about ownership. Match what tmpfiles declares.
chown kitaebot:kitaebot "$W"
chmod 0750 "$W"

systemctl start kitaebot
systemctl is-active kitaebot
