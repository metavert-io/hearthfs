use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::Deserialize;
use tracing::debug;

use crate::source::{ChangeEvent, Source, SourceEntry, SourceMeta, SourceSearchResult};

/// A manifest-driven API source that builds a virtual filesystem from a remote
/// discovery manifest. Any REST API serving a compatible manifest can be mounted.
pub struct ApiSource {
    base_url: String,
    client: reqwest::Client,
    manifest: Manifest,
    /// Cache: path → (fetched_at, content_bytes)
    cache: RwLock<HashMap<String, CacheEntry>>,
}

struct CacheEntry {
    fetched_at: Instant,
    ttl: Duration,
    data: Vec<u8>,
}

impl CacheEntry {
    fn is_fresh(&self) -> bool {
        self.fetched_at.elapsed() < self.ttl
    }
}

// --- Manifest types ---

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub search: Option<SearchConfig>,
    #[serde(default)]
    pub markdown: Option<MarkdownConfig>,
    #[serde(default)]
    pub tree: Vec<TreeNode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchConfig {
    pub supported: bool,
    pub endpoint: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub params: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarkdownConfig {
    pub supported: bool,
    #[serde(default = "default_md_header")]
    pub header: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TreeNode {
    pub path: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub source: Option<NodeSource>,
    #[serde(default)]
    pub cache: Option<CacheConfig>,
    #[serde(default)]
    pub children: Vec<TreeNode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeSource {
    /// For type: "file" — direct API endpoint
    #[serde(default)]
    pub api: Option<String>,
    /// For type: "collection" — list endpoint
    #[serde(default)]
    pub list: Option<String>,
    /// For type: "collection" — single item endpoint (with {id} template)
    #[serde(default)]
    pub item: Option<String>,
    /// Accept header for requests
    #[serde(default)]
    pub accept: Option<String>,
    /// Field to generate filesystem slug from
    #[serde(default)]
    pub slug_from: Option<String>,
    /// Field containing the item ID
    #[serde(default)]
    pub id_field: Option<String>,
    /// Static list of values for a collection
    #[serde(default)]
    pub values: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
}

fn default_method() -> String {
    "GET".to_string()
}
fn default_md_header() -> String {
    "Accept: text/markdown".to_string()
}
fn default_ttl() -> u64 {
    120
}

impl ApiSource {
    /// Connect to a remote API, fetch its manifest, and build the virtual filesystem.
    pub async fn new(
        base_url: String,
        auth_header: Option<String>,
        manifest_path: String,
    ) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(auth) = &auth_header {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                auth.parse().context("invalid auth header")?,
            );
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()?;

        let manifest_url = format!("{}{}", base_url, manifest_path);
        debug!("fetching manifest from {manifest_url}");

        let resp = client
            .get(&manifest_url)
            .send()
            .await
            .context("failed to fetch manifest")?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "manifest fetch failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }

        let manifest: Manifest = resp.json().await.context("failed to parse manifest")?;
        debug!(
            "loaded manifest: {} v{} ({} tree nodes)",
            manifest.name,
            manifest.version,
            manifest.tree.len()
        );

        Ok(Self {
            base_url,
            client,
            manifest,
            cache: RwLock::new(HashMap::new()),
        })
    }

    /// Get the TTL for a tree node.
    fn node_ttl(node: &TreeNode) -> Duration {
        Duration::from_secs(
            node.cache
                .as_ref()
                .map(|c| c.ttl_seconds)
                .unwrap_or(default_ttl()),
        )
    }

    /// Check cache for a key.
    fn cache_get(&self, key: &str) -> Option<Vec<u8>> {
        let cache = self.cache.read().unwrap();
        cache
            .get(key)
            .filter(|e| e.is_fresh())
            .map(|e| e.data.clone())
    }

    /// Store in cache.
    fn cache_set(&self, key: String, data: Vec<u8>, ttl: Duration) {
        let mut cache = self.cache.write().unwrap();
        cache.insert(
            key,
            CacheEntry {
                fetched_at: Instant::now(),
                ttl,
                data,
            },
        );
    }

    /// Make an HTTP GET request, optionally requesting markdown.
    async fn fetch(&self, endpoint: &str, accept_markdown: bool) -> Result<Vec<u8>> {
        let url = if endpoint.starts_with("http") {
            endpoint.to_string()
        } else {
            format!("{}{}", self.base_url, endpoint)
        };

        let mut req = self.client.get(&url);
        if accept_markdown {
            req = req.header("Accept", "text/markdown");
        }

        let resp = req.send().await.context("HTTP request failed")?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "HTTP 401 from {url} — API key rejected (revoked/expired?); mint a new one and re-authenticate"
            ));
        }
        if !resp.status().is_success() {
            return Err(anyhow!("HTTP {} from {}", resp.status(), url));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Resolve a virtual path to its (endpoint, wants_markdown, ttl), or
    /// `None` if the path doesn't map to a readable node.
    async fn endpoint_for(
        &self,
        path: &Path,
        path_str: &str,
    ) -> Result<Option<(String, bool, Duration)>> {
        if let Some((node, vars)) = resolve_in_tree(&self.manifest.tree, path_str) {
            let source = node
                .source
                .as_ref()
                .ok_or_else(|| anyhow!("no source for {path_str}"))?;
            let wants_md = self.wants_markdown(node);
            let ttl = Self::node_ttl(node);

            // Direct file with api endpoint
            if let Some(api) = &source.api {
                return Ok(Some((Self::expand_endpoint(api, &vars), wants_md, ttl)));
            }

            // Collection item — resolve slug to ID, fetch via item endpoint
            if let Some(item_template) = &source.item {
                let slug = path
                    .file_name()
                    .ok_or_else(|| anyhow!("no filename in path"))?
                    .to_string_lossy();

                // The matched node IS the collection — use it to resolve the slug
                let id = self.resolve_item_id(node, &slug, &vars).await?;
                let mut all_vars = vars;
                all_vars.insert("id".to_string(), id.clone());
                if let Some(id_field) = source.id_field.as_deref() {
                    all_vars.insert(id_field.to_string(), id);
                }
                return Ok(Some((
                    Self::expand_endpoint(item_template, &all_vars),
                    wants_md,
                    ttl,
                )));
            }
        }

        // Fallback: item of a plain-path collection — /entities/<slug>.md
        // where the tree node is `/entities` with an `item` template. (The
        // template-path style /people/{slug}.md resolves above instead.)
        if let Some((parent, _)) = path_str.rsplit_once('/') {
            let parent_prefix = format!("{parent}/");
            if let Some(node) = find_collection_for_parent(&self.manifest.tree, &parent_prefix) {
                if let Some(item_template) = node.source.as_ref().and_then(|s| s.item.as_ref()) {
                    let slug = path
                        .file_name()
                        .ok_or_else(|| anyhow!("no filename in path"))?
                        .to_string_lossy();
                    let id = self.resolve_item_id(node, &slug, &HashMap::new()).await?;
                    let mut vars = HashMap::new();
                    vars.insert("id".to_string(), id.clone());
                    if let Some(id_field) = node.source.as_ref().and_then(|s| s.id_field.as_deref())
                    {
                        vars.insert(id_field.to_string(), id);
                    }
                    return Ok(Some((
                        Self::expand_endpoint(item_template, &vars),
                        self.wants_markdown(node),
                        Self::node_ttl(node),
                    )));
                }
            }
        }

        Ok(None)
    }

    /// Parse an API endpoint template, substituting variables.
    fn expand_endpoint(template: &str, vars: &HashMap<String, String>) -> String {
        let mut result = template.to_string();
        for (key, value) in vars {
            result = result.replace(&format!("{{{key}}}"), value);
        }
        // Strip "GET " or "POST " prefix if present
        if let Some(stripped) = result.strip_prefix("GET ") {
            return stripped.to_string();
        }
        if let Some(stripped) = result.strip_prefix("POST ") {
            return stripped.to_string();
        }
        result
    }

    /// Whether the manifest indicates markdown support.
    fn wants_markdown(&self, node: &TreeNode) -> bool {
        // Check node-level accept header first
        if let Some(source) = &node.source {
            if let Some(accept) = &source.accept {
                return accept.contains("markdown");
            }
        }
        // Fall back to manifest-level markdown config
        self.manifest
            .markdown
            .as_ref()
            .map(|m| m.supported)
            .unwrap_or(false)
    }

    /// List entries for a collection node by calling its list endpoint.
    async fn list_collection(
        &self,
        node: &TreeNode,
        vars: &HashMap<String, String>,
    ) -> Result<Vec<SourceEntry>> {
        let source = node
            .source
            .as_ref()
            .ok_or_else(|| anyhow!("collection has no source"))?;

        // Static values collection
        if let Some(values) = &source.values {
            return Ok(values
                .iter()
                .map(|v| SourceEntry {
                    name: v.clone(),
                    is_dir: !node.children.is_empty(),
                })
                .collect());
        }

        // Dynamic collection via API
        let list_endpoint = source
            .list
            .as_ref()
            .ok_or_else(|| anyhow!("collection has no list endpoint"))?;

        let endpoint = Self::expand_endpoint(list_endpoint, vars);
        let cache_key = format!("list:{endpoint}");

        if let Some(cached) = self.cache_get(&cache_key) {
            let items: Vec<serde_json::Value> = serde_json::from_slice(&cached)?;
            return Ok(self.items_to_entries(node, &items));
        }

        let data = self.fetch(&endpoint, false).await?;
        let items: Vec<serde_json::Value> = serde_json::from_slice(&data)?;
        let entries = self.items_to_entries(node, &items);

        // Cache the raw JSON list
        self.cache_set(cache_key, data, Self::node_ttl(node));

        Ok(entries)
    }

    /// Convert JSON items to directory entries using slug_from field.
    fn items_to_entries(&self, node: &TreeNode, items: &[serde_json::Value]) -> Vec<SourceEntry> {
        let source = match &node.source {
            Some(s) => s,
            None => return vec![],
        };
        let slug_field = source.slug_from.as_deref().unwrap_or("name");
        let id_field = source.id_field.as_deref().unwrap_or("id");
        // Entries carry .md when either path style says so: template paths
        // (/people/{slug}.md) or plain collection paths whose item endpoint
        // serves markdown (/entities + item /brain/entities/{id}.md).
        let has_ext = node.path.ends_with(".md")
            || source.item.as_deref().is_some_and(|i| i.ends_with(".md"));

        unique_collection_slugs(items, slug_field, id_field)
            .into_iter()
            .map(|(slug, _id)| SourceEntry {
                name: if has_ext { format!("{slug}.md") } else { slug },
                is_dir: false,
            })
            .collect()
    }

    /// Resolve a slug back to an API item ID for a collection.
    async fn resolve_item_id(
        &self,
        node: &TreeNode,
        slug: &str,
        vars: &HashMap<String, String>,
    ) -> Result<String> {
        let source = node
            .source
            .as_ref()
            .ok_or_else(|| anyhow!("no source config"))?;

        let slug_field = source.slug_from.as_deref().unwrap_or("name");
        let id_field = source.id_field.as_deref().unwrap_or("id");

        let list_endpoint = source
            .list
            .as_ref()
            .ok_or_else(|| anyhow!("no list endpoint"))?;
        let endpoint = Self::expand_endpoint(list_endpoint, vars);
        let cache_key = format!("list:{endpoint}");

        // Try cache first
        let data = if let Some(cached) = self.cache_get(&cache_key) {
            cached
        } else {
            let fetched = self.fetch(&endpoint, false).await?;
            self.cache_set(cache_key, fetched.clone(), Self::node_ttl(node));
            fetched
        };

        let items: Vec<serde_json::Value> = serde_json::from_slice(&data)?;
        let clean_slug = slug.strip_suffix(".md").unwrap_or(slug);

        // Must mirror items_to_entries exactly: entries are served with
        // deduplicated slugs, so lookups resolve through the same mapping.
        for (entry_slug, id) in unique_collection_slugs(&items, slug_field, id_field) {
            if entry_slug == clean_slug {
                return Ok(id);
            }
        }

        Err(anyhow!("item not found for slug: {slug}"))
    }
}

#[async_trait]
impl Source for ApiSource {
    async fn list(&self, path: &Path) -> Result<Vec<SourceEntry>> {
        let path_str = if path.as_os_str().is_empty() {
            "/".to_string()
        } else {
            format!("/{}", path.display())
        };

        // Root listing: return top-level tree node names
        if path_str == "/" {
            return Ok(self
                .manifest
                .tree
                .iter()
                .map(|node| {
                    let name = node
                        .path
                        .trim_start_matches('/')
                        .split('/')
                        .next()
                        .unwrap_or(&node.path);
                    SourceEntry {
                        name: name.to_string(),
                        is_dir: node.node_type == "collection"
                            || !node.children.is_empty()
                            || name != node.path.trim_start_matches('/'),
                    }
                })
                .collect());
        }

        // Find matching tree node
        if let Some((node, vars)) = resolve_in_tree(&self.manifest.tree, &path_str) {
            // If this node has children AND we resolved variables, list the children.
            // e.g., /intents/active has children [{slug}.md collection]
            // e.g., /panels/research has children [summary.md, items/]
            if !node.children.is_empty() && !vars.is_empty() {
                let mut entries = Vec::new();
                for child in &node.children {
                    let child_name = child
                        .path
                        .trim_start_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or(&child.path);
                    if child.node_type == "collection" {
                        // Dynamic collection child — list its items
                        match self.list_collection(child, &vars).await {
                            Ok(items) => entries.extend(items),
                            Err(e) => {
                                debug!("failed to list child collection {}: {e}", child.path);
                            }
                        }
                    } else {
                        // Static child (e.g., summary.md)
                        entries.push(SourceEntry {
                            name: child_name.to_string(),
                            is_dir: child.node_type != "file",
                        });
                    }
                }
                return Ok(entries);
            }

            // Collection without resolved vars — list the collection values/items
            if node.node_type == "collection" {
                return self.list_collection(node, &vars).await;
            }

            // Non-collection with static children
            if !node.children.is_empty() {
                return Ok(node
                    .children
                    .iter()
                    .map(|child| {
                        let name = child
                            .path
                            .trim_start_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or(&child.path);
                        SourceEntry {
                            name: name.to_string(),
                            is_dir: child.node_type != "file",
                        }
                    })
                    .collect());
            }
        }

        // Check if this path is the parent directory of a collection
        // e.g., /people is the parent of /people/{slug}.md
        let prefix = format!("{path_str}/");
        if let Some(node) = find_collection_for_parent(&self.manifest.tree, &prefix) {
            let vars = HashMap::new(); // parent path has no variables to resolve
            return self.list_collection(node, &vars).await;
        }

        // Check if this is a parent directory of tree nodes (static children)
        let mut entries: Vec<SourceEntry> = vec![];
        let mut seen = std::collections::HashSet::new();
        collect_children(&self.manifest.tree, &prefix, &mut entries, &mut seen);
        if !entries.is_empty() {
            return Ok(entries);
        }

        Err(anyhow!("path not found: {path_str}"))
    }

    async fn read(&self, path: &Path) -> Result<Bytes> {
        let path_str = format!("/{}", path.display());
        let cache_key = format!("read:{path_str}");

        if let Some(cached) = self.cache_get(&cache_key) {
            return Ok(Bytes::from(cached));
        }

        if let Some((endpoint, wants_md, ttl)) = self.endpoint_for(path, &path_str).await? {
            let data = self.fetch(&endpoint, wants_md).await?;
            self.cache_set(cache_key, data.clone(), ttl);
            return Ok(Bytes::from(data));
        }

        Err(anyhow!("cannot read: {path_str}"))
    }

    async fn read_fit(&self, path: &Path, budget: u64) -> Result<Option<Bytes>> {
        let path_str = format!("/{}", path.display());
        // Budget is part of the cache key: a 500-token view and a 5000-token
        // view of the same path are different content.
        let cache_key = format!("read:{path_str}?budget={budget}");

        if let Some(cached) = self.cache_get(&cache_key) {
            return Ok(Some(Bytes::from(cached)));
        }

        if let Some((endpoint, wants_md, ttl)) = self.endpoint_for(path, &path_str).await? {
            let data = self
                .fetch(&with_budget(&endpoint, budget), wants_md)
                .await?;
            self.cache_set(cache_key, data.clone(), ttl);
            return Ok(Some(Bytes::from(data)));
        }

        Ok(None)
    }

    async fn stat(&self, path: &Path) -> Result<SourceMeta> {
        let path_str = if path.as_os_str().is_empty() {
            "/".to_string()
        } else {
            format!("/{}", path.display())
        };

        // Root is always a directory
        if path_str == "/" {
            return Ok(SourceMeta {
                size: 0,
                is_dir: true,
                modified: SystemTime::now(),
                created: SystemTime::now(),
            });
        }

        // Check if it matches a tree node
        if let Some((node, vars)) = resolve_in_tree(&self.manifest.tree, &path_str) {
            // A resolved collection template is a file ONLY if it's a leaf
            // (no children). If it has children, it's a directory you can ls into.
            // e.g., /people/jane-doe.md (leaf, no children) → file
            //       /intents/active (has children: {slug}.md) → directory
            //       /panels/research (has children: summary.md, items/) → directory
            let is_concrete_leaf =
                node.node_type == "collection" && !vars.is_empty() && node.children.is_empty();
            let is_dir = if is_concrete_leaf {
                false
            } else {
                node.node_type == "collection" || !node.children.is_empty()
            };
            let size = if is_dir { 0 } else { 1024 * 1024 };
            return Ok(SourceMeta {
                size,
                is_dir,
                modified: SystemTime::now(),
                created: SystemTime::now(),
            });
        }

        // Check if it's a parent directory of tree nodes
        let prefix = format!("{path_str}/");
        let has_children = self
            .manifest
            .tree
            .iter()
            .any(|n| n.path.starts_with(&prefix))
            || self.manifest.tree.iter().any(|n| {
                // Check nested children too
                has_descendant(&n.children, &prefix)
            });
        if has_children {
            return Ok(SourceMeta {
                size: 0,
                is_dir: true,
                modified: SystemTime::now(),
                created: SystemTime::now(),
            });
        }

        // Check if this is a parent directory of a collection (e.g., /people for /people/{slug}.md)
        if find_collection_for_parent(&self.manifest.tree, &prefix).is_some() {
            return Ok(SourceMeta {
                size: 0,
                is_dir: true,
                modified: SystemTime::now(),
                created: SystemTime::now(),
            });
        }

        // It might be a dynamic collection item (slug.md) — treat as a file
        // Find the parent collection: /people/jane-doe.md → parent /people/
        if let Some(parent_str) = path_str.rsplit_once('/').map(|(p, _)| p.to_string()) {
            let parent_prefix = format!("{parent_str}/");
            if find_collection_for_parent(&self.manifest.tree, &parent_prefix).is_some() {
                return Ok(SourceMeta {
                    size: 1024 * 1024,
                    is_dir: false,
                    modified: SystemTime::now(),
                    created: SystemTime::now(),
                });
            }
        }

        Err(anyhow!("not found: {path_str}"))
    }

    async fn watch(&self) -> Result<Pin<Box<dyn Stream<Item = ChangeEvent> + Send>>> {
        // No watch support for API sources yet
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    fn supports_search(&self) -> bool {
        self.manifest
            .search
            .as_ref()
            .map(|s| s.supported)
            .unwrap_or(false)
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SourceSearchResult>> {
        let search_config = self
            .manifest
            .search
            .as_ref()
            .ok_or_else(|| anyhow!("search not configured in manifest"))?;

        let url = format!("{}{}", self.base_url, search_config.endpoint);
        let resp = self
            .client
            .get(&url)
            .query(&[("q", query), ("limit", &limit.to_string())])
            .header("Accept", "application/json")
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!("search failed: {}", resp.status()));
        }

        // Search response may be {results: [...]} or a flat array
        let body: serde_json::Value = resp.json().await?;
        let items = match &body {
            serde_json::Value::Array(arr) => arr.clone(),
            serde_json::Value::Object(obj) => obj
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            _ => vec![],
        };
        let results = items
            .iter()
            .map(|item| {
                let title = item
                    .get("title")
                    .or_else(|| item.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("untitled")
                    .to_string();
                let snippet = item
                    .get("snippet")
                    .or_else(|| item.get("summary"))
                    .or_else(|| item.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let score = item
                    .get("score")
                    .or_else(|| item.get("relevance"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let path = item
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(slugify(&title)));

                SourceSearchResult {
                    path,
                    title,
                    snippet,
                    score,
                }
            })
            .collect();

        Ok(results)
    }

    fn skip_indexing(&self) -> bool {
        true
    }
}

// --- Helper functions ---

/// Deterministic, unique (slug, id) pairs for a collection listing.
///
/// Raw slugs can collide ("Hearthloom" and "hearthloom" are one slug) or come
/// out empty — and duplicate or empty filenames in one directory corrupt NFS
/// READDIR replies client-side ("RPC struct is bad", seen live 2026-07-11).
/// First occurrence keeps the plain slug; later collisions get an id suffix;
/// empty slugs fall back to the id. resolve_item_id resolves through this
/// same mapping, so served names always round-trip.
fn unique_collection_slugs(
    items: &[serde_json::Value],
    slug_field: &str,
    id_field: &str,
) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let Some(name) = item.get(slug_field).and_then(|v| v.as_str()) else {
            continue;
        };
        let id = match item.get(id_field) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(other) => other.to_string(),
            None => continue,
        };
        let mut slug = slugify(name);
        if slug.is_empty() {
            slug = slugify(&id);
        }
        if slug.is_empty() {
            continue;
        }
        if !seen.insert(slug.clone()) {
            slug = format!("{slug}-{}", slugify(&id));
            if slug.is_empty() || !seen.insert(slug.clone()) {
                continue; // still colliding — drop rather than corrupt readdir
            }
        }
        out.push((slug, id));
    }
    out
}

/// Append a `budget=<tokens>` query param to an endpoint that may or may not
/// already carry a query string.
fn with_budget(endpoint: &str, budget: u64) -> String {
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    format!("{endpoint}{sep}budget={budget}")
}

/// Convert a name/title to a filesystem-safe slug.
pub fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    // Dedup consecutive hyphens, trim
    let mut result = String::new();
    let mut prev_hyphen = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_hyphen && !result.is_empty() {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result.trim_end_matches('-').to_string()
}

/// Resolve a path string against a tree of nodes, supporting path templates.
fn resolve_in_tree<'a>(
    nodes: &'a [TreeNode],
    path: &str,
) -> Option<(&'a TreeNode, HashMap<String, String>)> {
    resolve_in_tree_with_prefix(nodes, path, "")
}

