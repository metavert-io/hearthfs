//! MCP smart source — mount any Model Context Protocol server's *resources*
//! as files. One adapter covers the whole MCP ecosystem, and flips the usual
//! story: agents that don't speak MCP read MCP content through plain file
//! operations — and because this source does NOT skip local indexing,
//! HearthFS gives MCP servers semantic search they don't natively have.
//!
//! Transports: Streamable HTTP (`mcp:<url>`) and stdio (`mcp:cmd:<command>`,
//! run via `sh -c`). Resources are presented as a flat directory of files
//! named from the resource name/URI with mime-derived extensions;
//! collisions are uniquified (same invariant as ApiSource: duplicate or
//! empty names corrupt NFS READDIR replies).

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::Stream;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, RwLock};

use hearthfs_core::source::{ChangeEvent, Source, SourceEntry, SourceMeta, SourceSearchResult};

const PROTOCOL_VERSION: &str = "2025-03-26";
const LIST_TTL: Duration = Duration::from_secs(60);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESOURCES: usize = 5000;

#[derive(Debug, Clone)]
struct Resource {
    uri: String,
    #[allow(dead_code)]
    mime: Option<String>,
}

pub struct McpSource {
    transport: Transport,
    next_id: AtomicU64,
    /// filename → resource, refreshed on TTL expiry.
    listing: RwLock<Option<(Instant, HashMap<String, Resource>, Vec<String>)>>,
}

enum Transport {
    Http {
        client: reqwest::Client,
        url: String,
        session: RwLock<Option<String>>,
    },
    Stdio {
        io: Mutex<StdioIo>,
    },
}

struct StdioIo {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl McpSource {
    /// `spec` is `<http(s) url>` or `cmd:<shell command>`; `auth` is an
    /// optional bearer token (HTTP transport only).
    pub async fn connect(spec: &str, auth: Option<String>) -> Result<Self> {
        let transport = if let Some(command) = spec.strip_prefix("cmd:") {
            let mut child = tokio::process::Command::new("/bin/sh")
                .args(["-c", command])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("spawning MCP server: {command}"))?;
            let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
            let stdout = BufReader::new(child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?);
            Transport::Stdio {
                io: Mutex::new(StdioIo {
                    _child: child,
                    stdin,
                    stdout,
                }),
            }
        } else {
            let mut headers = reqwest::header::HeaderMap::new();
            if let Some(token) = &auth {
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", token.trim())
                        .parse()
                        .context("invalid auth token")?,
                );
            }
            Transport::Http {
                client: reqwest::Client::builder()
                    .default_headers(headers)
                    .timeout(RPC_TIMEOUT)
                    .build()?,
                url: spec.trim_end_matches('/').to_string(),
                session: RwLock::new(None),
            }
        };

        let source = Self {
            transport,
            next_id: AtomicU64::new(1),
            listing: RwLock::new(None),
        };

        // MCP handshake: initialize, then the initialized notification.
        let init = source
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "hearthfs", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await
            .context("MCP initialize failed")?;
        tracing::debug!(server = %init.get("serverInfo").cloned().unwrap_or_default(), "MCP initialized");
        source.notify("notifications/initialized").await?;
        Ok(source)
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = tokio::time::timeout(RPC_TIMEOUT, self.roundtrip(&request, id))
            .await
            .map_err(|_| anyhow!("MCP {method} timed out"))??;
        if let Some(err) = response.get("error") {
            return Err(anyhow!("MCP {method} error: {err}"));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str) -> Result<()> {
        let notification = json!({"jsonrpc": "2.0", "method": method});
        match &self.transport {
            Transport::Http { client, url, session } => {
                let mut req = client
                    .post(url)
                    .header("Accept", "application/json, text/event-stream")
                    .json(&notification);
                if let Some(sid) = session.read().await.as_ref() {
                    req = req.header("Mcp-Session-Id", sid);
                }
                let _ = req.send().await; // notifications expect no response (202)
            }
            Transport::Stdio { io } => {
                let mut io = io.lock().await;
                io.stdin
                    .write_all(format!("{notification}\n").as_bytes())
                    .await?;
                io.stdin.flush().await?;
            }
        }
        Ok(())
    }

