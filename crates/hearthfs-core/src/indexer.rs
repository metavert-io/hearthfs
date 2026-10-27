use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::embeddings::Embedder;
use crate::fts::FtsIndex;
use crate::source::Source;
use crate::vector_store::VectorStore;

/// Embedding chunk geometry: one probe per ~1500-char window, capped per
/// file. Each probe is truncated to the backend's usable context
/// (`Embedder::probe_chars`); the cap bounds indexing time and store size.
const EMBED_CHUNK_BYTES: usize = 1500;
const MAX_EMBED_CHUNKS: usize = 48;

/// Background indexer that scans a source and populates the FTS and vector indexes.
pub struct Indexer {
    source: Arc<dyn Source>,
    fts: Arc<FtsIndex>,
    embedder: Option<Arc<Embedder>>,
    vectors: Option<Arc<VectorStore>>,
}

impl Indexer {
    pub fn new(source: Arc<dyn Source>, fts: Arc<FtsIndex>) -> Self {
        Self {
            source,
            fts,
            embedder: None,
            vectors: None,
        }
    }

    pub fn with_embeddings(mut self, embedder: Arc<Embedder>, vectors: Arc<VectorStore>) -> Self {
        self.embedder = Some(embedder);
        self.vectors = Some(vectors);
        self
    }

    /// Perform a full scan of the source and index all text files.
    /// Skips entirely for smart sources that own their own search.
    pub async fn full_scan(&self) -> Result<u64> {
        if self.source.skip_indexing() {
            info!("skipping indexing (smart source handles its own search)");
            return Ok(0);
        }

        let mut count = 0u64;
        let mut queue: Vec<PathBuf> = vec![PathBuf::from("")];

        while let Some(dir) = queue.pop() {
            let entries = match self.source.list(&dir).await {
                Ok(e) => e,
                Err(e) => {
                    warn!("failed to list {}: {e}", dir.display());
                    continue;
                }
            };

            for entry in entries {
                let child = if dir.as_os_str().is_empty() {
                    PathBuf::from(&entry.name)
                } else {
                    dir.join(&entry.name)
                };

                if entry.is_dir {
                    if should_skip_dir(&entry.name) {
                        debug!("skipping directory: {}", child.display());
                        continue;
                    }
                    queue.push(child);
                    continue;
                }

                if !is_text_file(&entry.name) {
                    if is_env_file(&entry.name) {
                        // Older versions indexed .env files; drop any
                        // entry an existing index cache still holds.
                        self.purge(&child);
                    }
                    debug!("skipping non-text file: {}", child.display());
                    continue;
                }

                match self.index_file(&child).await {
                    Ok(true) => count += 1,
                    Ok(false) => {} // already current
                    Err(e) => warn!("failed to index {}: {e}", child.display()),
                }
            }
        }

        info!("indexed {count} files");
        Ok(count)
    }

    /// Remove a file from the FTS and vector indexes (best effort).
    fn purge(&self, path: &Path) {
        let path_str = path.to_string_lossy();
        if let Err(e) = self.fts.remove_file(&path_str) {
            warn!("failed to drop {} from the index: {e}", path.display());
        }
        if let Some(vectors) = &self.vectors {
            if let Err(e) = vectors.remove_file(&path_str) {
                warn!(
                    "failed to drop {} from the vector index: {e}",
                    path.display()
                );
            }
        }
    }

    /// Index a single file. Returns true if it was (re)indexed, false if already current.
    async fn index_file(&self, path: &Path) -> Result<bool> {
        let data = self.source.read(path).await?;
        let content_hash = simple_hash(&data);
        let path_str = path.to_string_lossy();

        let fts_current = self.fts.is_current(&path_str, &content_hash)?;
        let vectors_stale = match (&self.embedder, &self.vectors) {
            (Some(_), Some(vectors)) => !vectors.is_current(&path_str, &content_hash)?,
            _ => false,
        };
        if fts_current && !vectors_stale {
            return Ok(false);
        }

        let text = String::from_utf8_lossy(&data);
        if !fts_current {
            self.fts.index_file(&path_str, &text, &content_hash)?;
        }

        // Embed a probe per chunk so long files are represented past their
        // first lines. The model truncates at ~128 wordpiece tokens, so the
        // probe is the chunk's head; the cap bounds indexing time and store
        // size for very large files. Probes embed in parallel — inference is
        // CPU-bound and Embedder::embed is safe to call concurrently.
        if vectors_stale {
            if let (Some(embedder), Some(vectors)) = (&self.embedder, &self.vectors) {
                vectors.remove_file(&path_str)?;
                let probe_chars = embedder.probe_chars();
                let probes: Vec<String> = crate::chunking::chunk_text(&text, EMBED_CHUNK_BYTES)
                    .iter()
                    .take(MAX_EMBED_CHUNKS)
                    .map(|chunk| chunk.chars().take(probe_chars).collect())
                    .collect();
                for (idx, result) in embedder.embed_batch(&probes).into_iter().enumerate() {
                    match result {
                        Ok(vec) => {
                            vectors.upsert_chunk(&path_str, idx, &vec, &content_hash)?;
                        }
                        Err(e) => {
                            debug!("embedding failed for {} chunk {idx}: {e}", path.display());
                        }
                    }
                }
            }
        }

        debug!("indexed {}", path.display());
        Ok(true)
    }
}

/// Directories to skip during indexing.
fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | ".hg"
            | ".svn"
            | "__pycache__"
            | ".tox"
            | ".mypy_cache"
            | ".pytest_cache"
            | "dist"
            | "build"
            | ".next"
            | ".nuxt"
            | "vendor"
            | ".venv"
            | "venv"
            | ".cargo"
    )
}