fn resolve_in_tree_with_prefix<'a>(
    nodes: &'a [TreeNode],
    path: &str,
    parent_prefix: &str,
) -> Option<(&'a TreeNode, HashMap<String, String>)> {
    let path = path.trim_end_matches('/');
    for node in nodes {
        // Build full path for this node: parent prefix + node's relative path
        let full_path = if parent_prefix.is_empty() {
            node.path.clone()
        } else {
            format!(
                "{}/{}",
                parent_prefix.trim_end_matches('/'),
                node.path.trim_start_matches('/')
            )
        };
        let full_path = full_path.trim_end_matches('/').to_string();
        let mut vars = HashMap::new();

        if match_template(&full_path, path, &mut vars) {
            return Some((node, vars));
        }

        // Recurse into children with this node's resolved path as prefix
        // For template paths, we need the concrete prefix (with vars substituted)
        if !node.children.is_empty() {
            // Try to match path as a prefix of this node
            let node_parts: Vec<&str> = full_path.split('/').filter(|s| !s.is_empty()).collect();
            let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

            if path_parts.len() > node_parts.len() {
                // Path is longer — check if the first N parts match this node
                let mut prefix_vars = HashMap::new();
                let prefix_path: String = format!("/{}", path_parts[..node_parts.len()].join("/"));
                if match_template(&full_path, &prefix_path, &mut prefix_vars) {
                    // Build concrete prefix from the matched path parts
                    let concrete_prefix = prefix_path;
                    if let Some((child_node, mut child_vars)) =
                        resolve_in_tree_with_prefix(&node.children, path, &concrete_prefix)
                    {
                        // Merge parent vars into child vars
                        for (k, v) in prefix_vars {
                            child_vars.entry(k).or_insert(v);
                        }
                        return Some((child_node, child_vars));
                    }
                }
            }
        }
    }
    None
}

