//! GitHub smart source — browse and read configured repositories read-only
//! through the GitHub REST API, with search delegated to GitHub code search.
//!
//! Tree shape: `/owner/repo/<contents…>`. Requires a PAT (fine-grained,
//! read-only Contents scope) stored by `hearthfs auth github`; unauthenticated
//! GitHub allows only 60 requests/hour, which no real mount survives.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::Stream;

use hearthfs_core::source::{ChangeEvent, Source, SourceEntry, SourceMeta, SourceSearchResult};

const API: &str = "https://api.github.com";
const CACHE_TTL: Duration = Duration::from_secs(300);

pub struct GithubSource {
    client: reqwest::Client,
    /// (owner, repo) pairs this mount exposes.
    repos: Vec<(String, String)>,
    cache: RwLock<HashMap<String, (Instant, Vec<u8>)>>,
}

impl GithubSource {
    /// `repos_spec` is the CLI form: `owner/repo[,owner/repo…]`.
    pub fn connect(repos_spec: &str, token: &str) -> Result<Self> {
        let repos: Vec<(String, String)> = repos_spec
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.trim()
                    .split_once('/')
                    .map(|(o, r)| (o.to_string(), r.to_string()))
                    .ok_or_else(|| anyhow!("expected owner/repo, got '{s}'"))
            })
            .collect::<Result<_>>()?;
        if repos.is_empty() {
            return Err(anyhow!("no repositories configured"));
        }

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token.trim())
                .parse()
                .context("invalid token")?,
        );
        headers.insert("X-GitHub-Api-Version", "2022-11-28".parse().unwrap());
        headers.insert(reqwest::header::USER_AGENT, "hearthfs".parse().unwrap());
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            repos,
            cache: RwLock::new(HashMap::new()),
        })
    }

    /// Where `hearthfs auth github` stores the PAT (0600).
    pub fn key_path() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(|h| {
            Path::new(&h)
                .join(".config")
                .join("hearthfs")
                .join("github.key")
        })
    }

    pub fn load_key() -> Option<String> {
        let key = std::fs::read_to_string(Self::key_path()?).ok()?;
        let key = key.trim().to_string();
        (!key.is_empty()).then_some(key)
    }

    async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        if let Some((at, data)) = self.cache.read().unwrap().get(url) {
            if at.elapsed() < CACHE_TTL {
                return Ok(serde_json::from_slice(data)?);
            }
        }
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("GitHub request failed")?;
        match resp.status().as_u16() {
            401 => {
                return Err(anyhow!(
                    "GitHub rejected the token (401) — mint a new PAT and run: hearthfs auth github"
                ))
            }
            403 | 429 => {
                return Err(anyhow!(
                    "GitHub rate limit or access denied ({}) for {url}",
                    resp.status()
                ))
            }
            404 => return Err(anyhow!("not found on GitHub: {url}")),
            s if s >= 400 => return Err(anyhow!("GitHub HTTP {s} from {url}")),
            _ => {}
        }
        let data = resp.bytes().await?.to_vec();
        let json = serde_json::from_slice(&data)?;
        self.cache
            .write()
            .unwrap()
            .insert(url.to_string(), (Instant::now(), data));
        Ok(json)
    }

    /// Split a mount path into (owner, repo, path-in-repo). Errors when the
    /// owner/repo isn't one this mount was configured with.
    fn split(&self, path: &Path) -> Result<(String, String, String)> {
        let parts: Vec<String> = path
            .iter()
            .map(|c| c.to_string_lossy().to_string())
            .collect();
        if parts.len() < 2 {
            return Err(anyhow!("path too short: {}", path.display()));
        }
        let (owner, repo) = (&parts[0], &parts[1]);
        if !self.repos.iter().any(|(o, r)| o == owner && r == repo) {
            return Err(anyhow!(
                "repo {owner}/{repo} is not configured on this mount"
            ));
        }
        Ok((owner.clone(), repo.clone(), parts[2..].join("/")))
    }

    fn contents_url(owner: &str, repo: &str, path: &str) -> String {
        if path.is_empty() {
            format!("{API}/repos/{owner}/{repo}/contents")
        } else {
            format!("{API}/repos/{owner}/{repo}/contents/{path}")
        }
    }

    fn dir_meta() -> SourceMeta {
        SourceMeta {
            size: 0,
            is_dir: true,
            modified: SystemTime::now(),
            created: SystemTime::now(),
        }
    }
}

