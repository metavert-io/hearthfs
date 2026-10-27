use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use nfs3_server::tcp::{NFSTcp, NFSTcpListener};

mod adapters;
mod volumes;

#[derive(Parser)]
#[command(name = "hearthfs", version, about = "Local-first semantic filesystem for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a source directory or smart source as a HearthFS filesystem
    ///
    /// Examples:
    ///   hearthfs mount /path/to/dir ~/mnt/local
    ///   hearthfs mount hearthloom ~/mnt/brain
    ///   hearthfs mount hearthloom://127.0.0.1:43278 ~/mnt/brain
    Mount {
        /// Source: local path, or `hearthloom` / hearthloom://host:port
        source: String,
        /// Path to the mountpoint
        mountpoint: String,
        /// Port to bind the NFS server on (default: 11111)
        #[arg(long, default_value = "11111")]
        port: u16,
        /// API key for authenticated smart sources (or HEARTHFS_API_KEY;
        /// falls back to the key stored by `hearthfs auth hearthloom`)
        #[arg(long, env = "HEARTHFS_API_KEY")]
        api_key: Option<String>,
        /// Manifest path for generic `api:` sources
        #[arg(long, default_value = "/api/discovery/manifest")]
        manifest_path: String,
        /// Load the key stored by `hearthfs auth <name>` (keeps secrets off
        /// argv — preferred over --api-key for `api:` sources)
        #[arg(long)]
        auth_service: Option<String>,
    },
    /// Store an API key for a smart source (prompts on stdin; written 0600)
    ///
    /// Example: hearthfs auth hearthloom
    Auth {
        /// The service to store a key for (hearthloom, github)
        service: String,
    },
    /// List available source adapters (what the app's wizard renders)
    Adapters {
        /// Emit the machine-readable registry (field schemas, defaults)
        #[arg(long)]
        json: bool,
    },
    /// Manage configured volumes (persisted in ~/.config/hearthfs/volumes.json)
    #[command(subcommand)]
    Volume(VolumeCommand),
    /// Show running mounts and indexing progress
    Status,
    /// Show event log statistics
    Stats,
    /// Unmount a HearthFS filesystem
    Unmount {
        /// Path to the mountpoint
        mountpoint: String,
    },
}

