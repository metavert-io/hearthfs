# HearthFS: contributor and agent guide

HearthFS mounts folders, notes vaults, second brains and repositories as a
read-only filesystem with search and token-budgeted reads built into the
paths (see README.md). This file is the edit-time rulebook: the build
checks, the invariants that keep fixed bugs fixed, and the FSKit facts that
are expensive to rediscover. `docs/architecture.md` describes the system.
If a `CLAUDE.local.md` exists next to this file, read it too: it holds a
maintainer's local notes, which are not part of the repository.

## Build checks (run all three before calling anything done)

```
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

The macOS app and FSKit extension build separately: `./fskit/build.sh`
(see "FSKit build" below and fskit/README.md).

## Benchmarks and retrieval evals

- `docs/benchmarks.md` holds the published measurements (indexing
  throughput, FSKit vs NFS read latency). Update it when new numbers land,
  and keep the README's Performance section consistent with it.
- `docs/retrieval-eval.md` documents the eval harness. Retrieval tier
  (free): `cargo run --release --example retrieval_eval -- <root>`. Agent
  tier (paid, via `claude -p`): `./scripts/eval/run_stage.sh <root>
  <corpus>...`. A corpus is a folder holding `question.json` and `data/`;
  `eval/fixtures/sample/` is a small example.
- Before changing retrieval (fusion weights, the path weight in fts.rs,
  chunk sizes, the FTS OR fallback), re-run the retrieval eval on corpora
  of realistic size. Two findings are counterintuitive and were measured,
  not guessed:
  - **The vector leg's fusion weight is backend-dependent and deliberate**
    (`Embedder::fusion_weight`: 0.05 for local models, which only backfill
    recall; 1.0 for Voyage; `HEARTHFS_VEC_WEIGHT` overrides). Tuned BM25
    with path weighting beat both a stronger local model (bge-small) and
    voyage-4 on first-hit precision at 1k–5k files; the cloud backend's
    measured value is top-20 recall on large corpora. Don't assume a
    stronger embedder helps without re-running the eval's weight sweep
    (`HEARTHFS_EVAL_VEC_WEIGHT`).
  - **Single-hop "needle" misses are semantic-inference cases** (the answer
    shares no words with the question), not query-parsing bugs; further
    single-query MRR tuning was deliberately stopped.

## Architecture invariants (violating these re-breaks fixed bugs)

- **Indexes are per-chunk, not per-file** (fts_chunks table, vectors keyed
  by (path, chunk_idx)); files are scored by their best chunk. Whole-file
  granularity collapses retrieval past ~300 files. Index schema changes bump
  `PRAGMA user_version` and drop-and-reindex (index dirs are caches).
- **FTS queries need the OR fallback** (fts.rs): FTS5 implicitly ANDs terms,
  so long natural-language queries return nothing without it.
- **The local embedder truncates at `max_len` wordpiece tokens** (default
  256, `HEARTHFS_EMBED_MAX_LEN`), and each chunk's probe is cut to
  `Embedder::probe_chars` to match. `Embedder::embed` is `&self` and
  thread-safe (tract `SimplePlan::run` is `&self`); do not reintroduce
  Mutexes around the model/tokenizer.
- **Mount indexing is asynchronous**: the NFS server serves before
  `full_scan` finishes (look for "initial indexing complete" at RUST_LOG=info).
  Anything that benchmarks or tests search through a fresh mount must wait
  for that line (scripts/eval/run_stage.sh does).
- **The NFS server is loopback-only**: it binds 127.0.0.1 and `mount_nfs`
  mounts `127.0.0.1:/` (`nfs_bind_addr` / `build_mount_args` in
  cli/src/main.rs, guarded by tests). Never bind 0.0.0.0 or mount through a
  LAN IP — that serves the mounted source to the whole network, which is
  what 0.1.2 did after an early macOS 26 build refused loopback NFS mounts
  with EADDRNOTAVAIL (26.5.2 accepts them). `noresvport` stays set. Mount
  must spawn via `tokio::process::Command` (mount_nfs blocks in-kernel).
- **rusqlite connections are not Sync** — FtsIndex/EventLog/VectorStore wrap
  them in `Mutex<Connection>`.
- **NFS mount lifecycle invariants (2026-07-17 wedge incident — a stale
  mount froze every caller, including a Hearthloom daemon, in uninterruptible
  kernel wait):** mounts MUST stay `soft,deadtimeout=15` (hard mounts hang
  forever); startup MUST clear any stale mount at the mountpoint before
  remounting (mounting over one is the wedge); SIGTERM/SIGINT MUST unmount
  before exit; `mount_nfs` failure and a failed post-mount self-check
  (10s read probe) are FATAL — never warn-and-continue with a server
  nobody can reach. Probe a possibly-wedged mount only from a killable
  background subshell, never a bare `ls`.
- Indexed text-file extensions live in indexer.rs `is_text_file` — `.eml`/
  `.ics`/`.vcf` are deliberately included (email corpora); don't prune.
  `.env` files (`.env`, `.env.*`, `*.env`) are deliberately excluded so
  secrets never reach `@search` snippets; `.env.example` is indexed.
- **Adapters/volumes have ONE implementation** (cli/src/adapters.rs +
  volumes.rs): the macOS app renders `hearthfs adapters --json` and shells
  every operation to the CLI. Never implement adapter or volume logic in
  Swift — add adapters in Rust and the UI picks them up automatically.
- **MCP sources index locally on purpose** (skip_indexing=false) — that's
  the differentiator (MCP has no standard search). git sources resolve to
  local clones (prepare_git) and index locally too; github/api/hearthloom
  delegate search and skip indexing. Secrets for api/mcp volumes are
  referenced by NAME (auth_service field → ~/.config/hearthfs/<name>.key)
  so they never enter volumes.json or argv.
- **ApiSource collection entries must be unique and non-empty** —
  duplicate or empty names corrupt NFS READDIR replies client-side ("RPC
  struct is bad"). `unique_collection_slugs` enforces this and is shared
  by listing AND slug→id resolution; keep them mirrored. Both collection
  styles are supported and tested: template paths (`/people/{slug}.md`)
  and plain paths + item template (`/entities` + `/brain/entities/{id}.md`).
- **The `@search` output format is a cross-project contract**: Hearthloom
  parses it (magic_search in its virtual_source.rs). Don't change the
  `- **path** (score: …)` markdown shape without versioning it.
- `@fit` on smart sources goes through `Source::read_fit` (server-side
  `?budget=`); the budget is part of the ApiSource cache key. Secrets live
  at `~/.config/hearthfs/<service>.key` (0600) via `hearthfs auth`.

## Build hygiene: remove stale copies on every build and install

Stale local copies are a persistent source of confusion ("multiple HearthFS
in Finder", an old app image foregrounded by `open`, a root-owned
/Applications copy silently surviving `rm`). build.sh automates all of this:
- DELETE (not just lsregister -u) stale app copies: GUI-Xcode DerivedData
  (`~/Library/Developer/Xcode/DerivedData/HearthFS-*`) and legacy
  fskit/build/HearthFS.app.
- `pkill` old app processes before `open` after an install — `open`
  foregrounds a running old-image process instead of launching the new
  binary.
- Prior installs may be root-owned (the app's admin "Move to
  Applications" flow); plain `rm -rf` fails and leaves the OLD app in
  place — escalate removal, install user-owned.
- VERIFY after install: CFBundleShortVersionString of /Applications copy
  matches the build; `hearthfs --version` exists for the CLI.
- Warn when a PATH-installed CLI (/usr/local/bin, /opt/homebrew/bin) is
  older than target/release — the app's findCLI() prefers PATH copies.

## FSKit build (macOS 26) — hard-won facts, do not re-derive

- fskit/ is XcodeGen-generated (`project.yml`); Swift appex bridges to
  crates/hearthfs-ffi (C ABI staticlib). Build: `./fskit/build.sh` (wraps
  arm64 cargo + xcodegen + `xcodebuild … -allowProvisioningUpdates
  -allowProvisioningDeviceRegistration build` + LS-registration eviction);
  `--install` additionally deploys to /Applications (⇒ reboot + re-toggle).
- **Signing is per developer.** `fskit/HearthFS.xcconfig` (committed, wired
  in through project.yml `configFiles`) holds placeholders and
  `#include?`s the gitignored `fskit/Local.xcconfig` (template:
  `Local.xcconfig.example`) with `DEVELOPMENT_TEAM` and
  `HEARTHFS_BUNDLE_PREFIX`. Bundle IDs are `$(HEARTHFS_BUNDLE_PREFIX)` (app)
  and `$(HEARTHFS_BUNDLE_PREFIX).fskit` (extension). Never commit a team ID
  or a concrete bundle ID, and never hardcode one in Swift: the app reads
  the embedded extension's ID at runtime and the extension logs under its
  own bundle ID. `fskit/HearthFS.xcodeproj` is generated and not tracked.
