//! C ABI over the HearthFS VFS for the FSKit app extension (Swift host).
//!
//! Spike-grade bridging: structured results cross the boundary as JSON
//! strings (freed with `hfs_string_free`); file bytes cross as raw buffers.
//! All calls are synchronous — each handle owns a tokio runtime and blocks
//! on the async VFS. Return convention: 0 = success, negative = errno-style
//! failure (-2 = not found / ENOENT, -5 = EIO).

use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;
use std::sync::Arc;

use hearthfs_core::passthrough::PassthroughVfs;
use hearthfs_source_fs::FsSource;

pub struct HfsHandle {
    runtime: tokio::runtime::Runtime,
    vfs: Arc<PassthroughVfs>,
}

/// Root inode of every HearthFS tree.
#[no_mangle]
pub extern "C" fn hfs_root_inode() -> u64 {
    1
}

/// Open a HearthFS view over `source_dir`, with indexes under `db_dir`.
/// Returns null on failure. Indexing starts in the background; search
/// improves as it completes.
///
/// # Safety
/// `source_dir` and `db_dir` must each be null or a valid NUL-terminated
/// string.
#[no_mangle]
pub unsafe extern "C" fn hfs_open(source_dir: *const c_char, db_dir: *const c_char) -> *mut HfsHandle {
    let (Some(source_dir), Some(db_dir)) = (cstr_to_path(source_dir), cstr_to_path(db_dir))
    else {
        return std::ptr::null_mut();
    };

    let build = || -> Result<HfsHandle, Box<dyn std::error::Error>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;

        std::fs::create_dir_all(&db_dir)?;
        let source = Arc::new(FsSource::new(source_dir)?);
        let fts = Arc::new(hearthfs_core::fts::FtsIndex::open(&db_dir.join("fts.db"))?);
        let mut vfs = PassthroughVfs::new(source.clone()).with_fts(fts.clone());

        // Embeddings are optional; local default only (FSKit host sets
        // HEARTHFS_EMBEDDER for other backends).
        let embedder = hearthfs_core::embeddings::Embedder::from_env()
            .ok()
            .flatten();
        let embedder_and_vectors = embedder.and_then(|e| {
            let vectors =
                hearthfs_core::vector_store::VectorStore::open(&db_dir.join("vectors.db"), e.dim())
                    .ok()?;
            vectors.set_model(e.id()).ok()?;
            Some((Arc::new(e), Arc::new(vectors)))
        });
        if let Some((ref e, ref v)) = embedder_and_vectors {
            vfs = vfs.with_embeddings(e.clone(), v.clone());
        }

        let mut indexer = hearthfs_core::indexer::Indexer::new(source, fts);
        if let Some((e, v)) = embedder_and_vectors {
            indexer = indexer.with_embeddings(e, v);
        }
        runtime.spawn(async move {
            match indexer.full_scan().await {
                Ok(n) => tracing::info!("initial indexing complete: {n} files"),
                Err(e) => tracing::error!("indexing failed: {e}"),
            }
        });

        Ok(HfsHandle {
            runtime,
            vfs: Arc::new(vfs),
        })
    };

    match build() {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Release a handle from `hfs_open`.
///
/// # Safety
/// `handle` must be a pointer returned by `hfs_open`, not yet freed.
#[no_mangle]
pub unsafe extern "C" fn hfs_close(handle: *mut HfsHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// Attributes of an inode as JSON: {"inode","size","is_dir","modified_secs"}.
///
/// # Safety
/// `handle` must be null or a live pointer from `hfs_open`; `out_json` must
/// be null or valid for writing one pointer.
#[no_mangle]
pub unsafe extern "C" fn hfs_getattr(
    handle: *mut HfsHandle,
    inode: u64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return -5;
    };
    match h.runtime.block_on(h.vfs.getattr(inode)) {
        Ok(meta) => write_json(out_json, &meta_json(inode, &meta)),
        Err(_) => -2,
    }
}

/// Look up `name` in directory `parent`. JSON as in `hfs_getattr`.
///
/// # Safety
/// As for `hfs_getattr`; `name` must be null or a valid NUL-terminated
/// string.
#[no_mangle]
pub unsafe extern "C" fn hfs_lookup(
    handle: *mut HfsHandle,
    parent: u64,
    name: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return -5;
    };
    let Some(name) = cstr_to_str(name) else {
        return -5;
    };
    match h.runtime.block_on(h.vfs.lookup(parent, name)) {
        Ok(meta) => write_json(out_json, &meta_json(meta.inode, &meta)),
        Err(_) => -2,
    }
}

/// Directory listing as JSON: [{"name","inode","is_dir"}].
///
/// # Safety
/// As for `hfs_getattr`.
#[no_mangle]
pub unsafe extern "C" fn hfs_readdir(
    handle: *mut HfsHandle,
    inode: u64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return -5;
    };
    match h.runtime.block_on(h.vfs.readdir(inode)) {
        Ok(entries) => {
            let list: Vec<serde_json::Value> = entries
                .into_iter()
                .map(|(name, inode, is_dir)| {
                    serde_json::json!({"name": name, "inode": inode, "is_dir": is_dir})
                })
                .collect();
            write_json(out_json, &serde_json::Value::Array(list))
        }
        Err(_) => -2,
    }
}

/// Read up to `len` bytes at `offset` into `buf`. Bytes written go to
/// `out_len`; `out_eof` is 1 at end of file.
///
/// # Safety
/// `handle` must be null or a live pointer from `hfs_open`; `buf` must be
/// null or valid for writing `len` bytes; `out_len` and `out_eof` must be
/// null or valid for writing.
#[no_mangle]
pub unsafe extern "C" fn hfs_read(
    handle: *mut HfsHandle,
    inode: u64,
    offset: u64,
    len: u32,
    buf: *mut u8,
    out_len: *mut u32,
    out_eof: *mut u8,
) -> i32 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return -5;
    };
    if buf.is_null() || out_len.is_null() || out_eof.is_null() {
        return -5;
    }
    match h.runtime.block_on(h.vfs.read(inode, offset, len)) {
        Ok((data, eof)) => {
            let n = data.len().min(len as usize);
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), buf, n);
                *out_len = n as u32;
                *out_eof = eof as u8;
            }
            0
        }
        Err(_) => -2,
    }
}

/// Free a string returned via any `out_json` parameter.
///
/// # Safety
/// `s` must have been produced by this library and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn hfs_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

fn meta_json(inode: u64, meta: &hearthfs_core::passthrough::EntryMeta) -> serde_json::Value {
    let modified_secs = meta
        .modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    serde_json::json!({
        "inode": inode,
        "size": meta.size,
        "is_dir": meta.is_dir,
        "modified_secs": modified_secs,
    })
}

fn write_json(out: *mut *mut c_char, value: &serde_json::Value) -> i32 {
    if out.is_null() {
        return -5;
    }
    match CString::new(value.to_string()) {
        Ok(s) => {
            unsafe { *out = s.into_raw() };
            0
        }
        Err(_) => -5,
    }
}

fn cstr_to_str<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(s) }.to_str().ok()
}

fn cstr_to_path(s: *const c_char) -> Option<PathBuf> {
    cstr_to_str(s).map(PathBuf::from)
}
