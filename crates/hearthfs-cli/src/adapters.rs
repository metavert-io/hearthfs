//! The adapter registry: every way HearthFS can mount an external source,
//! with the config-field schemas the CLI prompts for and the app's
//! add-volume wizard renders. Single source of truth — the UI consumes
//! `hearthfs adapters --json`, so CLI and UI can never disagree.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

/// How a field is entered/rendered. The UI maps these to controls
/// (path → folder picker, secret → SecureField via `hearthfs auth`, etc.).
#[derive(Clone, Copy, PartialEq)]
pub enum FieldKind {
    Path,
    Url,
    Text,
}

impl FieldKind {
    fn as_str(self) -> &'static str {
        match self {
            FieldKind::Path => "path",
            FieldKind::Url => "url",
            FieldKind::Text => "text",
        }
    }
}

pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    pub help: &'static str,
    /// Runtime-discovered default (e.g. an Obsidian vault path), if any.
    pub default: fn() -> Option<String>,
}

pub struct AdapterSpec {
    pub kind: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub fields: &'static [FieldSpec],
    /// Secret service consumed by this adapter (`hearthfs auth <service>`),
    /// with the expected key prefixes for validation.
    pub secret: Option<SecretSpec>,
    pub transports: &'static [&'static str],
    pub default_transport: &'static str,
}

pub struct SecretSpec {
    pub service: &'static str,
    pub key_prefixes: &'static [&'static str],
    pub hint: &'static str,
}

fn no_default() -> Option<String> {
    None
}

/// First Obsidian vault path from the app's own vault registry.
fn obsidian_default() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let reg = PathBuf::from(&home).join("Library/Application Support/obsidian/obsidian.json");
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(reg).ok()?).ok()?;
    json.get("vaults")?
        .as_object()?
        .values()
        .filter_map(|v| v.get("path").and_then(|p| p.as_str()))
        .next()
        .map(String::from)
}

fn gbrain_default() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(&home).join(".gbrain");
    dir.is_dir().then(|| dir.to_string_lossy().to_string())
}

fn hearthloom_default() -> Option<String> {
    Some(hearthfs_source_hearthloom::DEFAULT_BASE_URL.to_string())
}

fn manifest_path_default() -> Option<String> {
    Some("/api/discovery/manifest".to_string())
}

const LOCAL_TRANSPORTS: &[&str] = &["fskit", "nfs"];
const API_TRANSPORTS: &[&str] = &["nfs"];

