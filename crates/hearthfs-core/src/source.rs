use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;

/// A change event from a watched source.
#[derive(Debug, Clone)]
pub enum ChangeEvent {
    Created(Box<Path>),
    Modified(Box<Path>),
    Removed(Box<Path>),
}

/// Metadata for a source entry.
#[derive(Debug, Clone)]
pub struct SourceMeta {
    pub size: u64,
    pub is_dir: bool,
    pub modified: SystemTime,
    pub created: SystemTime,
}

/// A directory listing entry.
#[derive(Debug, Clone)]
pub struct SourceEntry {
    pub name: String,
    pub is_dir: bool,
}

/// A search result returned by a smart source.
#[derive(Debug, Clone)]
pub struct SourceSearchResult {
    pub path: PathBuf,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

/// Pluggable backend for raw content access.
///
/// Local sources (FsSource) implement the core four methods.
/// Smart sources (ApiSource) additionally implement search and skip indexing.
#[async_trait]
pub trait Source: Send + Sync {
    /// List entries in a directory.
    async fn list(&self, path: &Path) -> Result<Vec<SourceEntry>>;

    /// Read file content.
    async fn read(&self, path: &Path) -> Result<Bytes>;

    /// Read a budget-shaped view of a file, if the source can produce one
    /// server-side (smart sources with a budget query param). `Ok(None)`
    /// means unsupported here — the caller falls back to reading the full
    /// content and fitting locally.
    async fn read_fit(&self, _path: &Path, _budget: u64) -> Result<Option<Bytes>> {
        Ok(None)
    }

    /// Get metadata for a path.
    async fn stat(&self, path: &Path) -> Result<SourceMeta>;

    /// Watch for changes. Returns a stream of change events.
    async fn watch(&self) -> Result<Pin<Box<dyn Stream<Item = ChangeEvent> + Send>>>;

    /// Whether this source handles its own search. Default: false.
    fn supports_search(&self) -> bool {
        false
    }

    /// Search the source. Only called when supports_search() returns true.
    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SourceSearchResult>> {
        Ok(vec![])
    }

    /// Whether the local indexer should skip this source. Default: false.
    /// Smart sources return true because they own their own search.
    fn skip_indexing(&self) -> bool {
        false
    }
}
