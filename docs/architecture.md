# HearthFS Architecture

## Overview

HearthFS indexes a source (a local folder, a git clone, or a remote source
such as a Hearthloom brain or an MCP server) and exposes it as a read-only,
POSIX-compatible filesystem with semantic capabilities. The layers:

```
┌──────────────────────────────────────────────┐
│  Transport: FSKit extension (macOS 26.5.2+)  │
│  or NFSv3 server on 127.0.0.1                │
├──────────────────────────────────────────────┤
│  Virtual filesystem (PassthroughVfs):        │
│  real paths + magic paths                    │
├──────────────────────────────────────────────┤
│  Indexing: chunked FTS5 + embeddings +       │
│  tree-sitter structure                       │
├──────────────────────────────────────────────┤
│  Source layer (pluggable backends)           │
├──────────────────────────────────────────────┤
│  Event log (SQLite, append-only)             │
└──────────────────────────────────────────────┘
```

Everything runs in one process: the `hearthfs mount` server for NFS, or
the FSKit extension (which links the same Rust code as a static library).
The layers talk through in-process Rust interfaces; there are no sidecar
processes.

## Mount lifecycle (NFS transport)

`hearthfs mount <source> <mountpoint>`:

1. Opens the source: a local path, `git:<url>` (cloned under
   `~/.hearthfs/clones/`), `hearthloom[+https]://…`, `github:<owner/repo,…>`,
   `api:<url>` or `mcp:<url | cmd:…>`.
2. For sources without their own search, opens the indexes under
   `~/.hearthfs/<hash of source>/` and starts a full scan in the background
   (files whose content hash is unchanged are skipped). Smart sources
   (Hearthloom, GitHub, generic API) delegate search and skip local
   indexing; MCP sources are indexed locally.
3. Clears any stale mount left at the mountpoint by an unclean exit.
4. Binds the NFSv3 server to `127.0.0.1` and mounts it with `mount_nfs`
   through `127.0.0.1` (`soft,deadtimeout=15`), then checks that a read
   through the kernel answers within 10 seconds. A failed mount or
   self-check is fatal.
5. Serves requests until SIGTERM/SIGINT, then unmounts and exits.

Reads always go to the source, so file content is live. Search reflects the
last scan: `Source::watch` exists (the folder source implements it with
`notify`), but nothing consumes it yet, so files changed after a mount
started are re-indexed the next time it mounts. Indexing runs on background
tasks and never blocks filesystem operations; until it finishes, search
covers only what has been indexed so far.

`hearthfs volume …` stores volume configurations in
`~/.config/hearthfs/volumes.json` and mounts them through the same command
(NFS transport, as a detached process) or through `mount -F -t hearthfs`
(FSKit transport).

## Transports

### macOS 26.5.2+: FSKit

A host app (`fskit/`, built with XcodeGen) carries an ExtensionKit
extension at the `com.apple.fskit.fsmodule` extension point. It implements
a read-only `FSUnaryFileSystem` whose every operation calls the C ABI in
`crates/hearthfs-ffi`, which wraps `PassthroughVfs`. `mount -F -t hearthfs
<folder> <mountpoint>` mounts a local folder: FSKit hands the extension the
folder as a security-scoped `FSPathURLResource`, and the sandboxed
extension keeps its indexes in its container. No socket and no kernel
extension are involved. Background and measurements:
[fskit-spike.md](fskit-spike.md), [benchmarks.md](benchmarks.md).

### NFSv3 on 127.0.0.1

`crates/hearthfs-nfs` adapts `PassthroughVfs` to the `nfs3_server` crate's
read-only filesystem interface (lookup, getattr, read, readdir, readlink);
mutating operations return `EROFS`. The server binds `127.0.0.1` only and
the kernel mounts `127.0.0.1:/`, so other machines cannot reach it. NFS is
the transport for smart sources (the FSKit extension takes only local
folders), for macOS before 26.5.2, and for workloads that re-read a lot,
which benefit from the kernel NFS client's caching. macOS's built-in NFS
client means there is nothing to install: no macFUSE and no kernel
extension.

Mount options are deliberate: `soft,deadtimeout=15` turns a dead server
into errors instead of processes hung in uninterruptible kernel waits (the
other options are `nolocks,noresvport,vers=3,tcp`).

### Linux

Not supported yet. A FUSE transport over the same `PassthroughVfs` is the
plan.

### The VFS interface

Transports call `PassthroughVfs` (crates/hearthfs-core/src/passthrough.rs)
directly: `lookup`, `getattr`, `read`, `readdir`. It maps paths to inodes,
serves real files from the source, and synthesizes magic paths.
`vfs.rs` also sketches a `VfsHandler` trait (with xattr operations) for
future transports; nothing implements it yet.