    async fn roundtrip(&self, request: &Value, id: u64) -> Result<Value> {
        match &self.transport {
            Transport::Http { client, url, session } => {
                let mut req = client
                    .post(url)
                    .header("Accept", "application/json, text/event-stream")
                    .json(request);
                if let Some(sid) = session.read().await.as_ref() {
                    req = req.header("Mcp-Session-Id", sid);
                }
                let resp = req.send().await.context("MCP HTTP request failed")?;
                if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                    return Err(anyhow!(
                        "MCP server rejected the token (401) — store a fresh one with: \
                         hearthfs auth <key-name>"
                    ));
                }
                if !resp.status().is_success() {
                    return Err(anyhow!("MCP HTTP {} from {url}", resp.status()));
                }
                if let Some(sid) = resp
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                {
                    *session.write().await = Some(sid.to_string());
                }
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let body = resp.text().await?;
                if content_type.contains("text/event-stream") {
                    // The response arrives as SSE data events; find our id.
                    for line in body.lines() {
                        if let Some(data) = line.strip_prefix("data:") {
                            if let Ok(msg) = serde_json::from_str::<Value>(data.trim()) {
                                if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                                    return Ok(msg);
                                }
                            }
                        }
                    }
                    Err(anyhow!("no response for id {id} in SSE stream"))
                } else {
                    Ok(serde_json::from_str(&body).context("parsing MCP response")?)
                }
            }
            Transport::Stdio { io } => {
                let mut io = io.lock().await;
                io.stdin
                    .write_all(format!("{request}\n").as_bytes())
                    .await?;
                io.stdin.flush().await?;
                // Newline-delimited JSON; skip notifications/other traffic
                // until our id shows up.
                loop {
                    let mut line = String::new();
                    let n = io.stdout.read_line(&mut line).await?;
                    if n == 0 {
                        return Err(anyhow!("MCP server closed stdout"));
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        return Ok(msg);
                    }
                }
            }
        }
    }

    /// The resource listing as (ordered filenames, filename → resource),
    /// cached briefly and paginated to completion.
    async fn listing(&self) -> Result<(Vec<String>, HashMap<String, Resource>)> {
        if let Some((at, map, order)) = self.listing.read().await.as_ref() {
            if at.elapsed() < LIST_TTL {
                return Ok((order.clone(), map.clone()));
            }
        }
        let mut resources: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let result = self.rpc("resources/list", params).await?;
            if let Some(page) = result.get("resources").and_then(|r| r.as_array()) {
                resources.extend(page.iter().cloned());
            }
            cursor = result
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .map(String::from);
            if cursor.is_none() || resources.len() >= MAX_RESOURCES {
                if resources.len() >= MAX_RESOURCES {
                    tracing::warn!("MCP listing capped at {MAX_RESOURCES} resources");
                }
                break;
            }
        }

        let mut map = HashMap::new();
        let mut order = Vec::new();
        for res in &resources {
            let Some(uri) = res.get("uri").and_then(|u| u.as_str()) else {
                continue;
            };
            let mime = res
                .get("mimeType")
                .and_then(|m| m.as_str())
                .map(String::from);
            let base = res
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|n| !n.is_empty())
                .map(String::from)
                .unwrap_or_else(|| uri.rsplit('/').next().unwrap_or(uri).to_string());
            let mut name = filename_for(&base, mime.as_deref());
            // Uniquify: duplicate/empty names corrupt NFS READDIR replies.
            if name.is_empty() || map.contains_key(&name) {
                let mut hash: u64 = 0xcbf29ce484222325;
                for b in uri.bytes() {
                    hash ^= b as u64;
                    hash = hash.wrapping_mul(0x100000001b3);
                }
                let (stem, ext) = match name.rsplit_once('.') {
                    Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                    _ => (name.clone(), String::new()),
                };
                name = format!("{stem}-{hash:08x}{ext}");
                if map.contains_key(&name) {
                    continue;
                }
            }
            order.push(name.clone());
            map.insert(
                name,
                Resource {
                    uri: uri.to_string(),
                    mime,
                },
            );
        }
        *self.listing.write().await = Some((Instant::now(), map.clone(), order.clone()));
        Ok((order, map))
    }
}