pub fn registry() -> Vec<AdapterSpec> {
    vec![
        AdapterSpec {
            kind: "folder",
            display_name: "Local Folder",
            description: "Index and mount any folder on this Mac (notes, docs, code).",
            fields: &[FieldSpec {
                key: "path",
                label: "Source folder",
                kind: FieldKind::Path,
                required: true,
                help: "The folder to index; originals are never modified.",
                default: no_default,
            }],
            secret: None,
            transports: LOCAL_TRANSPORTS,
            default_transport: "fskit",
        },
        AdapterSpec {
            kind: "obsidian",
            display_name: "Obsidian Vault",
            description: "Mount an Obsidian vault — plain-markdown local-first notes.",
            fields: &[FieldSpec {
                key: "path",
                label: "Vault folder",
                kind: FieldKind::Path,
                required: true,
                help: "Your vault's root folder (auto-detected when Obsidian is installed).",
                default: obsidian_default,
            }],
            secret: None,
            transports: LOCAL_TRANSPORTS,
            default_transport: "fskit",
        },
        AdapterSpec {
            kind: "logseq",
            display_name: "Logseq Graph",
            description: "Mount a Logseq graph folder (outliner-style local markdown).",
            fields: &[FieldSpec {
                key: "path",
                label: "Graph folder",
                kind: FieldKind::Path,
                required: true,
                help: "The graph's root folder (contains pages/ and journals/).",
                default: no_default,
            }],
            secret: None,
            transports: LOCAL_TRANSPORTS,
            default_transport: "fskit",
        },
        AdapterSpec {
            kind: "gbrain",
            display_name: "GBrain",
            description: "Mount a GBrain brain — its markdown git repo is the system of \
                          record, so HearthFS indexes it directly (no server needed).",
            fields: &[FieldSpec {
                key: "path",
                label: "Brain repo folder",
                kind: FieldKind::Path,
                required: true,
                help: "The markdown git repo backing your brain (local PGLite brains \
                       live under ~/.gbrain).",
                default: gbrain_default,
            }],
            secret: None,
            transports: LOCAL_TRANSPORTS,
            default_transport: "fskit",
        },
        AdapterSpec {
            kind: "hearthloom",
            display_name: "Hearthloom Brain",
            description: "Mount your Hearthloom second brain: BRAIN.md, entity dossiers, \
                          day recall, goals — always the vault-locked view.",
            fields: &[
                FieldSpec {
                    key: "url",
                    label: "Brain URL",
                    kind: FieldKind::Url,
                    required: false,
                    help: "Your local daemon (the default) or a hosted brain, \
                           e.g. https://brain.example.com",
                    default: hearthloom_default,
                },
                FieldSpec {
                    key: "auth_service",
                    label: "Key name",
                    kind: FieldKind::Text,
                    required: false,
                    help: "Leave empty to use the default hearthloom key. For a \
                           second brain (e.g. cloud + local), store its key under \
                           its own name (hearthfs auth <name>) and put that here.",
                    default: no_default,
                },
            ],
            secret: Some(SecretSpec {
                service: "hearthloom",
                key_prefixes: &["hl_"],
                hint: "Mint a read-scoped key in Hearthloom: menu → API Keys.",
            }),
            transports: API_TRANSPORTS,
            default_transport: "nfs",
        },
        AdapterSpec {
            kind: "git",
            display_name: "Git Repository",
            description: "Clone any git repository locally and index it — full semantic \
                          search over GitHub/GitLab/Bitbucket/self-hosted repos, public \
                          or private (your own git credentials are used).",
            fields: &[
                FieldSpec {
                    key: "url",
                    label: "Repository URL",
                    kind: FieldKind::Url,
                    required: true,
                    help: "https or ssh clone URL, e.g. https://github.com/owner/repo.git",
                    default: no_default,
                },
                FieldSpec {
                    key: "ref",
                    label: "Branch",
                    kind: FieldKind::Text,
                    required: false,
                    help: "Branch or tag to track (default: the remote's default branch).",
                    default: no_default,
                },
            ],
            secret: None,
            transports: LOCAL_TRANSPORTS,
            default_transport: "fskit",
        },
        AdapterSpec {
            kind: "api",
            display_name: "Custom API (manifest)",
            description: "Mount any service that serves a HearthFS discovery manifest — \
                          the open integration point (Hearthloom is one of these).",
            fields: &[
                FieldSpec {
                    key: "url",
                    label: "Base URL",
                    kind: FieldKind::Url,
                    required: true,
                    help: "The service's base URL, e.g. http://127.0.0.1:8080",
                    default: no_default,
                },
                FieldSpec {
                    key: "manifest_path",
                    label: "Manifest path",
                    kind: FieldKind::Text,
                    required: false,
                    help: "Where the service serves its discovery manifest.",
                    default: manifest_path_default,
                },
                FieldSpec {
                    key: "auth_service",
                    label: "Key name",
                    kind: FieldKind::Text,
                    required: false,
                    help: "Leave empty for open services. Otherwise store a key first \
                           (hearthfs auth <name>) and put that name here — keys never \
                           enter volumes.json.",
                    default: no_default,
                },
            ],
            secret: None,
            transports: API_TRANSPORTS,
            default_transport: "nfs",
        },
        AdapterSpec {
            kind: "mcp",
            display_name: "MCP Server",
            description: "Mount any Model Context Protocol server's resources as files — \
                          and get HearthFS's semantic search over them (MCP has none). \
                          One adapter, the whole MCP ecosystem.",
            fields: &[
                FieldSpec {
                    key: "url",
                    label: "Server URL",
                    kind: FieldKind::Url,
                    required: false,
                    help: "Streamable-HTTP endpoint, e.g. http://127.0.0.1:8080/mcp \
                           (leave empty when using a command).",
                    default: no_default,
                },
                FieldSpec {
                    key: "command",
                    label: "Command",
                    kind: FieldKind::Text,
                    required: false,
                    help: "stdio server command, e.g. \
                           npx -y @modelcontextprotocol/server-filesystem ~/docs \
                           (leave empty when using a URL).",
                    default: no_default,
                },
                FieldSpec {
                    key: "auth_service",
                    label: "Key name",
                    kind: FieldKind::Text,
                    required: false,
                    help: "For authenticated HTTP servers: store a bearer token first \
                           (hearthfs auth <name>) and put that name here.",
                    default: no_default,
                },
            ],
            secret: None,
            transports: API_TRANSPORTS,
            default_transport: "nfs",
        },
        AdapterSpec {
            kind: "github",
            display_name: "GitHub",
            description: "Browse and read GitHub repositories read-only; search is \
                          delegated to GitHub code search.",
            fields: &[FieldSpec {
                key: "repos",
                label: "Repositories",
                kind: FieldKind::Text,
                required: true,
                help: "Comma-separated owner/repo list, e.g. owner/repo,rust-lang/rust",
                default: no_default,
            }],
            secret: Some(SecretSpec {
                service: "github",
                key_prefixes: &["ghp_", "github_pat_"],
                hint: "A fine-grained PAT with read-only Contents access: \
                       github.com → Settings → Developer settings → Tokens.",
            }),
            transports: API_TRANSPORTS,
            default_transport: "nfs",
        },
    ]
}

