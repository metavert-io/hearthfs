#!/bin/bash
# Mount a HearthFS view of <source-dir> at <mountpoint> via the FSKit module.
#
# Usage: ./mount.sh <source-dir> <mountpoint>
#
# The -t type string must match the FSShortName declared in the extension's
# EXAppExtensionAttributes ("hearthfs", lowercase — see project.yml).
# FSVolume.h: "Match this value to the FSShortName attribute within the
# EXAppExtensionAttributes dictionary of the module's Info.plist", and the
# shipping Apple modules follow it (com.apple.fskit.msdos declares
# FSShortName "msdos" and mounts with `mount -F -t msdos`).
# -F tells mount(8) to use an FSKit module instead of a kext.
set -euo pipefail

if [ $# -ne 2 ]; then
    echo "usage: $0 <source-dir> <mountpoint>" >&2
    exit 64
fi

SRC="$(cd "$1" && pwd)"
MNT="$2"

if [ ! -d "$SRC" ]; then
    echo "error: source '$1' is not a directory" >&2
    exit 1
fi

mkdir -p "$MNT"
MNT="$(cd "$MNT" && pwd)"

echo "mount -F -t hearthfs $SRC $MNT"
mount -F -t hearthfs "$SRC" "$MNT"

echo "Mounted. Indexing starts in the background (magic paths like @search"
echo "improve as it completes). Unmount with: umount '$MNT'"