/// Match a path template like /people/{slug}.md against a concrete path.
fn match_template(template: &str, path: &str, vars: &mut HashMap<String, String>) -> bool {
    let tmpl_parts: Vec<&str> = template.split('/').filter(|s| !s.is_empty()).collect();
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if tmpl_parts.len() != path_parts.len() {
        return false;
    }

    for (t, p) in tmpl_parts.iter().zip(path_parts.iter()) {
        if let Some(var_content) = t.strip_prefix('{') {
            if let Some(var_name) = var_content.strip_suffix('}') {
                vars.insert(var_name.to_string(), p.to_string());
            } else if let Some(var_and_ext) = var_content.strip_suffix(".md}") {
                // Handle {slug}.md pattern
                if let Some(val) = p.strip_suffix(".md") {
                    vars.insert(var_and_ext.to_string(), val.to_string());
                } else {
                    return false;
                }
            } else {
                // Template variable with suffix like {name}.md
                if let Some((var_name, suffix)) = var_content.split_once('}') {
                    if let Some(val) = p.strip_suffix(suffix) {
                        vars.insert(var_name.to_string(), val.to_string());
                    } else {
                        return false;
                    }
                }
            }
        } else if t != p {
            return false;
        }
    }
    true
}

/// Collect immediate children under a given prefix from the tree.
fn collect_children(
    nodes: &[TreeNode],
    prefix: &str,
    entries: &mut Vec<SourceEntry>,
    seen: &mut std::collections::HashSet<String>,
) {
    for node in nodes {
        let node_path = &node.path;
        if let Some(rest) = node_path.strip_prefix(prefix) {
            let name = rest.split('/').next().unwrap_or(rest);
            if !name.is_empty() && !name.contains('{') && seen.insert(name.to_string()) {
                entries.push(SourceEntry {
                    name: name.to_string(),
                    is_dir: node.node_type != "file" || rest.contains('/'),
                });
            }
        }
        collect_children(&node.children, prefix, entries, seen);
    }
}