- **FSKit is the adopted default mount on macOS 26+** (2026-07-11 decision
  gate: cold-read p95 1.99× NFS, sub-ms absolute; warm/readdir 10–25× slower
  — no kernel caching on the path-URL route — so re-read-heavy harnesses
  such as scripts/eval stay on NFS). See docs/benchmarks.md.
- The FSKit entitlement is RESTRICTED: ad-hoc signing → pkd silently ignores
  the appex (no logs). Automatic signing WITHOUT the Xcode "FSKit Module"
  capability STRIPS the entitlement silently. Required once per team: add the
  capability on the extension target in Xcode GUI, accept Apple's PLA at
  developer.apple.com (Account Holder only), then the CLI flags above mint and
  embed the profile.
- Entitlements plist must not contain `--` inside XML comments (AMFI parser
  rejects the whole file).
- mount type string = FSShortName (lowercase `hearthfs`): `mount -F -t hearthfs`.
- The appex MUST be arm64. On Apple silicon a shell running under Rosetta
  reports x86_64 from `uname -m`, so build the staticlib for an explicit
  target: `cargo build --release --target aarch64-apple-darwin -p hearthfs-ffi`
  (build.sh does). Also required: `CFBundlePackageType: XPC!` in the appex
  Info.plist.
- **pluginkit/pkd never lists ExtensionKit appexes on macOS 26 — that is
  normal, not a failure.** Verify discovery with `lsregister -dump | grep
  <bundle id>` (plugin record + extension point) and by streaming fskitd
  logs during a mount ("Found extension for fsShortName"). zsh shadows
  `log` with a builtin — use `/usr/bin/log`.
