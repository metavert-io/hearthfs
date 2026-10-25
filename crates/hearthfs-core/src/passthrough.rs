use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use anyhow::{anyhow, Result};

use crate::embeddings::Embedder;
use crate::event_log::EventLog;
use crate::fts::FtsIndex;
use crate::inode_table::InodeTable;
use crate::magic::{self, MagicPath};
use crate::source::{Source, SourceMeta};
use crate::vector_store::VectorStore;
use crate::tokens;

/// Metadata cached per inode, suitable for NFS getattr.
#[derive(Debug, Clone)]
pub struct EntryMeta {
    pub inode: u64,
    pub size: u64,
    pub is_dir: bool,
    pub modified: SystemTime,
    pub created: SystemTime,
}

/// Read-only passthrough VFS backed by a Source and an inode table.
///
/// Intercepts magic paths (`file@tokens`, `@search/query`, etc.) and returns
/// virtual content. All other operations pass through to the source.
pub struct PassthroughVfs {
    source: Arc<dyn Source>,
    pub inodes: InodeTable,
    fts: Option<Arc<FtsIndex>>,
    embedder: Option<Arc<Embedder>>,
    vectors: Option<Arc<VectorStore>>,
    event_log: Option<Arc<EventLog>>,
    virtual_files: RwLock<HashMap<u64, Vec<u8>>>,
    search_dirs: RwLock<HashMap<u64, PathBuf>>,
    section_dirs: RwLock<HashMap<u64, (PathBuf, String)>>,
    fit_dirs: RwLock<HashMap<u64, (PathBuf, String)>>,
}

impl PassthroughVfs {
    pub fn new(source: Arc<dyn Source>) -> Self {
        Self {
            source,
            inodes: InodeTable::new(),
            fts: None,
            embedder: None,
            vectors: None,
            event_log: None,
            virtual_files: RwLock::new(HashMap::new()),
            search_dirs: RwLock::new(HashMap::new()),
            section_dirs: RwLock::new(HashMap::new()),
            fit_dirs: RwLock::new(HashMap::new()),
        }
    }

    pub fn with_fts(mut self, fts: Arc<FtsIndex>) -> Self {
        self.fts = Some(fts);
        self
    }

    pub fn with_embeddings(mut self, embedder: Arc<Embedder>, vectors: Arc<VectorStore>) -> Self {
        self.embedder = Some(embedder);
        self.vectors = Some(vectors);
        self
    }

    pub fn with_event_log(mut self, log: Arc<EventLog>) -> Self {
        self.event_log = Some(log);
        self
    }

    fn log_event(
        &self,
        category: &str,
        event_type: &str,
        path: Option<&str>,
        tokens_saved: Option<i64>,
        duration_ms: Option<i64>,
    ) {
        if let Some(log) = &self.event_log {
            let _ = log.record(category, event_type, path, None, tokens_saved, duration_ms);
        }
    }

    /// Get metadata for an inode.
    pub async fn getattr(&self, inode: u64) -> Result<EntryMeta> {
        // Virtual file?
        if let Some(content) = self.virtual_files.read().unwrap().get(&inode) {
            return Ok(EntryMeta {
                inode,
                size: content.len() as u64,
                is_dir: false,
                modified: SystemTime::now(),
                created: SystemTime::now(),
            });
        }

        // Virtual directory? (search, section, or fit)
        if self.search_dirs.read().unwrap().contains_key(&inode)
            || self.section_dirs.read().unwrap().contains_key(&inode)
            || self.fit_dirs.read().unwrap().contains_key(&inode)
        {
            return Ok(EntryMeta {
                inode,
                size: 0,
                is_dir: true,
                modified: SystemTime::now(),
                created: SystemTime::now(),
            });
        }

        let path = self
            .inodes
            .get_path(inode)
            .ok_or_else(|| anyhow!("unknown inode {inode}"))?;
        let meta = self.source.stat(&path).await?;
        Ok(entry_meta(inode, &meta))
    }

