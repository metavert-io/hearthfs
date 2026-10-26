# HearthFS: review guidance

HearthFS mounts folders, notes vaults, second brains and repositories as a
read-only filesystem with search and token-budgeted reads built into the
paths. Rust workspace in `crates/`; the macOS FSKit app and extension in
`fskit/` (Swift, bridging to `crates/hearthfs-ffi`). `CLAUDE.md` holds the
full invariants, `docs/architecture.md` describes the system and
`SECURITY.md` states the promises the code must keep.

## Security
- The NFS server binds 127.0.0.1 and the kernel mounts `127.0.0.1:/`
  (`nfs_bind_addr`, `build_mount_args`). Never bind 0.0.0.0 or mount through a
  LAN address; keep `noresvport`.
- Mounts are read-only: every mutation returns `EROFS`, and source files are
  never modified.
- `.env` files (`.env`, `.env.*`, `*.env`) are never indexed, so secrets can't
  reach `@search` snippets; `.env.example` is indexed.
- API and MCP secrets are referenced by name (`auth_service` →
  `~/.config/hearthfs/<name>.key`, mode 0600). They never enter
  `volumes.json`, process arguments, logs or error messages.
- No team IDs, bundle IDs, hostnames or personal paths in the repo: signing
  comes from the gitignored `fskit/Local.xcconfig`.

## Mount lifecycle (a stale mount once froze every caller in kernel wait)
- Mounts stay `soft,deadtimeout=15`. Startup clears a stale mount before
  remounting, and SIGTERM/SIGINT unmount before exit.
- A failed `mount_nfs` or post-mount self-check is fatal, never a warning.
  `mount_nfs` is spawned with `tokio::process::Command`.

## Indexing and search
- Indexes are per chunk (`fts_chunks`; vectors keyed by `(path, chunk_idx)`).
  Index schema changes bump `PRAGMA user_version` and reindex.
- FTS queries keep the OR fallback. The vector fusion weight is
  backend-specific (`Embedder::fusion_weight`); retrieval changes need the
  retrieval eval re-run (`docs/retrieval-eval.md`).
- `Embedder::embed` takes `&self` and is thread-safe: no Mutex around the
  model or tokenizer. rusqlite connections are wrapped in `Mutex<Connection>`.
- Mount indexing is asynchronous: anything that searches a fresh mount waits
  for "initial indexing complete".

## Contracts
- Hearthloom parses the `@search` output (`- **path** (score: …)`); change
  its shape only with versioning.
- `ApiSource` collection names stay unique and non-empty
  (`unique_collection_slugs`, shared by listing and slug resolution).
- Adapter and volume logic lives only in Rust (`cli/src/adapters.rs`,
  `volumes.rs`). The Swift app renders `hearthfs adapters --json` and shells
  out to the CLI.

## FSKit
The extension is arm64-only. Keep
`FSRequiresSecurityScopedPathURLResources: true` and
`CFBundlePackageType: XPC!`, and never put `--` inside an XML comment in an
entitlements file.
