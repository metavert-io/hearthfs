use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nfs3_server::vfs::{
    DirEntryPlus, FileHandleU64, NextResult, NfsReadFileSystem, ReadDirPlusIterator,
};
use nfs3_types::nfs3::{fattr3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, specdata3};

use hearthfs_core::passthrough::{EntryMeta, PassthroughVfs};

/// Adapter that implements NfsReadFileSystem by delegating to a PassthroughVfs.
pub struct HearthNfs {
    vfs: Arc<PassthroughVfs>,
}

impl HearthNfs {
    pub fn new(vfs: Arc<PassthroughVfs>) -> Self {
        Self { vfs }
    }
}

impl NfsReadFileSystem for HearthNfs {
    type Handle = FileHandleU64;

    fn root_dir(&self) -> Self::Handle {
        FileHandleU64::new(1)
    }

    async fn lookup(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
    ) -> Result<Self::Handle, nfsstat3> {
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let meta = self
            .vfs
            .lookup(dirid.as_u64(), name)
            .await
            .map_err(|e| {
                tracing::debug!("lookup({}, {name}) failed: {e}", dirid.as_u64());
                nfsstat3::NFS3ERR_NOENT
            })?;
        Ok(FileHandleU64::new(meta.inode))
    }

    async fn getattr(&self, id: &Self::Handle) -> Result<fattr3, nfsstat3> {
        let meta = self
            .vfs
            .getattr(id.as_u64())
            .await
            .map_err(|_| nfsstat3::NFS3ERR_STALE)?;
        Ok(meta_to_fattr3(&meta))
    }

    async fn read(
        &self,
        id: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.vfs
            .read(id.as_u64(), offset, count)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)
    }

    async fn readdirplus(
        &self,
        dirid: &Self::Handle,
        cookie: u64,
    ) -> Result<impl ReadDirPlusIterator<Self::Handle>, nfsstat3> {
        let entries = self
            .vfs
            .readdir(dirid.as_u64())
            .await
            .map_err(|e| {
                tracing::debug!("readdir({}) failed: {e}", dirid.as_u64());
                nfsstat3::NFS3ERR_IO
            })?;

        // Fetch attrs for each entry so readdirplus can include them
        let mut plus_entries = Vec::with_capacity(entries.len());
        for (idx, (name, inode, _is_dir)) in entries.into_iter().enumerate() {
            let cookie_val = (idx + 1) as u64;
            let attr = self.vfs.getattr(inode).await.ok().map(|m| meta_to_fattr3(&m));
            plus_entries.push(DirEntryPlus {
                fileid: inode,
                name: filename3::from(name.into_bytes()),
                cookie: cookie_val,
                name_attributes: attr,
                name_handle: Some(FileHandleU64::new(inode)),
            });
        }

        Ok(VecDirIter {
            entries: plus_entries,
            pos: cookie as usize,
        })
    }

    async fn readlink(&self, _id: &Self::Handle) -> Result<nfspath3<'_>, nfsstat3> {
        // Symlinks not supported in v0
        Err(nfsstat3::NFS3ERR_INVAL)
    }
}

/// Simple iterator over a pre-built Vec of directory entries.
struct VecDirIter {
    entries: Vec<DirEntryPlus<FileHandleU64>>,
    pos: usize,
}

impl ReadDirPlusIterator<FileHandleU64> for VecDirIter {
    async fn next(&mut self) -> NextResult<DirEntryPlus<FileHandleU64>> {
        if self.pos >= self.entries.len() {
            return NextResult::Eof;
        }
        let entry = self.entries[self.pos].clone();
        self.pos += 1;
        NextResult::Ok(entry)
    }
}

fn meta_to_fattr3(meta: &EntryMeta) -> fattr3 {
    let ftype = if meta.is_dir {
        ftype3::NF3DIR
    } else {
        ftype3::NF3REG
    };
    let mode = if meta.is_dir { 0o755 } else { 0o644 };

    fattr3 {
        type_: ftype,
        mode,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: meta.size,
        used: meta.size,
        rdev: specdata3 {
            specdata1: 0,
            specdata2: 0,
        },
        fsid: 0,
        fileid: meta.inode,
        atime: system_time_to_nfs(meta.modified),
        mtime: system_time_to_nfs(meta.modified),
        ctime: system_time_to_nfs(meta.created),
    }
}

fn system_time_to_nfs(t: SystemTime) -> nfstime3 {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    nfstime3 {
        seconds: dur.as_secs() as u32,
        nseconds: dur.subsec_nanos(),
    }
}