#[derive(Subcommand)]
enum VolumeCommand {
    /// Add a volume (validates config; does not mount)
    ///
    /// Example: hearthfs volume add github --field repos=owner/repo
    Add {
        /// Adapter kind (see: hearthfs adapters)
        adapter: String,
        /// Volume name (default: derived from the source)
        #[arg(long)]
        name: Option<String>,
        /// Mountpoint (default: ~/HearthFSmounts/<name>)
        #[arg(long)]
        mountpoint: Option<String>,
        /// Transport override: fskit | nfs (default: adapter's default)
        #[arg(long)]
        transport: Option<String>,
        /// Adapter config fields as key=value (repeatable)
        #[arg(long = "field")]
        fields: Vec<String>,
    },
    /// List volumes and their mount state
    List {
        /// Machine-readable output for the app
        #[arg(long)]
        json: bool,
    },
    /// Remove a volume's configuration (unmount first if mounted)
    Remove { name: String },
    /// Mount a configured volume
    Mount { name: String },
    /// Unmount a configured volume
    Unmount { name: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Die quietly when downstream pipes close (hearthfs volume list | head).
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hearthfs=info,nfs3_server=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Auth { service } => {
            return store_api_key(&service);
        }
        Command::Adapters { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&adapters::registry_json())?);
            } else {
                for a in adapters::registry() {
                    let auth = match &a.secret {
                        Some(s) => format!("  (auth: hearthfs auth {})", s.service),
                        None => String::new(),
                    };
                    println!("{:<12} {}{}", a.kind, a.description, auth);
                }
            }
            return Ok(());
        }
        Command::Volume(cmd) => {
            match cmd {
                VolumeCommand::Add {
                    adapter,
                    name,
                    mountpoint,
                    transport,
                    fields,
                } => {
                    let vol = volumes::add(&adapter, name, mountpoint, transport, &fields)?;
                    eprintln!(
                        "Added volume '{}' ({} → {}). Mount with: hearthfs volume mount {}",
                        vol.name, vol.adapter, vol.mountpoint, vol.name
                    );
                }
                VolumeCommand::List { json } => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&volumes::list_json()?)?);
                    } else {
                        for v in volumes::load()?.volumes {
                            let state = if volumes::is_mounted(&v.mountpoint) {
                                "mounted"
                            } else {
                                "not mounted"
                            };
                            println!(
                                "{:<20} {:<11} {:<7} {:<12} {}",
                                v.name, v.adapter, v.transport, state, v.mountpoint
                            );
                        }
                    }
                }
                VolumeCommand::Remove { name } => {
                    let vol = volumes::get(&name)?;
                    if volumes::is_mounted(&vol.mountpoint) {
                        volumes::unmount(&name)?;
                    }
                    volumes::remove(&name)?;
                    eprintln!("Removed volume '{name}'");
                }
                VolumeCommand::Mount { name } => volumes::mount(&name)?,
                VolumeCommand::Unmount { name } => volumes::unmount(&name)?,
            }
            return Ok(());
        }
        Command::Mount {
            source,
            mountpoint,
            port,
            api_key,
            manifest_path,
            auth_service,
        } => {
            let source_arc: Arc<dyn hearthfs_core::source::Source> = if source == "hearthloom"
                || source.starts_with("hearthloom://")
                || source.starts_with("hearthloom+https://")
            {
                // hearthloom                  → the daemon's default local address
                // hearthloom://host:port      → http://host:port (local daemon)
                // hearthloom+https://host     → https://host (cloud brain)
                let base_url = if let Some(rest) = source.strip_prefix("hearthloom+https://") {
                    format!("https://{rest}")
                } else {
                    match source.strip_prefix("hearthloom://") {
                        Some(rest) if !rest.is_empty() => format!("http://{rest}"),
                        _ => hearthfs_source_hearthloom::DEFAULT_BASE_URL.to_string(),
                    }
                };
                // Key resolution: explicit flag, then a named key (lets a
                // cloud brain's PAT live separately from the local one),
                // then the default hearthloom key file.
                let key = api_key
                    .or_else(|| {
                        auth_service
                            .as_deref()
                            .and_then(load_service_key)
                    })
                    .or_else(hearthfs_source_hearthloom::HearthloomSource::load_key)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no Hearthloom API key. Mint a read-scoped key in Hearthloom \
                             (menu → API Keys), then store it with: hearthfs auth hearthloom"
                        )
                    })?;
                tracing::info!("connecting to hearthloom at {base_url}");
                Arc::new(
                    hearthfs_source_hearthloom::HearthloomSource::connect(&base_url, &key)
                        .await?,
                )
            } else if let Some(repos) = source.strip_prefix("github:") {
                let key = api_key
                    .or_else(hearthfs_source_github::GithubSource::load_key)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no GitHub token. Mint a fine-grained PAT (read-only Contents), \
                             then store it with: hearthfs auth github"
                        )
                    })?;
                tracing::info!("connecting to github ({repos})");
                Arc::new(hearthfs_source_github::GithubSource::connect(repos, &key)?)
            } else if let Some(url) = source.strip_prefix("api:") {
                // Generic manifest-driven source — the open integration point.
                let key = api_key.or_else(|| {
                    auth_service
                        .as_deref()
                        .and_then(load_service_key)
                });
                let auth = key.map(|k| format!("Bearer {}", k.trim()));
                tracing::info!("connecting to manifest API at {url}");
                Arc::new(
                    hearthfs_core::api_source::ApiSource::new(
                        url.trim_end_matches('/').to_string(),
                        auth,
                        manifest_path.clone(),
                    )
                    .await?,
                )
            } else if let Some(spec) = source.strip_prefix("mcp:") {
                // MCP server resources as files; local indexing gives them
                // semantic search MCP doesn't natively have.
                let key = api_key.or_else(|| {
                    auth_service
                        .as_deref()
                        .and_then(load_service_key)
                });
                tracing::info!("connecting to MCP server");
                Arc::new(hearthfs_source_mcp::McpSource::connect(spec, key).await?)
            } else if let Some(rest) = source.strip_prefix("git:") {
                // Clone-and-index: full local search over any git repo.
                let (url, gref) = match rest.rsplit_once('#') {
                    Some((u, r)) if !r.contains('/') => (u, Some(r)),
                    _ => (rest, None),
                };
                let clone = adapters::prepare_git(url, gref)?;
                tracing::info!("mounting git clone of {url} at {}", clone.display());
                Arc::new(hearthfs_source_fs::FsSource::new(clone)?)
            } else {
                // Local filesystem source
                let path = source.strip_prefix("file://").unwrap_or(&source);
                // Courtesy recursion guard: a dir carrying Hearthloom's origin
                // marker IS a mounted brain — re-indexing it makes a window
                // into the brain look like user files.
                if std::path::Path::new(path)
                    .join(hearthfs_source_hearthloom::ORIGIN_MARKER)
                    .exists()
                {
                    tracing::warn!(
                        "{path} looks like a mounted Hearthloom brain \
                         ({} present) — mount it with `hearthfs mount hearthloom <mountpoint>` \
                         instead of re-indexing it as local files",
                        hearthfs_source_hearthloom::ORIGIN_MARKER
                    );
                }
                Arc::new(hearthfs_source_fs::FsSource::new(path.into())?)
            };

            let is_smart = source_arc.supports_search();

            // Initialize databases
            let db_dir = data_dir(&source);
            std::fs::create_dir_all(&db_dir)?;
            let log_path = db_dir.join("events.db");
            let event_log = Arc::new(hearthfs_core::event_log::EventLog::open(&log_path)?);

            // Build VFS
            let mut vfs_builder = hearthfs_core::passthrough::PassthroughVfs::new(source_arc.clone())
                .with_event_log(event_log.clone());

            // Only set up local FTS/embeddings for non-smart sources
            let fts;
            let embedder_and_vectors;
            if !is_smart {
                let fts_path = db_dir.join("fts.db");
                fts = Some(Arc::new(hearthfs_core::fts::FtsIndex::open(&fts_path)?));
                tracing::info!("FTS index at {}", fts_path.display());
                vfs_builder = vfs_builder.with_fts(fts.as_ref().unwrap().clone());

                // HEARTHFS_EMBEDDER (local:<dir> | voyage:<model>) wins;
                // otherwise the default local model directory.
                let embedder_result = match hearthfs_core::embeddings::Embedder::from_env() {
                    Ok(Some(e)) => Ok(e),
                    Ok(None) => hearthfs_core::embeddings::Embedder::load(&model_dir()),
                    Err(e) => Err(e),
                };
                embedder_and_vectors = match embedder_result {
                    Ok(embedder) => {
                        let dim = embedder.dim();
                        let vec_path = db_dir.join("vectors.db");
                        let vectors = Arc::new(
                            hearthfs_core::vector_store::VectorStore::open(&vec_path, dim)?,
                        );
                        // Wipes stale vectors if the model changed.
                        vectors.set_model(embedder.id())?;
                        let embedder = Arc::new(embedder);
                        tracing::info!("embedding backend {} ({dim}-dim)", embedder.id());
                        Some((embedder, vectors))
                    }
                    Err(e) => {
                        tracing::info!("no embedding backend: {e} (FTS-only search)");
                        None
                    }
                };
                if let Some((ref embedder, ref vectors)) = embedder_and_vectors {
                    vfs_builder = vfs_builder.with_embeddings(embedder.clone(), vectors.clone());
                }
            } else {
                fts = None;
                embedder_and_vectors = None;
                tracing::info!("smart source — search delegated, no local indexing");
            }
            let vfs = Arc::new(vfs_builder);

            // Background indexing (skips automatically for smart sources)
            if let Some(ref fts_arc) = fts {
                let mut indexer =
                    hearthfs_core::indexer::Indexer::new(source_arc.clone(), fts_arc.clone());
                if let Some((ref embedder, ref vectors)) = embedder_and_vectors {
                    indexer = indexer.with_embeddings(embedder.clone(), vectors.clone());
                }
                tokio::spawn(async move {
                    match indexer.full_scan().await {
                        Ok(n) => tracing::info!("initial indexing complete: {n} files"),
                        Err(e) => tracing::error!("indexing failed: {e}"),
                    }
                });
            }

            let nfs = hearthfs_nfs::HearthNfs::new(vfs);

            // A stale mount from an unclean death must be cleared BEFORE we
            // bind and remount, or every op on the mountpoint wedges
            // (2026-07-17 incident).
            ensure_mountpoint_clear(&mountpoint)?;

            // Loopback only: a mounted source must never be reachable from
            // the network.
            let listener = NFSTcpListener::bind_ro(&nfs_bind_addr(port), nfs).await?;
            let listen_ip = listener.get_listen_ip();
            let listen_port = listener.get_listen_port();

            tracing::info!("NFS server listening on {listen_ip}:{listen_port} (loopback only)");

            // Auto-mount in a background task
            let mp = std::path::Path::new(&mountpoint);
            if !mp.exists() {
                std::fs::create_dir_all(mp)?;
            }

            // Clean shutdown: unmount before exiting on SIGTERM/SIGINT so a
            // `launchctl stop`/pkill never leaves a mount with no server —
            // that stale mount is exactly what wedges the next start.
            {
                let mp = mountpoint.clone();
                tokio::spawn(async move {
                    use tokio::signal::unix::{signal, SignalKind};
                    let mut term = signal(SignalKind::terminate()).expect("signal handler");
                    let mut int = signal(SignalKind::interrupt()).expect("signal handler");
                    tokio::select! {
                        _ = term.recv() => {},
                        _ = int.recv() => {},
                    }
                    tracing::info!("shutdown signal — unmounting {mp}");
                    let _ = tokio::task::spawn_blocking(move || force_unmount(&mp)).await;
                    std::process::exit(0);
                });
            }

            let mount_mp = mountpoint.clone();
            let mount_args = build_mount_args(listen_ip, listen_port, &mount_mp);
            tracing::info!(
                "Mount with: mount_nfs {}",
                mount_args.join(" ")
            );

            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                let mount_cmd = tokio::process::Command::new("mount_nfs")
                    .args(&mount_args)
                    .output()
                    .await;

                match mount_cmd {
                    Ok(output) if output.status.success() => {
                        tracing::info!(mountpoint = %mount_mp, "mounted");
                    }
                    Ok(output) => {
                        // A server nobody can reach through the mountpoint is
                        // a silent landmine (launchd shows healthy, callers
                        // see stale state) — fail loudly instead.
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let hint = if stderr.contains("assign requested address") {
                            " (early macOS 26 builds refuse NFS mounts of 127.0.0.1; \
                             update macOS, or use the FSKit transport for local folders)"
                        } else {
                            ""
                        };
                        tracing::error!("mount_nfs failed: {stderr}{hint} — exiting");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        tracing::error!("could not run mount_nfs: {e} — exiting");
                        std::process::exit(1);
                    }
                }

                // Startup self-check: the mount must ANSWER, not just exist.
                // If a read through the kernel doesn't come back within the
                // deadline, unmount and exit nonzero so launchd records a
                // failure instead of leaving a wedged mount standing.
                let verified = tokio::task::spawn_blocking({
                    let mp = mount_mp.clone();
                    move || probe_mount(&mp, std::time::Duration::from_secs(10))
                })
                .await
                .unwrap_or(false);
                if verified {
                    tracing::info!(mountpoint = %mount_mp, "mount verified (self-check read ok)");
                } else {
                    tracing::error!(
                        mountpoint = %mount_mp,
                        "mount FAILED self-check (no answer within 10s) — unmounting and exiting"
                    );
                    let _ = tokio::task::spawn_blocking(move || force_unmount(&mount_mp)).await;
                    std::process::exit(2);
                }
            });

            // Run forever
            listener.handle_forever().await?;
            Ok(())
        }
        Command::Status => {
            // Scan ~/.hearthfs for active mounts
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            let base = PathBuf::from(&home).join(".hearthfs");
            if !base.exists() {
                println!("No HearthFS data found.");
                return Ok(());
            }
            let mut found = false;
            for entry in std::fs::read_dir(&base)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let fts_path = entry.path().join("fts.db");
                    if fts_path.exists() {
                        let fts = hearthfs_core::fts::FtsIndex::open(&fts_path)?;
                        let count = fts.file_count()?;
                        println!(
                            "  {} — {} files indexed",
                            entry.path().display(),
                            count
                        );
                        found = true;
                    }
                }
            }
            if !found {
                println!("No HearthFS indexes found.");
            }
            Ok(())
        }
        Command::Stats => {
            // Find and display stats from all event logs
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            let base = PathBuf::from(&home).join(".hearthfs");
            if !base.exists() {
                println!("No HearthFS data found.");
                return Ok(());
            }
            let mut found = false;
            for entry in std::fs::read_dir(&base)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let log_path = entry.path().join("events.db");
                if !log_path.exists() {
                    continue;
                }
                let log = hearthfs_core::event_log::EventLog::open(&log_path)?;
                let stats = log.stats()?;
                found = true;
                println!("HearthFS Stats ({})", entry.path().display());
                println!("  File reads:       {}", stats.total_reads);
                println!("  Magic path reads: {}", stats.total_magic_reads);
                println!("  Searches:         {}", stats.total_searches);
                println!("  Index events:     {}", stats.total_index_events);
                println!("  Tokens saved:     {}", stats.tokens_saved);
                if stats.total_reads > 0 {
                    println!("  Avg read latency: {:.1}ms", stats.avg_read_ms);
                }
                if stats.total_searches > 0 {
                    println!("  Avg search latency: {:.1}ms", stats.avg_search_ms);
                }
            }
            if !found {
                println!("No event logs found. Mount a directory first.");
            }
            Ok(())
        }
        Command::Unmount { mountpoint } => {
            tracing::info!(%mountpoint, "unmounting");
            let output = std::process::Command::new("umount")
                .arg(&mountpoint)
                .output()?;
            if output.status.success() {
                println!("Unmounted {mountpoint}");
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("Failed to unmount: {stderr}");
            }
            Ok(())
        }
    }
}