    /// Lookup a child by name in a parent directory, returning its inode + metadata.
    pub async fn lookup(&self, parent_inode: u64, name: &str) -> Result<EntryMeta> {
        // If the parent is a search directory, the child name IS the query
        let search_parent = self
            .search_dirs
            .read()
            .unwrap()
            .get(&parent_inode)
            .cloned();
        if let Some(search_parent) = search_parent {
            let content = self.generate_search(&search_parent, name).await?;
            return Ok(self.register_virtual_file(
                &format!("__search__/{}/{name}", search_parent.display()),
                &content,
            ));
        }

        // If the parent is a section directory, the child name IS the section name
        let section_info = self
            .section_dirs
            .read()
            .unwrap()
            .get(&parent_inode)
            .cloned();
        if let Some((parent_dir, filename)) = section_info {
            let content = self.generate_section(&parent_dir, &filename, name).await?;
            return Ok(self.register_virtual_file(
                &format!("__section__/{}/{filename}/{name}", parent_dir.display()),
                &content,
            ));
        }

        // If the parent is a fit directory, the child name IS the token budget
        let fit_info = self.fit_dirs.read().unwrap().get(&parent_inode).cloned();
        if let Some((parent_dir, filename)) = fit_info {
            let content = self.generate_fit(&parent_dir, &filename, name).await?;
            return Ok(self.register_virtual_file(
                &format!("__fit__/{}/{filename}/{name}", parent_dir.display()),
                &content,
            ));
        }

        let parent_path = self
            .inodes
            .get_path(parent_inode)
            .ok_or_else(|| anyhow!("unknown inode {parent_inode}"))?;

        // Check for magic path
        match magic::parse(name) {
            MagicPath::Plain(_) => {
                let child_path = if parent_path.as_os_str().is_empty() {
                    name.into()
                } else {
                    parent_path.join(name)
                };
                let meta = self.source.stat(&child_path).await?;
                let child_inode = self.inodes.get_or_insert(&child_path);
                Ok(entry_meta(child_inode, &meta))
            }
            MagicPath::Tokens(real_name) => {
                let content = self.generate_tokens(&parent_path, real_name).await?;
                Ok(self.register_virtual_file(
                    &format!("__virtual__/{}", name),
                    &content,
                ))
            }
            MagicPath::Search(_query) => {
                // `@search` → return a virtual directory
                // The actual query comes as a child lookup inside this dir
                Ok(self.register_search_dir(&parent_path, name))
            }
            MagicPath::Outline(real_name) => {
                let content = self.generate_outline(&parent_path, real_name).await?;
                Ok(self.register_virtual_file(
                    &format!("__virtual__/{}", name),
                    &content,
                ))
            }
            MagicPath::Section(real_name, _section_name) => {
                // `file@section` → return a virtual directory
                // The actual section name comes as a child lookup inside this dir
                Ok(self.register_section_dir(&parent_path, real_name, name))
            }
            MagicPath::Fit(real_name, _budget) => {
                // `file@fit` → return a virtual directory
                // The token budget comes as a child lookup inside this dir
                Ok(self.register_fit_dir(&parent_path, real_name, name))
            }
            MagicPath::Summary(_) => {
                let content = b"@summary not yet implemented (requires LLM).\n".to_vec();
                Ok(self.register_virtual_file(
                    &format!("__virtual__/{}", name),
                    &content,
                ))
            }
        }
    }

    /// Read file contents.
    pub async fn read(&self, inode: u64, offset: u64, count: u32) -> Result<(Vec<u8>, bool)> {
        let start_time = std::time::Instant::now();

        // Virtual files (magic path results)
        if let Some(content) = self.virtual_files.read().unwrap().get(&inode) {
            let start = (offset as usize).min(content.len());
            let end = (start + count as usize).min(content.len());
            let eof = end >= content.len();
            // Log magic reads only on first chunk (offset 0)
            if offset == 0 {
                let path = self.inodes.get_path(inode);
                let path_str = path.as_ref().map(|p| p.to_string_lossy().to_string());
                let raw_size = content.len() as i64;
                let tokens_saved = tokens::estimate_tokens(content) as i64;
                self.log_event(
                    "fs",
                    "magic_read",
                    path_str.as_deref(),
                    Some(tokens_saved),
                    Some(start_time.elapsed().as_millis() as i64),
                );
                let _ = raw_size; // used for logging context
            }
            return Ok((content[start..end].to_vec(), eof));
        }

        let path = self
            .inodes
            .get_path(inode)
            .ok_or_else(|| anyhow!("unknown inode {inode}"))?;
        let data = self.source.read(&path).await?;
        let start = (offset as usize).min(data.len());
        let end = (start + count as usize).min(data.len());
        let eof = end >= data.len();

        // Log file reads only on first chunk
        if offset == 0 {
            self.log_event(
                "fs",
                "read",
                Some(&path.to_string_lossy()),
                None,
                Some(start_time.elapsed().as_millis() as i64),
            );
        }

        Ok((data[start..end].to_vec(), eof))
    }

    /// List directory entries, returning (name, inode, is_dir) tuples.
    pub async fn readdir(&self, inode: u64) -> Result<Vec<(String, u64, bool)>> {
        // Search, section, and fit directories are empty (queries happen via lookup)
        if self.search_dirs.read().unwrap().contains_key(&inode)
            || self.section_dirs.read().unwrap().contains_key(&inode)
            || self.fit_dirs.read().unwrap().contains_key(&inode)
        {
            return Ok(vec![]);
        }

        let dir_path = self
            .inodes
            .get_path(inode)
            .ok_or_else(|| anyhow!("unknown inode {inode}"))?;
        let entries = self.source.list(&dir_path).await?;

        let mut result = Vec::with_capacity(entries.len());
        for entry in entries {
            let child_path = if dir_path.as_os_str().is_empty() {
                entry.name.clone().into()
            } else {
                dir_path.join(&entry.name)
            };
            let child_inode = self.inodes.get_or_insert(&child_path);
            result.push((entry.name, child_inode, entry.is_dir));
        }
        Ok(result)
    }