## Indexing Layer

Three index types, all stored locally:

### Full-Text Search (FTS)

SQLite FTS5 via rusqlite, with the `porter unicode61` tokenizer. Files are
split into line-aligned chunks of about 5 KB, and each chunk is a row, so
BM25 scores local passages and a file ranks by its best chunk. The path is
an indexed column weighted 6× relative to content. FTS5 ANDs query terms
implicitly, so a long natural-language query that returns too little is
retried with its informative terms ORed. Indexed file types are listed in
`is_text_file` (indexer.rs); `.env` files are excluded so secrets never
reach search results.

### Embeddings

`Embedder` (embeddings.rs) has two backends:

- **Local (default):** `all-MiniLM-L6-v2` (384 dimensions) run in-process
  with tract (pure Rust ONNX inference). The model and tokenizer load from
  `./models/` or `~/.hearthfs/models/` (`scripts/fetch-model.sh` downloads
  a pinned, checksum-verified copy); without them, search is FTS only.
  Inputs are truncated at `max_len` wordpiece tokens (default 256,
  `HEARTHFS_EMBED_MAX_LEN`).
- **Voyage (opt-in):** `HEARTHFS_EMBEDDER=voyage:<model>` calls Voyage's
  embeddings API with a key from `$VOYAGE_API_KEY` or
  `~/.config/hearthfs/voyage.key`. This sends file text off the machine.
  `HEARTHFS_EMBEDDER=local:<dir>` selects another local ONNX model.

Each file contributes one embedding probe per ~1.5 KB window (at most 48
per file), embedded in parallel. Vectors are stored as blobs in SQLite and
searched by brute-force cosine similarity (a file scores as its best
chunk), which is fast enough at personal-corpus scale. Search fuses the FTS and vector result lists with
weighted reciprocal-rank fusion; the vector weight depends on the backend
(`Embedder::fusion_weight`: 0.05 for local models, which only backfill
recall, and 1.0 for Voyage). The weights were set with the retrieval eval
([retrieval-eval.md](retrieval-eval.md)).

### Structural Parsing

`tree-sitter` extracts structural outlines (functions, classes, modules)
from code; markdown uses a heading-based parser.

The structural index maps each file to a tree of named sections with byte ranges. This powers:
- `file.rs@outline` — returns the structural skeleton
- `file.rs@section/function_name` — returns just that section
- Token count estimates per section (so agents can budget reads)

Grammars are compiled in for Rust, Python, TypeScript, JavaScript, Go,
Java, C, C++ and Ruby.

## Magic Path Conventions

Magic paths use `@` as a separator between the real path and the virtual operation:

```
directory/@search/<query>      # ranked search results over the directory (a file)
file.ext@fit/<budget>          # best view of the file within a token budget
file.ext@outline               # structural outline with token counts
file.ext@section/<name>        # one named section
file.ext@tokens                # token count
file.ext@summary               # reserved; not implemented yet
```

**Design decision:** The `@` separator was chosen because:
- It's uncommon in real filenames
- It's valid in POSIX paths
- It visually separates the real path from the operation
- Shells handle it without quoting (unlike `?` or `#`)

Magic paths appear as virtual files and directories. `@search`,
`@section` and `@fit` are virtual directories whose child name carries the
query, section name or budget; reading `@search/<query>` returns Markdown
with ranked hits, token counts, snippets and next-step hints (the format is
a contract with Hearthloom, which parses it). On smart sources, `@fit` is
passed to the origin as `?budget=` so it can trim server-side.

## Extended attributes (planned)

Not implemented yet. The intended convention, so tools can learn about a
file without reading it:

```
user.hearthfs.tokens          # estimated token count (LLM tokens, not bytes)
user.hearthfs.language        # detected language
user.hearthfs.indexed         # timestamp of last indexing
user.hearthfs.sections        # JSON list of section names
user.hearthfs.content_hash    # hash of indexed content
```

## Event Log

An append-only SQLite database that records everything:

```sql
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
    category TEXT NOT NULL,   -- 'fs', 'index', 'query', 'system'
    event_type TEXT NOT NULL,  -- 'read', 'lookup', 'search', 'index_complete', etc.
    path TEXT,
    details TEXT,              -- JSON blob for event-specific data
    tokens_saved INTEGER,      -- estimated tokens saved vs raw read (nullable)
    duration_ms INTEGER        -- operation duration
);

CREATE INDEX idx_events_category ON events(category, timestamp);
CREATE INDEX idx_events_path ON events(path, timestamp);
```

