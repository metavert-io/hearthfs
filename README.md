# HearthFS

**Give your AI agents a filesystem, not another API.** HearthFS mounts your
knowledge — folders, notes vaults, second brains, GitHub repos — as a real,
read-only filesystem with search and token-budgeted reads built into the
paths. Any agent that can read a file (Claude Code, Cursor, Codex, your own
scripts) instantly gets semantic search and budget-shaped context over
everything you mount, with no SDK, no MCP server, and no cloud: indexing and
retrieval run entirely on your machine.

## Why a filesystem?

Every agent framework already ships file tools — `read`, `grep`, `ls`. A
filesystem is the one integration surface that requires **zero integration**:

- **Universal.** One mount serves Claude Code, Cursor, a Python script, and
  `grep` simultaneously. No per-tool connectors, no tool schemas to
  maintain, no SDK version drift.
- **Composable.** Magic paths are just paths: an agent can `cat
  report.md@fit/2000` inside any shell pipeline, and search results point
  at readable files.
- **Token-shaped.** `@fit/<budget>` returns the best view of a file within
  a token budget — the primitive agent context windows actually need, at
  the filesystem layer where every tool inherits it.
- **Local and read-only by construction.** Your originals are never
  modified, indexing never leaves your machine, and mounts are read-only —
  an agent cannot corrupt what it can only read.

## How it works

Mount a source. HearthFS indexes it in place (chunked full-text +
tree-sitter structure + optional local embeddings) and exposes it as a
POSIX-compatible filesystem. On macOS 26.5.2+ local folders mount natively
through Apple's **FSKit** — a first-class file system extension, no kext,
no macFUSE, no file server. Remote sources and older macOS use a pure-Rust
NFSv3 server that listens on 127.0.0.1 only. Linux support (FUSE) is
planned.

Magic paths give agents structured access to content:

```
project/@search/auth-flows          # semantic search across a directory
report.md@fit/2000                  # best view of a file within a token budget
notes.md@section/methodology        # extract a specific section
main.rs@outline                     # structural outline of a file
```

## Mount your second brains — and GitHub

HearthFS ships an **adapter registry**: every source type is configurable
from the macOS app's Volumes wizard *and* the CLI, which share one
implementation (`hearthfs adapters --json` is the registry; the UI renders
it and drives the same commands).

Supported today:

