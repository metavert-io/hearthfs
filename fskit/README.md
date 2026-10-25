# HearthFS FSKit module

Native macOS mounts for HearthFS via FSKit (no NFS loopback hack). A minimal
host app (`HearthFS.app`) carries an ExtensionKit extension
(`HearthFSExtension.appex`, extension point `com.apple.fskit.fsmodule`) that
implements a **read-only** `FSUnaryFileSystem` over the Rust VFS
(`libhearthfs_ffi.a`, `crates/hearthfs-ffi`). All lookups/reads go through
the `hfs_*` C API; mutation ops return EROFS. Magic paths (`@search/...`,
`@fit/...`) are plain lookups/reads to the VFS, so they work through the
mount unchanged.

**This is the default mount transport on macOS 26+** (decision 2026-07-11:
cold-read p95 ≈ 2× the NFS path but sub-millisecond absolute, with no
socket and no kext; see `docs/benchmarks.md`).

Requires: **macOS 26.5.2+** (26.3.x has a system bug that hangs FSKit mounts
at probe — the onboarding app gates on this), Xcode 26.x, `xcodegen`, Rust
toolchain, and a paid Apple Developer team with the FSKit Module capability
(see Signing below).

## Signing setup (once per developer)

```sh
cp Local.xcconfig.example Local.xcconfig   # git ignores Local.xcconfig
```

Set `DEVELOPMENT_TEAM` to your team ID and `HEARTHFS_BUNDLE_PREFIX` to a
reverse-DNS prefix you control. The app's bundle ID is the prefix and the
extension's is `<prefix>.fskit`; the Swift code reads them at runtime, so
nothing else needs editing. `HearthFS.xcconfig` (committed) supplies
placeholders and includes your `Local.xcconfig`.

## Building and installing

```sh
./build.sh            # build only (safe: won't touch the installed app)
./build.sh --install  # build + install to /Applications + re-register
```

`build.sh` builds the arm64 Rust staticlib (`--target aarch64-apple-darwin`:
a shell running under Rosetta on Apple silicon reports x86_64 from
`uname -m`, so the target is pinned), regenerates the Xcode project with
`xcodegen` (`HearthFS.xcodeproj` is generated output and not tracked),
builds with automatic signing (`-allowProvisioningUpdates`), and evicts the
DerivedData copy's LaunchServices registration (stale build-dir
registrations shadow /Applications and silently break extension
discovery).

> **Every `--install` requires a reboot + re-toggle.** Re-signing the appex
> invalidates the previous Settings approval, and fskitd (SIP-locked) holds
> its stale view until reboot. After rebooting: Settings → General → Login
> Items & Extensions → File System Extensions → enable HearthFS via the
> **"By Category"** list. The "By App" row may stay cosmetically stuck —
> ignore it; a successful mount is the real proof. Batch code changes to
> minimize installs.

## First-time setup

1. Signing setup (above), then add the FSKit Module capability once per
   team: run `xcodegen` here, open `HearthFS.xcodeproj` in Xcode, and on the
   HearthFSExtension target use "+ Capability" → **FSKit Module**.
2. `./build.sh --install`
3. Reboot, then enable the extension (path above).
4. Mount — either through the app's onboarding window (it picks a
   `/Volumes/<name>` mount point and handles admin prompts), or:

   ```sh
   ./mount.sh ~/some/source/dir /tmp/hearth-mnt
   # equivalently: mount -F -t hearthfs ~/some/source/dir /tmp/hearth-mnt
   ```

   The `-t hearthfs` type string matches the `FSShortName` in the
   extension's Info.plist. The sandboxed extension keeps its indexes under
   `~/Library/Containers/<prefix>.fskit/Data/.hearthfs/fskit-<hash>/`;
   indexing runs in the background after mount, so `@search` results improve
   as it completes.

5. **Unmount:** `umount /tmp/hearth-mnt`

## Verifying / debugging

- **Do not use pluginkit** — pkd never lists ExtensionKit appexes on
  macOS 26; an empty `pluginkit -m` is normal, not a failure.
- Discovery: `lsregister -dump | grep <prefix>` (LaunchServices plugin
  record) — `lsregister` lives under
  `/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/`.
- Live mount trace: `/usr/bin/log stream --predicate 'process == "fskitd" OR
  subsystem CONTAINS[c] "fskit"'` — look for "Found extension for
  fsShortName". (zsh shadows `log` with a builtin; use `/usr/bin/log`.)
- Extension logs: `/usr/bin/log stream --predicate 'subsystem == "<prefix>.fskit"'`
  (the extension logs under its own bundle ID).
- Sandbox denials (should be none):
  `/usr/bin/log show --last 2m --predicate 'process=="kernel"' | grep -i deny`.
- If a mount hangs at fskitd "Probe starting": check `sw_vers` first — that
  is the macOS 26.3.x platform-bug signature, fixed by updating to 26.5.2+.
  Do not chase signing/notarization; research proved them irrelevant.

## Signing (automatic; the old ad-hoc flow is dead)

- `com.apple.developer.fskit.fsmodule` is a **restricted** entitlement.
  Ad-hoc signing builds fine but fskitd **silently ignores** the appex; a
  post-build `codesign` with the entitlement does not help. The only working
  path: **automatic signing** with a real (paid) team, set in
  `Local.xcconfig`, where the "FSKit Module" capability was added to the
  extension's bundle ID (one-time, Xcode GUI) and the PLA accepted on
  developer.apple.com (Account Holder).
  `xcodebuild -allowProvisioningUpdates` then mints and embeds the profile —
  which is exactly what `build.sh` runs.
- `FSRequiresSecurityScopedPathURLResources: true` (project.yml) is
  load-bearing: without it the mount works but **every read** hits a kernel
  `deny(file-read-data)` (empty listings/ENOENT). Don't set it back.
- The entitlements plist must not contain `--` inside XML comments (AMFI
  rejects the whole file).
- The Rust lib is built against the 26.5 SDK while the app targets 26.0; the
  linker warns ("object file was built for newer macOS version"). Harmless.

## Layout

- `project.yml` — XcodeGen spec (regenerate with `xcodegen`; the
  `.xcodeproj` is generated output and git ignores it). The
  EXAppExtensionAttributes keys are verified against Apple's shipping FSKit
  modules — see comments in-file.
- `HearthFS.xcconfig` — signing defaults for both targets; includes the
  gitignored `Local.xcconfig` (template: `Local.xcconfig.example`).
- `Sources/App/` — SwiftUI host app: `OnboardingView`/`OnboardingModel`
  (live checklist: install → enable → mount, with an OS-version gate).
- `Sources/Extension/HearthFSExtension.swift` — the FSKit module
  (probe/load, lookup, readdir/enumerate, getattr, read; EROFS mutations).
- `Sources/Extension/HearthFSFFI.h` — bridging header for the C ABI.
- `HearthFSExtension.entitlements` — signed in via CODE_SIGN_ENTITLEMENTS.
- `Generated/` — Info.plists written by xcodegen.
- `build/DerivedData/Build/Products/Release/HearthFS.app` — build output.