#[async_trait]
impl Source for GithubSource {
    async fn list(&self, path: &Path) -> Result<Vec<SourceEntry>> {
        let parts: Vec<String> = path
            .iter()
            .map(|c| c.to_string_lossy().to_string())
            .collect();
        match parts.len() {
            // Root: unique owners.
            0 => {
                let mut owners: Vec<&String> = self.repos.iter().map(|(o, _)| o).collect();
                owners.sort();
                owners.dedup();
                Ok(owners
                    .into_iter()
                    .map(|o| SourceEntry {
                        name: o.clone(),
                        is_dir: true,
                    })
                    .collect())
            }
            // Owner: that owner's configured repos.
            1 => {
                let repos: Vec<&String> = self
                    .repos
                    .iter()
                    .filter(|(o, _)| *o == parts[0])
                    .map(|(_, r)| r)
                    .collect();
                if repos.is_empty() {
                    return Err(anyhow!("unknown owner {}", parts[0]));
                }
                Ok(repos
                    .into_iter()
                    .map(|r| SourceEntry {
                        name: r.clone(),
                        is_dir: true,
                    })
                    .collect())
            }
            // Inside a repo: the contents API.
            _ => {
                let (owner, repo, sub) = self.split(path)?;
                let json = self
                    .get_json(&Self::contents_url(&owner, &repo, &sub))
                    .await?;
                let items = json
                    .as_array()
                    .ok_or_else(|| anyhow!("{} is a file, not a directory", path.display()))?;
                Ok(items
                    .iter()
                    .filter_map(|item| {
                        Some(SourceEntry {
                            name: item.get("name")?.as_str()?.to_string(),
                            is_dir: item.get("type")?.as_str()? == "dir",
                        })
                    })
                    .collect())
            }
        }
    }

    async fn read(&self, path: &Path) -> Result<Bytes> {
        let (owner, repo, sub) = self.split(path)?;
        let json = self
            .get_json(&Self::contents_url(&owner, &repo, &sub))
            .await?;
        // Files ≤1MB arrive base64-inline; larger ones only via download_url.
        if let Some(content) = json.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                let cleaned: String = content.chars().filter(|c| !c.is_whitespace()).collect();
                return Ok(Bytes::from(
                    base64::engine::general_purpose::STANDARD
                        .decode(cleaned)
                        .context("decoding GitHub file content")?,
                ));
            }
        }
        if let Some(url) = json.get("download_url").and_then(|u| u.as_str()) {
            let resp = self.client.get(url).send().await?;
            if !resp.status().is_success() {
                return Err(anyhow!("GitHub HTTP {} fetching {url}", resp.status()));
            }
            return Ok(resp.bytes().await?);
        }
        Err(anyhow!("no content for {}", path.display()))
    }

    async fn stat(&self, path: &Path) -> Result<SourceMeta> {
        let depth = path.iter().count();
        if depth <= 2 {
            // Root, owners, and repo roots are directories by construction.
            if depth == 2 {
                self.split(path)?; // validates the repo is configured
            }
            return Ok(Self::dir_meta());
        }
        let (owner, repo, sub) = self.split(path)?;
        let json = self
            .get_json(&Self::contents_url(&owner, &repo, &sub))
            .await?;
        if json.is_array() {
            return Ok(Self::dir_meta());
        }
        Ok(SourceMeta {
            size: json
                .get("size")
                .and_then(|s| s.as_u64())
                .unwrap_or(1024 * 1024),
            is_dir: false,
            modified: SystemTime::now(),
            created: SystemTime::now(),
        })
    }

    async fn watch(&self) -> Result<Pin<Box<dyn Stream<Item = ChangeEvent> + Send>>> {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    fn supports_search(&self) -> bool {
        true
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SourceSearchResult>> {
        // Scope code search to the configured repos.
        let scope: String = self
            .repos
            .iter()
            .map(|(o, r)| format!("+repo:{o}/{r}"))
            .collect();
        let url = format!(
            "{API}/search/code?q={}{scope}&per_page={}",
            urlencode(query),
            limit.min(30)
        );
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github.text-match+json")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("GitHub code search failed: {}", resp.status()));
        }
        let json: serde_json::Value = resp.json().await?;
        let items = json
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|item| {
                let repo_full = item.get("repository")?.get("full_name")?.as_str()?;
                let file_path = item.get("path")?.as_str()?;
                let snippet = item
                    .get("text_matches")
                    .and_then(|m| m.as_array())
                    .and_then(|m| m.first())
                    .and_then(|m| m.get("fragment"))
                    .and_then(|f| f.as_str())
                    .unwrap_or("")
                    .to_string();
                Some(SourceSearchResult {
                    path: PathBuf::from(format!("{repo_full}/{file_path}")),
                    title: file_path.to_string(),
                    snippet,
                    score: item.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0),
                })
            })
            .collect())
    }

    fn skip_indexing(&self) -> bool {
        // Local indexing would fetch every file in every repo through the
        // API — delegate to GitHub code search instead. Users who want deep
        // semantic search over a repo should clone it and mount the clone.
        true
    }
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_specs() {
        let src = GithubSource::connect("a/b, c/d", "ghp_x").unwrap();
        assert_eq!(src.repos.len(), 2);
        assert!(GithubSource::connect("nonsense", "ghp_x").is_err());
        assert!(GithubSource::connect("", "ghp_x").is_err());
    }

    #[test]
    fn urlencodes() {
        assert_eq!(urlencode("auth flow"), "auth+flow");
        assert_eq!(urlencode("a&b"), "a%26b");
    }
}