    // --- Magic path generators ---

    async fn generate_tokens(&self, parent: &PathBuf, real_name: &str) -> Result<Vec<u8>> {
        let file_path = if parent.as_os_str().is_empty() {
            real_name.into()
        } else {
            parent.join(real_name)
        };
        let data = self.source.read(&file_path).await?;
        let count = tokens::estimate_tokens(&data);
        Ok(format!("{count}\n").into_bytes())
    }

    async fn generate_outline(&self, parent: &PathBuf, real_name: &str) -> Result<Vec<u8>> {
        let file_path = if parent.as_os_str().is_empty() {
            real_name.into()
        } else {
            parent.join(real_name)
        };
        let data = self.source.read(&file_path).await?;
        let outline = crate::structure::format_outline(real_name, &data);
        Ok(outline.into_bytes())
    }

    async fn generate_section(
        &self,
        parent: &PathBuf,
        real_name: &str,
        section_name: &str,
    ) -> Result<Vec<u8>> {
        let file_path = if parent.as_os_str().is_empty() {
            real_name.into()
        } else {
            parent.join(real_name)
        };
        let data = self.source.read(&file_path).await?;
        crate::structure::extract_section(real_name, &data, section_name)
            .ok_or_else(|| anyhow!("section '{}' not found in {}", section_name, real_name))
    }

    async fn generate_fit(
        &self,
        parent: &PathBuf,
        real_name: &str,
        budget_str: &str,
    ) -> Result<Vec<u8>> {
        let budget: u64 = budget_str
            .parse()
            .map_err(|_| anyhow!("invalid token budget '{budget_str}' (usage: file@fit/2000)"))?;
        let file_path = if parent.as_os_str().is_empty() {
            real_name.into()
        } else {
            parent.join(real_name)
        };
        // Smart sources can shape to the budget server-side (e.g. hearthloom's
        // ?budget= param) — no full fetch, no local fitting, and the origin
        // can trim on semantic boundaries we can't see from here.
        if let Some(fitted) = self.source.read_fit(&file_path, budget).await? {
            self.log_event(
                "fs",
                "fit_read",
                Some(&file_path.to_string_lossy()),
                None, // tokens saved unknown — the full content was never fetched
                None,
            );
            return Ok(fitted.to_vec());
        }
        let data = self.source.read(&file_path).await?;
        let raw_tokens = tokens::estimate_tokens(&data) as i64;
        let fitted = crate::structure::fit(real_name, &data, budget);
        let fitted_tokens = tokens::estimate_tokens(fitted.as_bytes()) as i64;
        self.log_event(
            "fs",
            "fit_read",
            Some(&file_path.to_string_lossy()),
            Some((raw_tokens - fitted_tokens).max(0)),
            None,
        );
        Ok(fitted.into_bytes())
    }

