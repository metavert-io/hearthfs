//! Persisted volume configuration — the shared state behind `hearthfs
//! volume …` and the app's Volumes page. One file, no secrets in it
//! (secrets live as 0600 key files referenced by service name).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::adapters;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    pub name: String,
    pub adapter: String,
    #[serde(default)]
    pub fields: HashMap<String, String>,
    pub mountpoint: String,
    /// "fskit" (local folders on macOS 26+) or "nfs" (everything else).
    pub transport: String,
    /// NFS server port for nfs-transport volumes.
    pub port: u16,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VolumesFile {
    #[serde(default)]
    pub volumes: Vec<VolumeConfig>,
}

const FIRST_PORT: u16 = 11200;

pub fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("cannot determine home directory")?;
    Ok(PathBuf::from(home).join(".config/hearthfs/volumes.json"))
}

fn runtime_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("cannot determine home directory")?;
    Ok(PathBuf::from(home).join(".hearthfs/volumes"))
}

pub fn load() -> Result<VolumesFile> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(VolumesFile::default());
    }
    let data =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))
}

pub fn save(file: &VolumesFile) -> Result<()> {
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(file)?)?;
    Ok(())
}

pub fn add(
    adapter_kind: &str,
    name: Option<String>,
    mountpoint: Option<String>,
    transport: Option<String>,
    field_args: &[String],
) -> Result<VolumeConfig> {
    let adapter = adapters::find(adapter_kind)?;

    // Fields: --field key=value args, then discovered defaults, then error.
    let mut fields: HashMap<String, String> = HashMap::new();
    for arg in field_args {
        let (k, v) = arg
            .split_once('=')
            .ok_or_else(|| anyhow!("--field takes key=value, got '{arg}'"))?;
        fields.insert(k.to_string(), v.to_string());
    }
    for spec in adapter.fields {
        if !fields.contains_key(spec.key) {
            if let Some(default) = (spec.default)() {
                fields.insert(spec.key.to_string(), default);
            } else if spec.required {
                return Err(anyhow!(
                    "adapter '{}' needs --field {}=<{}>  ({})",
                    adapter.kind,
                    spec.key,
                    spec.label.to_lowercase().replace(' ', "-"),
                    spec.help
                ));
            }
        }
    }
    // Local paths must exist and be absolute before we persist them.
    if let Some(path) = fields.get("path") {
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("source folder '{path}' not accessible"))?;
        fields.insert("path".into(), canonical.to_string_lossy().to_string());
    }
    // Validate now so `volume add` fails fast, not at mount time.
    let spec = adapters::source_spec(&adapter, &fields)?;
    let has_named_key = fields.get("auth_service").is_some_and(|s| !s.is_empty());
    if let Some(secret) = adapter.secret.as_ref().filter(|_| !has_named_key) {
        let configured = crate::secret_key_path(secret.service)
            .map(|p| p.exists())
            .unwrap_or(false);
        if !configured && std::env::var("HEARTHFS_API_KEY").is_err() {
            return Err(anyhow!(
                "adapter '{}' needs a stored key — run: hearthfs auth {}\n({})",
                adapter.kind,
                secret.service,
                secret.hint
            ));
        }
    }
    // Generic api volumes reference a key by name; make sure it exists.
    if let Some(svc) = fields.get("auth_service").filter(|s| !s.is_empty()) {
        let configured = crate::secret_key_path(svc)
            .map(|p| p.exists())
            .unwrap_or(false);
        if !configured {
            return Err(anyhow!(
                "no key stored under '{svc}' — run: hearthfs auth {svc}"
            ));
        }
    }

    let transport = transport.unwrap_or_else(|| adapter.default_transport.to_string());
    if !adapter.transports.contains(&transport.as_str()) {
        return Err(anyhow!(
            "adapter '{}' supports transports {:?}, not '{transport}'",
            adapter.kind,
            adapter.transports
        ));
    }

    let name = name.unwrap_or_else(|| default_name(&adapter, &fields));
    let mut file = load()?;
    if file.volumes.iter().any(|v| v.name == name) {
        return Err(anyhow!(
            "volume '{name}' already exists (hearthfs volume remove {name})"
        ));
    }
    // NOT plain "~/HearthFS": on case-insensitive volumes that resolves to
    // a "~/hearthfs" dir if one exists (such as a clone of this repo).
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let mountpoint = mountpoint.unwrap_or_else(|| format!("{home}/HearthFSmounts/{name}"));
    let port = file
        .volumes
        .iter()
        .map(|v| v.port)
        .max()
        .map(|p| p + 1)
        .unwrap_or(FIRST_PORT);

    let volume = VolumeConfig {
        name,
        adapter: adapter.kind.to_string(),
        fields,
        mountpoint,
        transport,
        port,
    };
    tracing::debug!(source = %spec, "validated volume source spec");
    file.volumes.push(volume.clone());
    save(&file)?;
    Ok(volume)
}

