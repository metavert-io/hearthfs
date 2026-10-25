# Security policy

## Reporting a vulnerability

Please report security problems privately, through GitHub: open this
repository's **Security** tab and choose **Report a vulnerability**. That
creates a private advisory that only the maintainers can see. Please do not
open a public issue, pull request or discussion about a vulnerability.

A useful report says what you found, how to reproduce it, which version or
commit you tested, and what an attacker could do with it. Fixes land on
`main` and ship in the next release; older releases are not patched
separately.

## What HearthFS protects, and how

- **Mounts are read-only.** Every mutation returns `EROFS`; the source files
  are never modified.
- **Nothing is served to the network.** The NFS server binds `127.0.0.1`
  and the kernel mounts it through `127.0.0.1`, so other machines cannot
  connect to it. The FSKit transport (macOS 26.5.2+) uses no socket at all.
- **The FSKit extension is sandboxed.** It reads the source folder only
  through the security-scoped URL that FSKit hands it.
- **Keys stay out of configs and command lines.** `hearthfs auth <name>`
  reads a key from stdin and stores it at `~/.config/hearthfs/<name>.key`
  with mode 0600. Volumes refer to keys by name, so keys never enter
  `volumes.json` or the arguments of the server process a volume starts.
- **`.env` files are not indexed**, so their contents never show up in
  `@search` results. A mount still shows every file in its source folder,
  though: mount only folders whose contents you are happy for your agents
  to read.
- **Embeddings are local by default.** The optional Voyage backend
  (`HEARTHFS_EMBEDDER=voyage:<model>`) sends file text to Voyage's API;
  without it, indexing and search never leave the machine.