/// Quick content hash (not cryptographic, just for change detection).
fn simple_hash(data: &[u8]) -> String {
    // FNV-1a 64-bit
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// `.env`, `.env.local`, `prod.env` and the like: credential files that are
/// never indexed. `.env.example` is a template, not a secret.
fn is_env_file(name: &str) -> bool {
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    base != ".env.example"
        && (base == ".env" || base.starts_with(".env.") || base.ends_with(".env"))
}

/// Heuristic: is this filename likely a text file we should index?
///
/// `.env` files (and `*.env`) are deliberately excluded: they hold
/// credentials, and indexing them would put secrets into `@search`
/// snippets. Templates such as `.env.example` are indexed.
fn is_text_file(name: &str) -> bool {
    let text_extensions = [
        "txt",
        "md",
        "rs",
        "py",
        "js",
        "ts",
        "tsx",
        "jsx",
        "go",
        "java",
        "c",
        "h",
        "cpp",
        "hpp",
        "cc",
        "hh",
        "rb",
        "sh",
        "bash",
        "zsh",
        "fish",
        "toml",
        "yaml",
        "yml",
        "json",
        "xml",
        "html",
        "css",
        "scss",
        "less",
        "sql",
        "r",
        "R",
        "swift",
        "kt",
        "kts",
        "scala",
        "clj",
        "ex",
        "exs",
        "erl",
        "hs",
        "ml",
        "mli",
        "lua",
        "vim",
        "el",
        "lisp",
        "scm",
        "rkt",
        "dockerfile",
        "makefile",
        "cmake",
        "gradle",
        "conf",
        "cfg",
        "ini",
        "csv",
        "tsv",
        "log",
        "diff",
        "patch",
        "tex",
        "bib",
        "rst",
        "adoc",
        "org",
        "eml",
        "ics",
        "vcf",
    ];

    // Check extension
    if let Some(ext) = Path::new(name).extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        if text_extensions.contains(&ext.as_str()) {
            return true;
        }
    }

    // Check known extensionless files
    let basename = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        basename.as_str(),
        "makefile"
            | "dockerfile"
            | "readme"
            | "license"
            | "changelog"
            | "contributing"
            | "authors"
            | "todo"
            | "gemfile"
            | "rakefile"
            | "procfile"
            | "vagrantfile"
            | ".gitignore"
            | ".gitattributes"
            | ".editorconfig"
            | ".env.example"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_text_file() {
        assert!(is_text_file("main.rs"));
        assert!(is_text_file("README.md"));
        assert!(is_text_file("Makefile"));
        assert!(is_text_file("data.json"));
        assert!(is_text_file("confirmation.eml"));
        assert!(is_text_file("meeting.ics"));
        assert!(!is_text_file("photo.png"));
        assert!(!is_text_file("archive.zip"));
        assert!(!is_text_file("binary.exe"));
    }

    #[test]
    fn env_files_are_not_indexed() {
        for secret in [
            ".env",
            "app/.env",
            ".env.local",
            ".env.production",
            "prod.env",
        ] {
            assert!(!is_text_file(secret), "{secret} must not be indexed");
            assert!(is_env_file(secret), "{secret} is an env file");
        }
        assert!(is_text_file(".env.example"));
        assert!(is_text_file("app/.env.example"));
        assert!(!is_env_file(".env.example"));
        assert!(!is_env_file("environment.md"));
    }

    /// Flat in-memory source for indexer tests.
    struct MemSource(Vec<(&'static str, &'static str)>);

    #[async_trait::async_trait]
    impl Source for MemSource {
        async fn list(&self, path: &Path) -> Result<Vec<crate::source::SourceEntry>> {
            assert!(path.as_os_str().is_empty(), "MemSource is flat");
            Ok(self
                .0
                .iter()
                .map(|(name, _)| crate::source::SourceEntry {
                    name: name.to_string(),
                    is_dir: false,
                })
                .collect())
        }
        async fn read(&self, path: &Path) -> Result<bytes::Bytes> {
            self.0
                .iter()
                .find(|(name, _)| Path::new(name) == path)
                .map(|(_, body)| bytes::Bytes::from_static(body.as_bytes()))
                .ok_or_else(|| anyhow::anyhow!("not found: {}", path.display()))
        }
        async fn stat(&self, _path: &Path) -> Result<crate::source::SourceMeta> {
            anyhow::bail!("unused")
        }
        async fn watch(
            &self,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = crate::source::ChangeEvent> + Send>>>
        {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[tokio::test]
    async fn full_scan_skips_env_files_and_purges_old_entries() {
        let dir = std::env::temp_dir().join(format!("hearthfs-env-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fts = Arc::new(FtsIndex::open(&dir.join("fts.db")).unwrap());
        // An index cache built by an older version still holds the secret.
        fts.index_file(".env", "API_TOKEN=hunter2secret", "stale")
            .unwrap();
        assert_eq!(fts.search("hunter2secret", 10).unwrap().len(), 1);

        let source = Arc::new(MemSource(vec![
            (".env", "API_TOKEN=hunter2secret"),
            (".env.example", "API_TOKEN=changeme"),
            ("notes.md", "The board approved six rain barrels."),
        ]));
        let indexed = Indexer::new(source, fts.clone()).full_scan().await.unwrap();

        assert_eq!(indexed, 2, ".env.example and notes.md only");
        assert!(fts.search("hunter2secret", 10).unwrap().is_empty());
        assert_eq!(fts.search("changeme", 10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_simple_hash() {
        let h1 = simple_hash(b"hello");
        let h2 = simple_hash(b"hello");
        let h3 = simple_hash(b"world");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }
}
