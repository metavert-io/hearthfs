use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;

use hearthfs_core::api_source::ApiSource;
use hearthfs_core::source::{ChangeEvent, Source, SourceEntry, SourceMeta, SourceSearchResult};

/// Hearthloom smart source — mounts the local Hearthloom "second brain"
/// daemon as a browsable filesystem (BRAIN.md, entity dossiers, day recalls,
/// goals), with search and @fit delegated to the daemon.
///
/// Thin wrapper around ApiSource. The Hearthloom-specific knowledge here is
/// the default daemon URL, the manifest path, the Bearer PAT auth (keys are
/// minted in Hearthloom: ☰ → API Keys, scope `read`), and the key file
/// location. The served surface is always the vault-LOCKED view — private
/// material never crosses this boundary, by Hearthloom's design.
pub struct HearthloomSource {
    inner: ApiSource,
}

/// The Hearthloom daemon's fixed local address.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:43278";
const MANIFEST_PATH: &str = "/api/discovery/manifest";

/// Marker file Hearthloom serves at the mount root; its presence in a
/// directory means "this is a mounted brain" (a recursion guard; see
/// https://github.com/metavert-io/hearthloom).
pub const ORIGIN_MARKER: &str = ".hearthloom-origin";

impl HearthloomSource {
    /// Connect to a Hearthloom daemon. `base_url` is e.g.
    /// `http://127.0.0.1:43278`; the API key is a Hearthloom PAT (`hl_…`)
    /// with the `read` scope.
    pub async fn connect(base_url: &str, api_key: &str) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let auth = format!("Bearer {}", api_key.trim());
        let inner = ApiSource::new(base_url, Some(auth), MANIFEST_PATH.to_string())
            .await
            .map_err(|e| {
                if e.to_string().contains("401") {
                    anyhow!(
                        "Hearthloom rejected the API key (401 Unauthorized). Keys are \
                         revocable and shown only once — mint a fresh read-scoped key in \
                         Hearthloom (menu → API Keys) and store it with: hearthfs auth hearthloom"
                    )
                } else {
                    e
                }
            })?;
        Ok(Self { inner })
    }

    /// Where `hearthfs auth hearthloom` stores the PAT (0600).
    pub fn key_path() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(|h| {
            Path::new(&h)
                .join(".config")
                .join("hearthfs")
                .join("hearthloom.key")
        })
    }

    /// Load the stored PAT, if any.
    pub fn load_key() -> Option<String> {
        let path = Self::key_path()?;
        let key = std::fs::read_to_string(path).ok()?;
        let key = key.trim().to_string();
        (!key.is_empty()).then_some(key)
    }
}

#[async_trait]
impl Source for HearthloomSource {
    async fn list(&self, path: &Path) -> Result<Vec<SourceEntry>> {
        self.inner.list(path).await
    }

    async fn read(&self, path: &Path) -> Result<Bytes> {
        self.inner.read(path).await
    }

    async fn read_fit(&self, path: &Path, budget: u64) -> Result<Option<Bytes>> {
        // Hearthloom trims server-side via ?budget= — the differentiating
        // integration feature (no full fetch, semantic-boundary trimming).
        self.inner.read_fit(path, budget).await
    }

    async fn stat(&self, path: &Path) -> Result<SourceMeta> {
        self.inner.stat(path).await
    }

    async fn watch(&self) -> Result<Pin<Box<dyn Stream<Item = ChangeEvent> + Send>>> {
        self.inner.watch().await
    }

    fn supports_search(&self) -> bool {
        self.inner.supports_search()
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SourceSearchResult>> {
        self.inner.search(query, limit).await
    }

    fn skip_indexing(&self) -> bool {
        // The brain has its own hybrid index — never re-embed it.
        true
    }
}
