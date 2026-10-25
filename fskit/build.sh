#!/bin/bash
# Build the HearthFS FSKit host app + extension, and optionally install it.
#
#   ./build.sh            build, then evict the DerivedData LaunchServices
#                         registration (it shadows /Applications otherwise)
#   ./build.sh --install  ... then copy to /Applications and re-register.
#                         NOTE: installing a rebuilt appex re-signs it, which
#                         invalidates the Settings enablement — expect to
#                         reboot and re-toggle (see printed instructions).
#
# Signing: copy Local.xcconfig.example to Local.xcconfig (gitignored) and
# set DEVELOPMENT_TEAM and HEARTHFS_BUNDLE_PREFIX. This script stops early
# if no team is set.
#
# Hard-won facts baked into this script (full story: CLAUDE.md "FSKit build"):
# - The appex MUST be arm64. A shell running under Rosetta on Apple silicon
#   reports x86_64 from `uname -m`, so the cargo target is pinned instead.
# - The restricted com.apple.developer.fskit.fsmodule entitlement only works
#   with AUTOMATIC signing + the FSKit Module capability on the developer
#   portal (added once via Xcode GUI); -allowProvisioningUpdates mints and
#   embeds the profile. Ad-hoc or post-build codesign = fskitd silently
#   ignores the appex. Do not resurrect the old re-sign step.
# - Every xcodebuild leaves a DerivedData copy of HearthFS.app that
#   LaunchServices registers, and stale registrations SHADOW /Applications
#   and break extension discovery — so eviction runs on every build.
# - pluginkit/pkd never lists ExtensionKit appexes on macOS 26; verify with
#   `lsregister -dump | grep <your bundle prefix>` or by mounting.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DERIVED="$SCRIPT_DIR/build/DerivedData"
APP_PRODUCT="$DERIVED/Build/Products/Release/HearthFS.app"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

export PATH="$HOME/.cargo/bin:$PATH"

echo "==> 1/4 Building Rust static lib (arm64 — do not trust uname -m here)"
(cd "$REPO_ROOT" && cargo build --release --target aarch64-apple-darwin -p hearthfs-ffi)

echo "==> 2/4 Regenerating Xcode project (xcodegen)"
(cd "$SCRIPT_DIR" && xcodegen)

# Signing settings resolve from HearthFS.xcconfig + Local.xcconfig.
setting() {
    xcodebuild -project "$SCRIPT_DIR/HearthFS.xcodeproj" -target HearthFSExtension \
        -configuration Release -showBuildSettings 2>/dev/null \
        | awk -v key="$1" '$1 == key && $2 == "=" { $1 = ""; $2 = ""; sub(/^ +/, ""); print; exit }'
}
TEAM="$(setting DEVELOPMENT_TEAM)"
EXT_ID="$(setting PRODUCT_BUNDLE_IDENTIFIER)"
if [ -z "$TEAM" ]; then
    echo "error: no DEVELOPMENT_TEAM. Copy fskit/Local.xcconfig.example to" >&2
    echo "       fskit/Local.xcconfig and set your team ID and bundle prefix." >&2
    exit 1
fi

echo "==> 3/4 Building HearthFS.app (Release, automatic signing, team $TEAM, extension $EXT_ID)"
xcodebuild \
    -project "$SCRIPT_DIR/HearthFS.xcodeproj" \
    -scheme HearthFS \
    -configuration Release \
    -derivedDataPath "$DERIVED" \
    -allowProvisioningUpdates \
    -allowProvisioningDeviceRegistration \
    build

echo "==> 4/4 Removing stale app copies and their LaunchServices registrations"
"$LSREGISTER" -u "$APP_PRODUCT" 2>/dev/null || true
"$LSREGISTER" -u "$SCRIPT_DIR/build/HearthFS.app" 2>/dev/null || true  # pre-2026-07 layout
rm -rf "$SCRIPT_DIR/build/HearthFS.app"
# GUI Xcode builds land under ~/Library/Developer/Xcode/DerivedData and both
# register with LaunchServices AND show up in Finder/Spotlight as extra
# "HearthFS" apps. Evict and DELETE them.
for gui_dir in "$HOME"/Library/Developer/Xcode/DerivedData/HearthFS-*; do
    [ -e "$gui_dir" ] || continue
    for gui_copy in "$gui_dir"/Build/Products/*/HearthFS.app; do
        [ -e "$gui_copy" ] && "$LSREGISTER" -u "$gui_copy" 2>/dev/null || true
    done
    rm -rf "$gui_dir"
    echo "    removed stale GUI-Xcode build: $gui_dir"
done
# A PATH-installed CLI (e.g. /usr/local/bin/hearthfs) that predates this
# build will be picked up by the app — warn loudly instead of guessing.
for stale_cli in /usr/local/bin/hearthfs /opt/homebrew/bin/hearthfs; do
    if [ -x "$stale_cli" ] && [ "$stale_cli" -ot "$REPO_ROOT/target/release/hearthfs" ]; then
        echo "    WARNING: $stale_cli is OLDER than target/release/hearthfs —"
        echo "             the app prefers it. Update or remove it."
    fi
done

if [[ "${1:-}" == "--install" ]]; then
    echo "==> Installing to /Applications"
    osascript -e 'tell application "HearthFS" to quit' 2>/dev/null || true
    # A prior install via the app's admin "Move to Applications" flow is
    # root-owned; plain rm fails silently-ish and leaves the OLD app in
    # place. Escalate for the removal only — the fresh copy is user-owned.
    if ! rm -rf /Applications/HearthFS.app 2>/dev/null; then
        echo "    (old install is root-owned — admin prompt to remove it)"
        osascript -e 'do shell script "rm -rf /Applications/HearthFS.app" with administrator privileges'
    fi
    cp -R "$APP_PRODUCT" /Applications/HearthFS.app
    "$LSREGISTER" -f /Applications/HearthFS.app
    # Kill any surviving old-image processes: `open` would foreground them
    # instead of launching the new binary (seen live 2026-07-11 — the "new"
    # app was the old UI).
    pkill -f "HearthFS.app/Contents/MacOS/HearthFS" 2>/dev/null || true
    sleep 1
    open /Applications/HearthFS.app   # first launch registers the extension
    # Verify: installed version matches the build, and the running process
    # is fresh.
    sleep 2
    INSTALLED_VER=$(defaults read /Applications/HearthFS.app/Contents/Info.plist \
        CFBundleShortVersionString 2>/dev/null || echo "?")
    BUILT_VER=$(defaults read "$APP_PRODUCT/Contents/Info.plist" \
        CFBundleShortVersionString 2>/dev/null || echo "?")
    if [ "$INSTALLED_VER" = "$BUILT_VER" ]; then
        echo "    verified: installed HearthFS.app is v$INSTALLED_VER (matches build)"
    else
        echo "    ERROR: installed v$INSTALLED_VER != built v$BUILT_VER — install did not take!"
        exit 1
    fi
    cat <<'EOF'

Installed. Because the appex was re-signed, macOS dropped its previous
enablement approval. To get mounting again:
  1. Reboot (fskitd is SIP-locked and holds a stale view until then).
  2. Settings > General > Login Items & Extensions > File System Extensions
     -> toggle HearthFS ON via the "By Category" list (the "By App" row can
     stay cosmetically stuck — ignore it).
  3. Prove it with a mount:  ./mount.sh <source-dir> <mountpoint>
     (or:  mount -F -t hearthfs <source-dir> <mountpoint>)
EOF
else
    echo
    echo "Built (not installed): $APP_PRODUCT"
    echo "Install with: $0 --install   (re-signs -> expect reboot + re-toggle)"
fi
