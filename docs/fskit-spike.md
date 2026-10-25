# FSKit Spike — Escaping the NFS LAN-IP Workaround

Status: **COMPLETE — FSKit adopted as macOS 26+ default** (2026-07-11).
Prototype shipped, verified end-to-end on macOS 26.5.2 (mount, reads,
@search, zero sandbox denies), and the decision-gate benchmark ran:
cold-read p95 is 1.99× NFS (mean of 5 runs; 2.27× steady-state) — a wash
against the <2× gate, decided by qualitative factors (an early macOS 26
build refused NFS loopback mounts, and the LAN-IP workaround was fragile
and LAN-exposed). Absolute FSKit overhead is ~0.3–0.5 ms per cold read.
Warm reads/readdir are 10–25× slower than NFS (no kernel caching on the
path-URL route), so re-read-heavy harnesses stay on NFS. Full numbers:
[benchmarks.md](benchmarks.md). Build/signing lore: CLAUDE.md FSKit
section.

## Why

In mid-2026 an early macOS 26 (Tahoe) build refused NFS loopback mounts,
which forced a LAN-IP workaround (`ipconfig getifaddr en0`, bind 0.0.0.0):
fragile (it broke off-network and raced DHCP changes) and exposed to the
LAN, and community reports described broader NFS reliability problems on
Tahoe. FSKit is the kext-free way out. (macOS 26.5.2 accepts loopback NFS
mounts again, and the NFS fallback is loopback-only since; see the
CHANGELOG.)

## Verified at the start of the spike (macOS 26.3.1, Xcode 26.6, SDK 26.5)

- `FSKit.framework` ships in the macOS 26.5 SDK.
- **`FSPathURLResource` exists** (`FSResource.h:408`, `FSKIT_API_AVAILABILITY_V2`):
  a resource representing a *path in the system file space* — i.e. a fully
  virtual filesystem backed by a directory URL, no block device required.
  Takes `initWithURL:writable:`; supports security-scoped URLs.
- `FSVolumeReadWriteOperations` (pure user-space I/O path) and
  `FSVolumeKernelOffloadedIOOperations` (block-device-only fast path) both
  present. We can only use the former — expect an XPC round-trip per
  uncached read.

## External findings (2026-07-04)

- A developer confirmed with Apple DTS that a fully virtual FS mounts via
  `FSPathURLResource` on macOS 26 (Apple forums thread 799283).
- Programmatic mounting goes through `/sbin/mount`; not callable from inside
  App Sandbox (blocks MAS distribution; FB20186709 filed upstream). Fine for
  our CLI use.
- Apple runs its own ExFAT/FAT32 in user space via FSKit on 26.1; ExtendFS
  ships ext4-read this way. The API is production-real.
- Alternative: macFUSE 5.2's FSKit backend (kext-free since 5.0, sandbox-
  capable since 5.2) — but adds a macFUSE install dependency and mounts only
  under `/Volumes`. Rejected as primary path: HearthFS's pitch includes
  "no macFUSE, no kext."

## Proposed architecture

```
hearthfs-cli (Rust)
  └─ mounts via /sbin/mount -F -t <our-fs-type> <source-path> <mountpoint>
HearthFS.app (thin Swift shell, needed to host the appex)
  └─ HearthFSExtension.appex (FSUnaryFileSystem, Swift)
       └─ calls libhearthfs_ffi.a (new crate: hearthfs-ffi)
            └─ hearthfs-core PassthroughVfs (unchanged)
```

- `hearthfs-ffi`: `#[no_mangle] extern "C"` wrappers over `PassthroughVfs`
  lookup/getattr/read/readdir/readlink/xattr, plus init(source_dir, index_dir).
  The VFS trait is already transport-agnostic (`vfs.rs`) — this is the third
  adapter alongside NFS and (future) FUSE.
- The appex implements `FSVolumeOperations`/`FSVolumeReadWriteOperations`,
  delegating every op to the FFI. Magic paths work unchanged (they're plain
  lookups/reads to the VFS).
- CLI keeps NFS as fallback for macOS < 26; picks FSKit when available.

## Risks / open questions for the prototype

1. **Per-read XPC overhead** — DTS says the user-space path is slower than
   macFUSE's kext. Our reads are small (magic paths) or streamed once
   (indexing is out-of-band), so likely acceptable; measure it.
2. **Appex registration ergonomics** — requires enabling in System Settings
   once per install; needs a signed app bundle. Distribution story becomes
   "download HearthFS.app + brew-install CLI" instead of pure cargo install.
3. **`FSUnaryFileSystem` = one volume per resource** — fine (one mount per
   source), matches current single-mount v0 scope.
4. Signing needs the Xcode GUI once per team (to add the FSKit Module
   capability); everything else can build from the command line.

## Outcome (2026-07-11)

1. `crates/hearthfs-ffi`: a C ABI over `PassthroughVfs`, built as a static
   library.
2. `fskit/`: the host app and the FSKit extension (Swift), generated with
   XcodeGen.
3. `mount -F -t hearthfs <folder> <mountpoint>` mounts through the
   extension; listing, reads and the magic paths work through the mount
   (macOS 26.5.2+; 26.3.x had a platform bug that hung mounts at probe).
4. Decision gate: if p95 cold-read latency is under 2× the NFS path, adopt
   FSKit as the default on macOS 26+. **Passed at 1.99× (borderline); FSKit
   adopted.** Numbers in [benchmarks.md](benchmarks.md).