- Stale LaunchServices registrations of build-dir copies of HearthFS.app
  (DerivedData, fskit/build) shadow /Applications and break discovery —
  evict with `lsregister -u <path>` then `lsregister -f /Applications/HearthFS.app`.
- Enablement toggle lives in Settings > General > Login Items & Extensions >
  File System Extensions (admin auth; not scriptable). `FSClient.shared.installedExtensions`
  (Swift async) / `fetchInstalledExtensionsWithCompletionHandler:` lists modules
  with an `isEnabled` flag, but it can omit an enabled module (see below) —
  a successful mount is the real proof.

- **WORKS on macOS 26.5.2+ (verified 2026-07-11); was blocked by a macOS 26.3.x
  platform bug.** On 26.3.1 `mount -F -t hearthfs` hung at fskitd "Probe starting"
  and the extension never launched — a known platform bug in FSKit's probe path,
  acknowledged on Apple's developer forums (thread 786270) and fixed in point
  releases. Updating to **26.5.2 (build 25F84)** fixed it: mount succeeds, the
  extension runs, and listing/reads/`@search` all verified. If mount hangs at
  "Probe starting" again on some future OS, suspect an OS regression, NOT our
  config — notarization, signing tweaks and disabling SIP did not help other
  projects with the same symptom. The old "toggle won't stick / needs reboot"
  theory was also WRONG: the toggle persists in
  `~/Library/Group Containers/group.com.apple.fskit.settings/enabledModules.plist`.

- **Sandboxed reads need `FSRequiresSecurityScopedPathURLResources: true`**
  (project.yml EXAppExtensionAttributes). With it false the appex mounts but every
  read hits a kernel `deny(1) file-read-data` (empty listings / ENOENT); true makes
  FSKit pass the source as a security-scoped URL, which the volume code already
  `startAccessingSecurityScopedResource()`s. Don't set it back to false.

- **Every rebuild breaks Settings enablement (re-sign invalidates the approval).**
  Symptoms after reinstalling a fresh appex: duplicate "By App" rows, a toggle that
  flashes but won't flip, then "no toggle at all". Recovery: (1) `lsregister -u` the
  DerivedData build-dir copy (it re-registers on every build and shadows /Applications
  — the persistent dup source), (2) if a stale entry lingers, remove the
  extension's bundle ID from `enabledModules.plist` (user-writable; back it up),
  (3) **reboot** (fskitd is SIP-locked, can't restart live to drop its stale view),
  (4) re-enable via Settings "By Category" toggle (the "By App" toggle stays cosmetically
  stuck — ignore it; the mount proves enablement). Step (1) is automated:
  build.sh evicts its own DerivedData, the legacy fskit/build copy, AND
  GUI-Xcode DerivedData registrations on every build (GUI builds register
  too — seen live 2026-07-11). Verify the enable took by mounting, not by
  `FSClient.fetchInstalledExtensions` (which is flaky — omits ours even when enabled+mounting).

- Host onboarding UI (fskit/Sources/App/): `OnboardingView` +
  `OnboardingModel` show a live checklist (installed → extension enabled via
  FSClient → mount a folder), an "Open File System Extensions…" button that
  deep-links to the fsmodule Settings pane, and folder-picker mount/unmount
  (shells out to /sbin/mount; the host app is non-sandboxed so this is
  allowed).