pub fn find(kind: &str) -> Result<AdapterSpec> {
    registry()
        .into_iter()
        .find(|a| a.kind == kind)
        .ok_or_else(|| {
            anyhow!(
                "unknown adapter '{kind}' (available: {})",
                registry()
                    .iter()
                    .map(|a| a.kind)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The `hearthfs mount` source-spec string for a configured adapter — the
/// invariant that keeps every volume mountable by hand from the CLI.
pub fn source_spec(adapter: &AdapterSpec, fields: &HashMap<String, String>) -> Result<String> {
    let get = |key: &str| -> Result<&str> {
        fields
            .get(key)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("adapter '{}' needs field '{key}'", adapter.kind))
    };
    Ok(match adapter.kind {
        "folder" | "obsidian" | "logseq" | "gbrain" => get("path")?.to_string(),
        "hearthloom" => match fields.get("url").map(|s| s.as_str()).unwrap_or("") {
            "" => "hearthloom".to_string(),
            url if url.starts_with("https://") => format!(
                "hearthloom+https://{}",
                url.trim_start_matches("https://").trim_end_matches('/')
            ),
            url => format!(
                "hearthloom://{}",
                url.trim_start_matches("http://").trim_end_matches('/')
            ),
        },
        "github" => format!("github:{}", get("repos")?.replace(' ', "")),
        "mcp" => match (
            fields.get("url").filter(|s| !s.is_empty()),
            fields.get("command").filter(|s| !s.is_empty()),
        ) {
            (Some(url), _) => format!("mcp:{}", url.trim_end_matches('/')),
            (None, Some(cmd)) => format!("mcp:cmd:{cmd}"),
            (None, None) => {
                return Err(anyhow!("mcp adapter needs either a url or a command field"))
            }
        },
        "git" => match fields.get("ref").map(|s| s.as_str()).unwrap_or("") {
            "" => format!("git:{}", get("url")?),
            gref => format!("git:{}#{gref}", get("url")?),
        },
        "api" => format!("api:{}", get("url")?.trim_end_matches('/')),
        other => return Err(anyhow!("adapter '{other}' has no source spec")),
    })
}

/// Extra `hearthfs mount` flags a configured volume needs beyond the spec —
/// how api-adapter volumes carry manifest path and key *name* (never the
/// key itself) into the spawned mount.
pub fn extra_mount_args(adapter: &AdapterSpec, fields: &HashMap<String, String>) -> Vec<String> {
    let mut args = Vec::new();
    if adapter.kind == "api" {
        if let Some(mp) = fields.get("manifest_path").filter(|s| !s.is_empty()) {
            args.push("--manifest-path".into());
            args.push(mp.clone());
        }
    }
    if matches!(adapter.kind, "api" | "mcp" | "hearthloom") {
        if let Some(svc) = fields.get("auth_service").filter(|s| !s.is_empty()) {
            args.push("--auth-service".into());
            args.push(svc.clone());
        }
    }
    args
}

/// Clone (or refresh) a git source into the local clone cache and return
/// the working-tree path. Refresh failures are non-fatal — an offline
/// mount serves the last-fetched state, which is the local-first behavior.
pub fn prepare_git(url: &str, gref: Option<&str>) -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("cannot determine home directory"))?;
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in format!("{url}#{}", gref.unwrap_or("")).bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let dir = std::path::PathBuf::from(home)
        .join(".hearthfs/clones")
        .join(format!("{hash:016x}"));

    let git = |args: &[&str]| -> Result<std::process::Output> {
        Ok(std::process::Command::new("git").args(args).output()?)
    };

    if dir.join(".git").exists() {
        let d = dir.to_string_lossy().to_string();
        let fetched = git(&["-C", &d, "fetch", "--depth", "1", "origin"])?;
        if fetched.status.success() {
            let target = match gref {
                Some(r) => format!("origin/{r}"),
                None => "FETCH_HEAD".to_string(),
            };
            let reset = git(&["-C", &d, "reset", "--hard", &target])?;
            if !reset.status.success() {
                tracing::warn!(
                    "git refresh failed for {url}: {}",
                    String::from_utf8_lossy(&reset.stderr)
                );
            }
        } else {
            tracing::warn!(
                "git fetch failed for {url} (offline?) — mounting last-fetched state: {}",
                String::from_utf8_lossy(&fetched.stderr)
            );
        }
    } else {
        std::fs::create_dir_all(dir.parent().unwrap())?;
        let d = dir.to_string_lossy().to_string();
        let mut args = vec!["clone", "--depth", "1", "--single-branch"];
        if let Some(r) = gref {
            args.extend(["--branch", r]);
        }
        args.extend([url, d.as_str()]);
        let out = git(&args)?;
        if !out.status.success() {
            return Err(anyhow!(
                "git clone {url} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
    Ok(dir)
}

/// Registry as JSON for the app's wizard (`hearthfs adapters --json`).
pub fn registry_json() -> serde_json::Value {
    serde_json::json!({
        "adapters": registry().iter().map(|a| serde_json::json!({
            "kind": a.kind,
            "display_name": a.display_name,
            "description": a.description,
            "fields": a.fields.iter().map(|f| serde_json::json!({
                "key": f.key,
                "label": f.label,
                "kind": f.kind.as_str(),
                "required": f.required,
                "help": f.help,
                "default": (f.default)(),
            })).collect::<Vec<_>>(),
            "secret": a.secret.as_ref().map(|s| serde_json::json!({
                "service": s.service,
                "key_prefixes": s.key_prefixes,
                "hint": s.hint,
                "configured": crate::secret_key_path(s.service)
                    .map(|p| p.exists())
                    .unwrap_or(false),
            })),
            "transports": a.transports,
            "default_transport": a.default_transport,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn hearthloom_spec_http_https_and_default() {
        let a = find("hearthloom").unwrap();
        assert_eq!(source_spec(&a, &fields(&[])).unwrap(), "hearthloom");
        assert_eq!(
            source_spec(&a, &fields(&[("url", "http://127.0.0.1:43278")])).unwrap(),
            "hearthloom://127.0.0.1:43278"
        );
        assert_eq!(
            source_spec(&a, &fields(&[("url", "https://brain.example.com/")])).unwrap(),
            "hearthloom+https://brain.example.com"
        );
    }
}