**What gets logged:**
- Every FS read operation (path, bytes read, whether it hit a magic path)
- Every search query (query text, result count, latency)
- Indexing events (file indexed, index type, duration)
- Token savings estimates (magic path read vs raw file size)

**What doesn't get logged:**
- File contents (the log is metadata-only)
- GETATTR/LOOKUP operations (too noisy; these fire constantly)

**Retention:** No automatic pruning. The log is small (a few MB per day of heavy use). Users can truncate manually or we can add a retention policy later.

`hearthfs stats` summarizes the event logs (reads, magic reads, searches,
tokens saved, latencies).

## Source Plugin Trait

Sources provide the raw content that HearthFS indexes and serves
(crates/hearthfs-core/src/source.rs, abridged):

```rust
#[async_trait]
pub trait Source: Send + Sync {
    async fn list(&self, path: &Path) -> Result<Vec<SourceEntry>>;
    async fn read(&self, path: &Path) -> Result<Bytes>;
    /// Server-side budget-shaped read (`@fit`), if the source supports it.
    async fn read_fit(&self, path: &Path, budget: u64) -> Result<Option<Bytes>>;
    async fn stat(&self, path: &Path) -> Result<SourceMeta>;
    async fn watch(&self) -> Result<Pin<Box<dyn Stream<Item = ChangeEvent> + Send>>>;
    /// Smart sources own their search and skip local indexing.
    fn supports_search(&self) -> bool;
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SourceSearchResult>>;
    fn skip_indexing(&self) -> bool;
}
```

Implementations:

- `hearthfs-source-fs`: a local folder (also used for git clones and for
  the folder-based adapters: Obsidian, Logseq, GBrain).
- `hearthfs-core::api_source::ApiSource`: any service that serves a
  HearthFS discovery manifest; search and `@fit` are delegated to it.
- `hearthfs-source-hearthloom`: a thin `ApiSource` wrapper for Hearthloom
  brains (local daemon or https).
- `hearthfs-source-github`: repositories through the GitHub REST API,
  search delegated to GitHub code search.
- `hearthfs-source-mcp`: an MCP server's resources (stdio or Streamable
  HTTP), indexed locally.

## Composition

```
CLI (hearthfs-cli)
  → hearthfs mount: Source (one of the crates above)
    → Indexer (hearthfs-core) builds the FTS, embedding and structural indexes
    → PassthroughVfs (hearthfs-core): path resolution, magic paths
    → hearthfs-nfs serves it on 127.0.0.1 for mount_nfs
    → Event log (hearthfs-core) records reads and searches
FSKit extension (fskit/, Swift)
  → hearthfs-ffi (C ABI static library) → the same Indexer + PassthroughVfs
```

## Crate Layout

```
hearthfs/
  Cargo.toml                       # workspace root
  crates/
    hearthfs-core/                 # VFS, indexing, embeddings, event log, magic paths, ApiSource
    hearthfs-nfs/                  # NFSv3 adapter (nfs3_server)
    hearthfs-ffi/                  # C ABI over the VFS for the FSKit extension
    hearthfs-source-fs/            # local folder source
    hearthfs-source-github/        # GitHub source
    hearthfs-source-hearthloom/    # Hearthloom source
    hearthfs-source-mcp/           # MCP source
    hearthfs-cli/                  # the `hearthfs` binary: mount, volumes, adapters, auth
  fskit/                           # macOS host app + FSKit extension (XcodeGen)
```

`hearthfs-core` is the largest crate, containing the VFS layer, all
indexing logic, the event log, and magic path resolution. The transports
and sources are thin layers around it.

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime for the NFS server, sources and background indexing |
| `nfs3_server` / `nfs3_types` | NFSv3 protocol server |
| `rusqlite` (bundled SQLite) | FTS5, the vector store and the event log |
| `tract-onnx`, `tokenizers` | Local embedding inference, pure Rust |
| `tree-sitter` + grammars | Structural parsing |
| `notify` | File watching for the folder source |
| `reqwest`, `ureq` | HTTP for API, GitHub, MCP sources and Voyage |
| `clap` | CLI argument parsing |
| `serde` / `serde_json` | Configs, manifests, FFI payloads |

Licences of everything above, and of the bundled C code, are in
[THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md).

## Future Considerations

Deliberately not built yet:
- Write support through the mount
- Linux support (FUSE)
- Branching / snapshot semantics
- Multi-agent coordination primitives
- Capability tokens for access control
- Additional source adapters
- Code-specific AST features (use token-optimizer/Serena instead)