    async fn generate_search(&self, parent: &PathBuf, query: &str) -> Result<Vec<u8>> {
        if query.is_empty() {
            return Ok(b"Usage: cat @search/<query>\n".to_vec());
        }

        // Delegate to smart source if it handles search
        if self.source.supports_search() {
            let results = self.source.search(query, 20).await?;
            let mut output = String::new();
            output.push_str(&format!("# Search: {query}\n\n"));
            if results.is_empty() {
                output.push_str("No results.\n");
            } else {
                for result in &results {
                    output.push_str(&format!(
                        "- **{}** (score: {:.2})\n  {}\n\n",
                        result.title, result.score, result.snippet
                    ));
                }
            }
            return Ok(output.into_bytes());
        }

        // Local search: FTS + vector
        let fts = self
            .fts
            .as_ref()
            .ok_or_else(|| anyhow!("search not available: index not initialized"))?;

        let prefix = if parent.as_os_str().is_empty() {
            String::new()
        } else {
            format!("{}/", parent.display())
        };

        let fts_results = fts.search(query, 20)?;

        let semantic_results =
            if let (Some(embedder), Some(vectors)) = (&self.embedder, &self.vectors) {
                match embedder.embed_query(query) {
                    Ok(query_vec) => vectors.search(&query_vec, 10).unwrap_or_default(),
                    Err(_) => vec![],
                }
            } else {
                vec![]
            };

        // Fuse both lists with weighted reciprocal-rank fusion. The vector
        // leg's weight depends on the backend: small local embedders are a
        // recall backfill (0.05), strong API embeddings earn a real weight
        // (Embedder::fusion_weight; measured with docs/retrieval-eval.md).
        const RRF_K: f64 = 60.0;
        let vec_weight: f64 = self
            .embedder
            .as_ref()
            .map(|e| e.fusion_weight())
            .unwrap_or(0.05);
        let mut fused: Vec<(String, f64, Option<String>)> = Vec::new();
        {
            let mut bump = |path: &str, rank: usize, weight: f64, snippet: Option<&str>| {
                let score = weight / (RRF_K + rank as f64 + 1.0);
                if let Some(entry) = fused.iter_mut().find(|(p, _, _)| p == path) {
                    entry.1 += score;
                    if entry.2.is_none() {
                        entry.2 = snippet.map(str::to_string);
                    }
                } else {
                    fused.push((path.to_string(), score, snippet.map(str::to_string)));
                }
            };
            for (rank, r) in fts_results.iter().enumerate() {
                bump(&r.path, rank, 1.0, Some(&r.snippet));
            }
            for (rank, r) in semantic_results.iter().enumerate() {
                bump(&r.path, rank, vec_weight, None);
            }
        }
        fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let mut output = String::new();
        output.push_str(&format!("# Search: {query}\n\n"));

        let mut count = 0;
        for (path, score, snippet) in &fused {
            if !prefix.is_empty() && !path.starts_with(&prefix) {
                continue;
            }
            count += 1;
            let tokens_note = match fts.token_estimate(path) {
                Ok(Some(t)) => format!(", ~{t} tokens"),
                _ => String::new(),
            };
            output.push_str(&format!("- **{path}** (score: {score:.4}{tokens_note})\n"));
            if let Some(snippet) = snippet {
                output.push_str(&format!("  {snippet}\n"));
            }
            output.push_str(&format!(
                "  next: cat '{path}@outline' | '{path}@fit/2000' | '{path}@section/<name>'\n\n"
            ));
        }

        if count == 0 {
            output.push_str("No results.\n");
        }
        Ok(output.into_bytes())
    }

    /// Register virtual file content and return its metadata.
    fn register_virtual_file(&self, key: &str, content: &[u8]) -> EntryMeta {
        let virtual_path: PathBuf = key.into();
        let inode = self.inodes.get_or_insert(&virtual_path);
        self.virtual_files
            .write()
            .unwrap()
            .insert(inode, content.to_vec());
        EntryMeta {
            inode,
            size: content.len() as u64,
            is_dir: false,
            modified: SystemTime::now(),
            created: SystemTime::now(),
        }
    }

    /// Register a virtual search directory and return its metadata.
    fn register_search_dir(&self, real_parent: &PathBuf, name: &str) -> EntryMeta {
        let virtual_path: PathBuf =
            format!("__searchdir__/{}/{name}", real_parent.display()).into();
        let inode = self.inodes.get_or_insert(&virtual_path);
        self.search_dirs
            .write()
            .unwrap()
            .insert(inode, real_parent.clone());
        EntryMeta {
            inode,
            size: 0,
            is_dir: true,
            modified: SystemTime::now(),
            created: SystemTime::now(),
        }
    }

    /// Register a virtual section directory and return its metadata.
    fn register_section_dir(
        &self,
        real_parent: &PathBuf,
        real_filename: &str,
        name: &str,
    ) -> EntryMeta {
        let virtual_path: PathBuf =
            format!("__sectiondir__/{}/{name}", real_parent.display()).into();
        let inode = self.inodes.get_or_insert(&virtual_path);
        self.section_dirs
            .write()
            .unwrap()
            .insert(inode, (real_parent.clone(), real_filename.to_string()));
        EntryMeta {
            inode,
            size: 0,
            is_dir: true,
            modified: SystemTime::now(),
            created: SystemTime::now(),
        }
    }

    /// Register a virtual fit directory and return its metadata.
    fn register_fit_dir(
        &self,
        real_parent: &PathBuf,
        real_filename: &str,
        name: &str,
    ) -> EntryMeta {
        let virtual_path: PathBuf = format!("__fitdir__/{}/{name}", real_parent.display()).into();
        let inode = self.inodes.get_or_insert(&virtual_path);
        self.fit_dirs
            .write()
            .unwrap()
            .insert(inode, (real_parent.clone(), real_filename.to_string()));
        EntryMeta {
            inode,
            size: 0,
            is_dir: true,
            modified: SystemTime::now(),
            created: SystemTime::now(),
        }
    }
}

fn entry_meta(inode: u64, meta: &SourceMeta) -> EntryMeta {
    EntryMeta {
        inode,
        size: meta.size,
        is_dir: meta.is_dir,
        modified: meta.modified,
        created: meta.created,
    }
}