/// Load a stored service key (trimmed), if present.
pub(crate) fn load_service_key(service: &str) -> Option<String> {
    let key = std::fs::read_to_string(secret_key_path(service)?).ok()?;
    let key = key.trim().to_string();
    (!key.is_empty()).then_some(key)
}

/// Standard location for a service's stored key. Shared by the auth
/// command, the adapter registry (to report "configured" to the UI), and
/// the source crates that consume the keys.
pub(crate) fn secret_key_path(service: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("hearthfs")
            .join(format!("{service}.key")),
    )
}

/// Prompt for and store a smart-source API key (0600, outside any repo).
/// The key is read from stdin so it never lands in shell history or logs.
fn store_api_key(service: &str) -> Result<()> {
    // Registry services get prefix validation and a hint; any other name is
    // allowed too — that's how generic `api` volumes name their keys.
    let spec = adapters::registry()
        .into_iter()
        .find_map(|a| a.secret.filter(|s| s.service == service));
    if spec.is_none() {
        eprintln!("(custom key name '{service}' — reference it from an api volume's Key name field)");
    }
    let key_path = secret_key_path(service)
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;

    match &spec {
        Some(s) => eprint!("Paste the {service} API key ({}): ", s.hint),
        None => eprint!("Paste the API key to store as '{service}': "),
    }
    let mut key = String::new();
    std::io::stdin().read_line(&mut key)?;
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("no key entered");
    }
    if let Some(s) = &spec {
        if !s.key_prefixes.iter().any(|p| key.starts_with(p)) {
            eprintln!(
                "warning: {service} keys normally start with {} — storing anyway",
                s.key_prefixes.join(" or ")
            );
        }
    }

    if let Some(dir) = key_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&key_path, format!("{key}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("Stored at {} (0600).", key_path.display());
    Ok(())
}