fn default_name(adapter: &adapters::AdapterSpec, fields: &HashMap<String, String>) -> String {
    let base = match adapter.kind {
        "folder" | "obsidian" | "logseq" | "gbrain" => fields
            .get("path")
            .and_then(|p| {
                PathBuf::from(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| adapter.kind.to_string()),
        "github" => fields
            .get("repos")
            .and_then(|r| r.split(',').next().map(|s| s.replace('/', "-")))
            .unwrap_or_else(|| "github".to_string()),
        other => other.to_string(),
    };
    // Dot-directories (~/.gbrain) must not become hidden volume names.
    let cleaned = base.trim_start_matches('.');
    let cleaned = if cleaned.is_empty() {
        adapter.kind
    } else {
        cleaned
    };
    cleaned.to_lowercase().replace(' ', "-")
}

pub fn remove(name: &str) -> Result<()> {
    let mut file = load()?;
    let before = file.volumes.len();
    file.volumes.retain(|v| v.name != name);
    if file.volumes.len() == before {
        return Err(anyhow!("no volume named '{name}'"));
    }
    save(&file)
}

pub fn get(name: &str) -> Result<VolumeConfig> {
    load()?
        .volumes
        .into_iter()
        .find(|v| v.name == name)
        .ok_or_else(|| anyhow!("no volume named '{name}' (hearthfs volume list)"))
}

pub fn is_mounted(mountpoint: &str) -> bool {
    std::process::Command::new("/sbin/mount")
        .output()
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            let canon = std::fs::canonicalize(mountpoint)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| mountpoint.to_string());
            out.lines().any(|l| l.contains(&format!(" on {canon} ")))
        })
        .unwrap_or(false)
}

/// Mount a configured volume. FSKit transport goes through `mount -F`;
/// NFS transport spawns a detached `hearthfs mount` server (log + pid under
/// ~/.hearthfs/volumes/).
pub fn mount(name: &str) -> Result<()> {
    let vol = get(name)?;
    if is_mounted(&vol.mountpoint) {
        return Err(anyhow!("'{name}' is already mounted at {}", vol.mountpoint));
    }
    let adapter = adapters::find(&vol.adapter)?;
    let spec = adapters::source_spec(&adapter, &vol.fields)?;
    std::fs::create_dir_all(&vol.mountpoint)
        .with_context(|| format!("creating mountpoint {}", vol.mountpoint))?;

    match vol.transport.as_str() {
        "fskit" => {
            // The FSKit extension takes a local directory — resolve git
            // specs to their clone path first.
            let fskit_source = match spec.strip_prefix("git:") {
                Some(rest) => {
                    let (url, gref) = match rest.rsplit_once('#') {
                        Some((u, r)) if !r.contains('/') => (u, Some(r)),
                        _ => (rest, None),
                    };
                    adapters::prepare_git(url, gref)?
                        .to_string_lossy()
                        .to_string()
                }
                None => spec.clone(),
            };
            let out = std::process::Command::new("/sbin/mount")
                .args(["-F", "-t", "hearthfs", &fskit_source, &vol.mountpoint])
                .output()?;
            if !out.status.success() {
                return Err(anyhow!(
                    "mount -F failed (is the HearthFS extension enabled in System \
                     Settings, macOS 26.5.2+?): {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }
        _ => {
            let dir = runtime_dir()?;
            std::fs::create_dir_all(&dir)?;
            let log = std::fs::File::create(dir.join(format!("{name}.log")))?;
            let mut args = vec![
                "mount".to_string(),
                spec.clone(),
                vol.mountpoint.clone(),
                "--port".to_string(),
                vol.port.to_string(),
            ];
            args.extend(adapters::extra_mount_args(&adapter, &vol.fields));
            let child = std::process::Command::new(std::env::current_exe()?)
                .args(&args)
                .stdout(log.try_clone()?)
                .stderr(log)
                .stdin(std::process::Stdio::null())
                .spawn()?;
            std::fs::write(dir.join(format!("{name}.pid")), child.id().to_string())?;
            // The server mounts asynchronously; give it a moment and report.
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if is_mounted(&vol.mountpoint) {
                    break;
                }
            }
            if !is_mounted(&vol.mountpoint) {
                return Err(anyhow!(
                    "server started but mount hasn't appeared — check {}",
                    dir.join(format!("{name}.log")).display()
                ));
            }
        }
    }
    eprintln!("Mounted '{name}' at {}", vol.mountpoint);
    Ok(())
}

pub fn unmount(name: &str) -> Result<()> {
    let vol = get(name)?;
    if is_mounted(&vol.mountpoint) {
        let out = std::process::Command::new("/sbin/umount")
            .arg(&vol.mountpoint)
            .output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "umount {} failed: {}",
                vol.mountpoint,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
    // Reap the NFS server if we spawned one.
    let pid_path = runtime_dir()?.join(format!("{name}.pid"));
    if let Ok(pid) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid.trim().parse::<u32>() {
            let _ = std::process::Command::new("/bin/kill")
                .arg(pid.to_string())
                .output();
        }
        let _ = std::fs::remove_file(&pid_path);
    }
    // Tidy the (empty) mountpoint so unmounted volumes don't litter Finder.
    let _ = std::fs::remove_dir(&vol.mountpoint);
    eprintln!("Unmounted '{name}'");
    Ok(())
}

/// One line per volume for `hearthfs volume list`; `--json` mode returns the
/// raw config plus mount state for the app.
pub fn list_json() -> Result<serde_json::Value> {
    let file = load()?;
    Ok(serde_json::json!({
        "volumes": file.volumes.iter().map(|v| serde_json::json!({
            "name": v.name,
            "adapter": v.adapter,
            "fields": v.fields,
            "mountpoint": v.mountpoint,
            "transport": v.transport,
            "port": v.port,
            "mounted": is_mounted(&v.mountpoint),
        })).collect::<Vec<_>>(),
    }))
}