/// Find a collection node whose path template starts with the given prefix.
/// e.g., prefix="/people/" matches node with path="/people/{slug}.md"
fn find_collection_for_parent<'a>(nodes: &'a [TreeNode], prefix: &str) -> Option<&'a TreeNode> {
    for node in nodes {
        // Check if this node's path starts with the prefix and has a template variable
        let node_path = &node.path;
        if node.node_type == "collection" && node_path.starts_with(prefix) {
            return Some(node);
        }
        // Plain-path collection: the node IS the parent directory
        // (path "/entities" for prefix "/entities/")
        if node.node_type == "collection" && node_path.as_str() == prefix.trim_end_matches('/') {
            return Some(node);
        }
        // Also check: the node's static path prefix matches
        // e.g., for /people/{slug}.md, the static prefix is /people/
        let static_prefix = if let Some(brace_pos) = node_path.find('{') {
            &node_path[..brace_pos]
        } else {
            node_path.as_str()
        };
        if static_prefix == prefix && node.node_type == "collection" {
            return Some(node);
        }
        // Recurse
        if let Some(found) = find_collection_for_parent(&node.children, prefix) {
            return Some(found);
        }
    }
    None
}

fn has_descendant(nodes: &[TreeNode], prefix: &str) -> bool {
    nodes
        .iter()
        .any(|n| n.path.starts_with(prefix) || has_descendant(&n.children, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Jane Doe"), "jane-doe");
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("  spaces  "), "spaces");
        assert_eq!(slugify("CamelCase"), "camelcase");
    }

    #[test]
    fn test_match_template() {
        let mut vars = HashMap::new();
        assert!(match_template("/people", "/people", &mut vars));
        assert!(vars.is_empty());

        vars.clear();
        assert!(match_template(
            "/people/{slug}.md",
            "/people/jane-doe.md",
            &mut vars
        ));
        assert_eq!(vars.get("slug").unwrap(), "jane-doe");

        vars.clear();
        assert!(match_template(
            "/panels/{name}",
            "/panels/research",
            &mut vars
        ));
        assert_eq!(vars.get("name").unwrap(), "research");

        vars.clear();
        assert!(!match_template(
            "/people/{slug}.md",
            "/panels/research",
            &mut vars
        ));
    }

    #[test]
    fn test_unique_collection_slugs() {
        let items: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                {"id": "e1", "name": "Hearthloom"},
                {"id": "e2", "name": "hearthloom"},
                {"id": "e3", "name": "!!!"},
                {"id": "e4", "name": "Jane Doe"}
            ]"#,
        )
        .unwrap();
        let slugs = unique_collection_slugs(&items, "name", "id");
        assert_eq!(
            slugs,
            vec![
                ("hearthloom".to_string(), "e1".to_string()),
                ("hearthloom-e2".to_string(), "e2".to_string()),
                ("e3".to_string(), "e3".to_string()), // empty slug → id fallback
                ("jane-doe".to_string(), "e4".to_string()),
            ]
        );
        // No duplicates — the invariant that keeps NFS READDIR replies valid
        let names: std::collections::HashSet<_> = slugs.iter().map(|(s, _)| s).collect();
        assert_eq!(names.len(), slugs.len());
    }

    #[test]
    fn test_with_budget() {
        assert_eq!(
            with_budget("/brain/BRAIN.md", 2000),
            "/brain/BRAIN.md?budget=2000"
        );
        assert_eq!(
            with_budget("/brain/days/2026-07-11.md?detail=full", 500),
            "/brain/days/2026-07-11.md?detail=full&budget=500"
        );
    }

    #[test]
    fn test_expand_endpoint() {
        let mut vars = HashMap::new();
        vars.insert("id".to_string(), "42".to_string());
        assert_eq!(
            ApiSource::expand_endpoint("GET /api/people/{id}", &vars),
            "/api/people/42"
        );
        assert_eq!(
            ApiSource::expand_endpoint("/api/items/{id}/detail", &vars),
            "/api/items/42/detail"
        );
    }
}