/// Data directory for HearthFS indexes, stored in ~/.hearthfs/<hash>/
fn data_dir(source: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in source.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    PathBuf::from(home)
        .join(".hearthfs")
        .join(format!("{hash:016x}"))
}

/// Model directory. Checks: ./models, ~/.hearthfs/models
fn model_dir() -> PathBuf {
    // Check local models/ dir first (for development)
    let local = PathBuf::from("models");
    if local.join("all-MiniLM-L6-v2.onnx").exists() {
        return local;
    }
    // Fall back to ~/.hearthfs/models/
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".hearthfs").join("models")
}

/// The NFS server binds loopback only, so a mounted source is never
/// reachable from other machines. (Until 0.1.2 it bound 0.0.0.0 and the
/// kernel mounted it through the LAN IP, because an early macOS 26 build
/// refused NFS mounts of 127.0.0.1 with EADDRNOTAVAIL. Loopback mounts work
/// on macOS 26.5.2, verified 2026-10-25.)
const NFS_BIND_IP: std::net::Ipv4Addr = std::net::Ipv4Addr::LOCALHOST;

fn nfs_bind_addr(port: u16) -> String {
    format!("{NFS_BIND_IP}:{port}")
}

/// `mount_nfs` arguments for the server listening at `ip:port`. The mount
/// goes through the same loopback address the server is bound to.
fn build_mount_args(ip: std::net::IpAddr, port: u16, mountpoint: &str) -> Vec<String> {
    // soft + deadtimeout: ops ERROR instead of hanging in uninterruptible
    // kernel wait when the server stops answering. Hard mounts wedged the
    // whole machine's view of the mountpoint in the 2026-07-17 incident
    // (unkillable ls, frozen hearthloom daemon). The server is local and
    // answers in ms — a 15s dead-server verdict is generous.
    vec![
        "-o".to_string(),
        format!(
            "soft,deadtimeout=15,nolocks,noresvport,vers=3,tcp,rsize=131072,actimeo=120,port={port},mountport={port}"
        ),
        format!("{ip}:/"),
        mountpoint.to_string(),
    ]
}