/// Filesystem-safe filename with a mime-derived extension when the source
/// name has none.
fn filename_for(base: &str, mime: Option<&str>) -> String {
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').trim_matches('.').to_string();
    if cleaned.contains('.') {
        return cleaned;
    }
    let ext = match mime.unwrap_or("") {
        m if m.contains("markdown") => ".md",
        m if m.contains("json") => ".json",
        m if m.contains("html") => ".html",
        m if m.contains("csv") => ".csv",
        _ => ".txt",
    };
    format!("{cleaned}{ext}")
}

#[async_trait]
impl Source for McpSource {
    async fn list(&self, path: &Path) -> Result<Vec<SourceEntry>> {
        if path.as_os_str().is_empty() || path == Path::new("/") {
            let (order, _) = self.listing().await?;
            return Ok(order
                .into_iter()
                .map(|name| SourceEntry {
                    name,
                    is_dir: false,
                })
                .collect());
        }
        Err(anyhow!("MCP resources form a flat directory"))
    }

    async fn read(&self, path: &Path) -> Result<Bytes> {
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("no filename"))?
            .to_string_lossy()
            .to_string();
        let (_, map) = self.listing().await?;
        let resource = map
            .get(&name)
            .ok_or_else(|| anyhow!("no MCP resource named {name}"))?;
        let result = self
            .rpc("resources/read", json!({"uri": resource.uri}))
            .await?;
        let contents = result
            .get("contents")
            .and_then(|c| c.as_array())
            .ok_or_else(|| anyhow!("malformed resources/read reply"))?;
        let mut out = Vec::new();
        for part in contents {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                out.extend_from_slice(text.as_bytes());
            } else if let Some(blob) = part.get("blob").and_then(|b| b.as_str()) {
                out.extend_from_slice(
                    &base64::engine::general_purpose::STANDARD
                        .decode(blob)
                        .context("decoding MCP blob")?,
                );
            }
        }
        Ok(Bytes::from(out))
    }

    async fn stat(&self, path: &Path) -> Result<SourceMeta> {
        let now = SystemTime::now();
        if path.as_os_str().is_empty() || path == Path::new("/") {
            return Ok(SourceMeta {
                size: 0,
                is_dir: true,
                modified: now,
                created: now,
            });
        }
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("no filename"))?
            .to_string_lossy()
            .to_string();
        let (_, map) = self.listing().await?;
        if !map.contains_key(&name) {
            return Err(anyhow!("no MCP resource named {name}"));
        }
        Ok(SourceMeta {
            size: 1024 * 1024, // unknown until read; same placeholder as ApiSource
            is_dir: false,
            modified: now,
            created: now,
        })
    }

    async fn watch(&self) -> Result<Pin<Box<dyn Stream<Item = ChangeEvent> + Send>>> {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    fn supports_search(&self) -> bool {
        false // MCP has no standard search — HearthFS indexes locally instead
    }

    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SourceSearchResult>> {
        Ok(vec![])
    }

    fn skip_indexing(&self) -> bool {
        // The point: local FTS+embeddings give MCP servers semantic search
        // they don't natively have.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames() {
        assert_eq!(filename_for("My Notes", Some("text/markdown")), "My-Notes.md");
        assert_eq!(filename_for("report.pdf", None), "report.pdf");
        assert_eq!(filename_for("data", Some("application/json")), "data.json");
        assert_eq!(filename_for("x/y:z", None), "x-y-z.txt");
    }
}
