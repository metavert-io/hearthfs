use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use hearthfs_core::source::{ChangeEvent, Source, SourceEntry, SourceMeta};

/// Local filesystem source — passthrough adapter over a directory.
pub struct FsSource {
    root: PathBuf,
}

impl FsSource {
    pub fn new(root: PathBuf) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("source directory not found: {}", root.display()))?;
        anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
        Ok(Self { root })
    }

    /// Resolve a relative path against the source root, preventing traversal.
    fn resolve(&self, path: &Path) -> Result<PathBuf> {
        let full = self.root.join(path);
        let canonical = full
            .canonicalize()
            .with_context(|| format!("path not found: {}", full.display()))?;
        anyhow::ensure!(
            canonical.starts_with(&self.root),
            "path traversal denied: {}",
            path.display()
        );
        Ok(canonical)
    }
}

#[async_trait]
impl Source for FsSource {
    async fn list(&self, path: &Path) -> Result<Vec<SourceEntry>> {
        let dir = self.resolve(path)?;
        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("cannot read directory: {}", dir.display()))?;
        while let Some(entry) = read_dir.next_entry().await? {
            let ft = entry.file_type().await?;
            entries.push(SourceEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: ft.is_dir(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn read(&self, path: &Path) -> Result<Bytes> {
        let file = self.resolve(path)?;
        let data = tokio::fs::read(&file)
            .await
            .with_context(|| format!("cannot read file: {}", file.display()))?;
        Ok(Bytes::from(data))
    }

    async fn stat(&self, path: &Path) -> Result<SourceMeta> {
        let full = self.resolve(path)?;
        let meta = tokio::fs::metadata(&full)
            .await
            .with_context(|| format!("cannot stat: {}", full.display()))?;
        Ok(SourceMeta {
            size: meta.len(),
            is_dir: meta.is_dir(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            created: meta
                .created()
                .unwrap_or_else(|_| meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
        })
    }

    async fn watch(&self) -> Result<Pin<Box<dyn Stream<Item = ChangeEvent> + Send>>> {
        let (tx, rx) = mpsc::channel(256);
        let root = self.root.clone();

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                let Ok(event) = res else { return };
                for path in event.paths {
                    let rel = path.strip_prefix(&root).unwrap_or(&path);
                    let boxed: Box<Path> = rel.into();
                    let change = match event.kind {
                        notify::EventKind::Create(_) => ChangeEvent::Created(boxed),
                        notify::EventKind::Modify(_) => ChangeEvent::Modified(boxed),
                        notify::EventKind::Remove(_) => ChangeEvent::Removed(boxed),
                        _ => continue,
                    };
                    let _ = tx.try_send(change);
                }
            },
            notify::Config::default(),
        )?;

        watcher.watch(&self.root, RecursiveMode::Recursive)?;

        // Keep the watcher alive by leaking it — it runs for the lifetime of the daemon.
        // In a real production system we'd store it, but for v0 this is fine.
        std::mem::forget(watcher);

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn test_list_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();

        let source = FsSource::new(tmp.path().to_path_buf()).unwrap();

        let entries = source.list(Path::new("")).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "hello.txt" && !e.is_dir));
        assert!(entries.iter().any(|e| e.name == "subdir" && e.is_dir));

        let content = source.read(Path::new("hello.txt")).await.unwrap();
        assert_eq!(&content[..], b"world");
    }

    #[tokio::test]
    async fn test_path_traversal_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let source = FsSource::new(tmp.path().to_path_buf()).unwrap();
        let result = source.stat(Path::new("../../etc/passwd")).await;
        assert!(result.is_err());
    }
}