/// Whether `path` currently has something mounted on it.
fn is_mounted_at(path: &str) -> bool {
    let canon = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string());
    std::process::Command::new("/sbin/mount")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.contains(&format!(" on {canon} ")))
        })
        .unwrap_or(false)
}

/// Force-unmount whatever is at `path` (best effort, escalating).
fn force_unmount(path: &str) {
    for args in [
        vec![path.to_string()],
        vec!["-f".to_string(), path.to_string()],
    ] {
        let _ = std::process::Command::new("/sbin/umount").args(&args).output();
        if !is_mounted_at(path) {
            return;
        }
    }
    let _ = std::process::Command::new("/usr/sbin/diskutil")
        .args(["unmount", "force", path])
        .output();
}

/// A previous unclean death (SIGKILL, crash, power loss) leaves the kernel
/// NFS mount in place with no server behind it; mounting over it stacks a
/// new mount on a wedged one and every path lookup hangs (2026-07-17
/// incident). Clear it or refuse to start.
fn ensure_mountpoint_clear(path: &str) -> Result<()> {
    if !is_mounted_at(path) {
        return Ok(());
    }
    tracing::warn!(
        "stale mount detected at {path} (previous instance died uncleanly) — force-unmounting"
    );
    force_unmount(path);
    if is_mounted_at(path) {
        anyhow::bail!(
            "cannot clear stale mount at {path}; refusing to mount over it. \
             Try: sudo umount -f {path}"
        );
    }
    tracing::info!("stale mount cleared");
    Ok(())
}