| adapter | what it mounts | search |
|---|---|---|
| `folder` | any local folder — docs, notes, code, email archives | HearthFS's own hybrid index |
| `obsidian` | [Obsidian](https://obsidian.md) vaults (auto-discovered from the app's vault registry) | HearthFS's own hybrid index |
| `logseq` | [Logseq](https://logseq.com) graph folders | HearthFS's own hybrid index |
| `gbrain` | [GBrain](https://github.com/garrytan/gbrain) brains — its markdown git repo is the system of record, so HearthFS indexes it directly | HearthFS's own hybrid index |
| `hearthloom` | [Hearthloom](https://github.com/metavert-io/hearthloom) personal knowledge graphs: BRAIN.md, entity dossiers, day-by-day recall, goals | delegated to the daemon (with `@fit` → server-side `?budget=` trimming) |
| `github` | GitHub repositories, read-only via the REST API | delegated to GitHub code search |
| `git` | **any** git repository (GitHub/GitLab/Bitbucket/self-hosted, public or private) — shallow-cloned locally, refreshed on mount, served offline when the remote is unreachable | HearthFS's own hybrid index (full semantic search over the code) |
| `mcp` | any **Model Context Protocol** server's resources (stdio command or Streamable HTTP URL) — one adapter, the whole MCP ecosystem | HearthFS's own hybrid index — **MCP servers gain semantic search they don't natively have** |
| `api` | anything that serves a HearthFS discovery manifest — the open integration point (Hearthloom is one of these; serve the manifest and you're mountable) | delegated to the service |

Cloud drives don't need adapters: if you run the Dropbox / Google Drive /
OneDrive sync client, your drive is already a local folder — mount it with
`folder`.

```bash
hearthfs volume add obsidian                          # vault path auto-discovered
hearthfs auth github                                  # paste a read-only PAT (stored 0600)
hearthfs volume add github --field repos=you/yourrepo
hearthfs volume mount yourrepo && hearthfs volume list
```

Volume configs persist in `~/.config/hearthfs/volumes.json`; secrets live
only in 0600 key files. Privacy is honored end-to-end: a mounted Hearthloom
brain always serves the vault-locked view — `private`-scoped material never
crosses the filesystem boundary.

## Using it with Claude Code (or any agent)

Point the agent at a mount — that's the whole integration:

```bash
hearthfs volume add folder --field path=~/research --name research
hearthfs volume mount research
cd ~/HearthFSmounts/research && claude
```

Then agents use their ordinary file tools:

- **Start broad:** read `@search/<question>` — it returns scored hits with
  token counts, snippets, and next-step hints (e.g. "read
  `notes/fusion.md@fit/1500` next").
- **Read within budget:** `<file>@fit/<tokens>` instead of whole-file reads.
- **For Hearthloom mounts:** `BRAIN.md` at the mount root is a ready-made
  "read this first" index for session start.

A one-line system-prompt hint ("this directory supports `@search/<query>`
and `<file>@fit/<tokens>` magic paths") is enough for agents to use them;
the agent eval in `scripts/eval/` briefs its agents the same way. The same mount
works for Cursor, Codex, LangChain file tools, or a plain shell script,
simultaneously — and Hearthloom itself consumes HearthFS mounts as
virtualized sources in the other direction.

## Features

- **Hybrid retrieval, fully local:** chunked BM25 (path-weighted) fused
  with local ONNX embeddings, indexed per chunk so long files rank by
  their best passage. Optional cloud embeddings (Voyage) are strictly
  opt-in.
- **Magic paths:** `@search`, `@fit/<budget>`, `@section`, `@outline`,
  `@tokens` — search and token-budgeted reads as filesystem semantics.
- **Adapter registry + volumes:** nine adapters today (local folders, four
  second brains, GitHub, any git repo, any MCP server, any manifest-serving
  API); add/mount/unmount via the macOS app's wizard or `hearthfs volume …`
  — same implementation, guaranteed parity.
- **Smart-source delegation:** API-backed sources (Hearthloom, GitHub)
  delegate search to their own engines and skip local re-indexing; local
  sources get HearthFS's own index.
- **Native macOS mounts (FSKit)** on macOS 26.5.2+; a loopback-only NFS
  server for remote sources and older macOS; Linux FUSE planned.
- **Read-only by design:** mutation ops return EROFS; originals untouched.
- **Structural awareness:** tree-sitter outlines/sections for 10+
  languages; `.eml`/`.ics`/`.vcf` indexed for email corpora.
- **Event log:** every magic read is recorded with tokens-saved accounting
  (`hearthfs stats`).

## Native macOS mounts — FSKit, not FUSE

Filesystem-level integrations on macOS have usually meant macFUSE (a
separate install, historically a kernel extension) or a file server
running on the same machine. HearthFS mounts local folders through
**FSKit**, Apple's first-party user-space filesystem framework (the same
mechanism Apple's own ExFAT/FAT32 drivers use on macOS 26). That buys real
properties, not plumbing trivia:

- **Local by construction.** Nothing listens on a socket: the FSKit mount
  is in-process XPC, so "your data stays on your machine" holds at the
  transport level, not just as policy. (For sources FSKit can't mount,
  HearthFS's NFS server listens on 127.0.0.1 only.)
- **No third-party kernel software.** No macFUSE install, no kext
  approval, no boot-security downgrade. The HearthFS app is a normal
  signed app with a File System Extension you toggle on in System
  Settings.
- **A first-class macOS citizen.** fskitd launches the extension on
  demand — no daemon to babysit, no mount that dies when a server process
  exits. Volumes appear under `/Volumes` in Finder's sidebar like any
  disk. The extension is sandboxed and reads your source folder only
  through a security-scoped grant: least-privilege by design.
- **Fast enough to forget about**: ~1 ms p95 cold reads through the
  mount — invisible next to the LLM latencies agents actually wait on
  (measured; see [docs/benchmarks.md](docs/benchmarks.md)).

Requires macOS 26.5.2+; on earlier macOS, use the NFS mount
(`hearthfs mount …`).

## Who it's for

- **Claude Code / Cowork users** who want agents to read large codebases and document sets without burning context windows
- **Tool builders** who want a filesystem-level API instead of wiring up vector databases
- **Anyone** running local AI workflows who wants semantic search without shipping data to a cloud

## What it isn't

HearthFS does not do code-specific intelligence (AST skeletons, symbol-aware editing, incremental re-reads). That space is well-served by [token-optimizer](https://github.com/alexgreensh/token-optimizer) and [Serena](https://github.com/oraios/serena). Use them alongside HearthFS for code-heavy Claude Code workflows.

## Install

HearthFS is not on crates.io yet. Install the CLI from a clone (needs a
Rust toolchain):

```bash
git clone https://github.com/metavert-io/hearthfs
cd hearthfs
cargo install --path crates/hearthfs-cli   # installs the `hearthfs` binary
```

The macOS app (FSKit mounts and the Volumes window) builds from `fskit/`;
see [FSKit backend](#fskit-backend-macos-2652) below.

## Quick start

```bash
hearthfs mount ~/documents ~/mnt/docs &   # serves until stopped; indexing runs in the background
cat ~/mnt/docs/@search/authentication     # ranked hits with snippets and token counts
cat ~/mnt/docs/spec.md@fit/2000           # the best view of a file within 2,000 tokens
grep -r "authentication" ~/mnt/docs       # ordinary tools work too
hearthfs status                           # indexes and file counts
kill %1                                   # stop the server; it unmounts first
```

## Performance

Measured on an Apple-silicon Mac; the method and full tables are in
[docs/benchmarks.md](docs/benchmarks.md).

- **Cold reads through the FSKit mount: ~1 ms at p95** (over NFS ~0.5 ms;
  reading the folder directly ~0.03 ms). Warm re-reads and directory walks
  are faster over NFS, which gets kernel caching.
- **Indexing** runs in the background while the mount already serves
  files: full-text indexing of a ~19k-file corpus takes a few minutes, and
  local embeddings for the same corpus about 2.5 hours across all cores.

How well search finds the right files depends on your corpus. The
retrieval eval measures it on corpora you supply: hit@k and MRR for free,
and tokens per correct answer for a Claude agent working over the raw
folder versus the mount. See [docs/retrieval-eval.md](docs/retrieval-eval.md).

## Related projects

- [Hearthloom](https://github.com/metavert-io/hearthloom): a local-first
  personal knowledge graph. HearthFS mounts it with the `hearthloom`
  adapter, and Hearthloom can use HearthFS mounts as sources.
- [Serena](https://github.com/oraios/serena) and
  [token-optimizer](https://github.com/alexgreensh/token-optimizer):
  code-specific agent tooling that pairs well with HearthFS (see "What it
  isn't").
- [Apple FSKit](https://developer.apple.com/documentation/fskit): the
  framework behind HearthFS's native macOS mounts.

## License

MIT © Metavert LLC. See [LICENSE](LICENSE).

## Building HearthFS

```bash
cargo build --release            # CLI + NFS server
```

Optional semantic search uses a local embedding model,
[all-MiniLM-L6-v2](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2)
(Apache-2.0). Fetch it once:

```bash
./scripts/fetch-model.sh           # into ~/.hearthfs/models/ (pinned revision, SHA-256 checked)
./scripts/fetch-model.sh models    # or into ./models/ for a repo-local copy
```

Without it, search is full-text only. Cloud embeddings are opt-in:
`HEARTHFS_EMBEDDER=voyage:voyage-4` with a key in `$VOYAGE_API_KEY` or
`~/.config/hearthfs/voyage.key` (this sends file text to Voyage).

Third-party licences, including the C libraries compiled in (SQLite,
Oniguruma, tree-sitter): [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

### FSKit backend (macOS 26.5.2+)

The kext-free FSKit mount lives in `fskit/` (XcodeGen project wrapping a
Rust FFI staticlib). **Requires macOS 26.5.2 or later** — earlier 26.3.x
releases have a platform bug that hangs the mount (the extension never
launches); 26.5.2 fixes it. Apple makes this path peculiar; the steps that
are NOT optional:

1. One-time signing setup: copy `fskit/Local.xcconfig.example` to
   `fskit/Local.xcconfig` (git ignores it) and set your team ID and a bundle
   ID prefix you control. Then run `xcodegen` in `fskit/`, open the
   generated `HearthFS.xcodeproj` in Xcode, and on the HearthFSExtension
   target use "+ Capability" to add **FSKit Module** — without this,
   automatic signing silently strips the entitlement.
2. One-time: your team's Account Holder must have accepted the current
   Apple Developer Program License Agreement (developer.apple.com), or
   profile creation fails with "PLA Update available".
3. `./fskit/build.sh --install` — builds the arm64 staticlib + app
   (automatic signing, provisioning flags), evicts the stale-registration
   trap, installs to /Applications, and prints the reboot/re-toggle steps.
   (`./fskit/build.sh` without the flag builds without touching the
   installed app.)
4. After reboot, enable the extension (System Settings > General > Login
   Items & Extensions > File System Extensions — use the **"By Category"**
   toggle); the app's onboarding checklist tracks this and mounts a folder.
5. Mount from the app, or: `mount -F -t hearthfs <source-dir> <mountpoint>`.
   Mounts default under `/Volumes` (the macOS standard). Once mounted,
   browse it like any folder, plus magic paths: `@search/<query>`,
   `<file>@fit/<budget>`, `<file>@outline`, `<file>@section/<name>`.

Read access requires `FSRequiresSecurityScopedPathURLResources: true` in the
extension's Info.plist (already set) — without it the sandboxed extension is
denied reads and listings come back empty.

**Rebuild caveat:** re-signing the appex invalidates its Settings enablement,
so every `--install` needs a reboot + re-toggle (build.sh handles the
stale-registration eviction and prints the steps). Batch changes to minimize
installs. Details and troubleshooting: `fskit/README.md`.