/// Verify the mount actually answers: list the mountpoint from a throwaway
/// thread with a deadline. The thread may be left blocked on failure — we
/// exit immediately after, so it never outlives the verdict.
fn probe_mount(path: &str, deadline: std::time::Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.to_string();
    std::thread::spawn(move || {
        let ok = std::fs::read_dir(&p).is_ok();
        let _ = tx.send(ok);
    });
    rx.recv_timeout(deadline).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfs_bind_address_is_loopback() {
        let addr: std::net::SocketAddr = nfs_bind_addr(11111).parse().unwrap();
        assert!(addr.ip().is_loopback(), "{addr} must be a loopback address");
        assert_eq!(addr.port(), 11111);
    }

    #[test]
    fn mount_args_use_the_bound_address() {
        let args = build_mount_args(std::net::IpAddr::V4(NFS_BIND_IP), 11111, "/tmp/mnt");
        assert_eq!(args[2], "127.0.0.1:/");
        assert_eq!(args[3], "/tmp/mnt");
        assert!(args[1].contains("port=11111,mountport=11111"), "{}", args[1]);
        // The wedge-incident invariant: never a hard mount.
        assert!(args[1].starts_with("soft,deadtimeout=15,"), "{}", args[1]);
    }

    /// The listener the mount command creates answers on loopback and
    /// refuses connections addressed to this machine's network address.
    #[tokio::test]
    async fn nfs_listener_is_unreachable_from_the_network() {
        let dir = std::env::temp_dir().join(format!("hearthfs-bind-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = Arc::new(hearthfs_source_fs::FsSource::new(dir.clone()).unwrap());
        let vfs = Arc::new(hearthfs_core::passthrough::PassthroughVfs::new(source));
        let listener = NFSTcpListener::bind_ro(&nfs_bind_addr(0), hearthfs_nfs::HearthNfs::new(vfs))
            .await
            .unwrap();
        let port = listener.get_listen_port();
        assert!(listener.get_listen_ip().is_loopback());

        let connect = |ip: std::net::IpAddr| async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                tokio::net::TcpStream::connect((ip, port)),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
        };
        assert!(connect(std::net::IpAddr::V4(NFS_BIND_IP)).await, "loopback must connect");

        // This machine's outbound address, if it has one. A UDP "connect"
        // only selects a route; it sends nothing.
        let lan_ip = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
            .and_then(|s| s.local_addr())
            .map(|a| a.ip())
            .ok()
            .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());
        if let Some(ip) = lan_ip {
            assert!(!connect(ip).await, "{ip}:{port} must refuse connections");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
