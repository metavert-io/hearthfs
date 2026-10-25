use std::ffi::{OsStr, OsString};
use std::time::SystemTime;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;

/// Opaque inode identifier.
pub type InodeId = u64;

/// Root inode, by convention.
pub const ROOT_INODE: InodeId = 1;

/// File type for a VFS entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    RegularFile,
    Directory,
    Symlink,
}

/// Attributes for a VFS entry (analogous to stat).
#[derive(Debug, Clone)]
pub struct VfsAttr {
    pub inode: InodeId,
    pub size: u64,
    pub file_type: FileType,
    pub mode: u32,
    pub modified: SystemTime,
    pub created: SystemTime,
}

/// A VFS entry returned by lookup.
#[derive(Debug, Clone)]
pub struct VfsEntry {
    pub inode: InodeId,
    pub file_type: FileType,
    pub name: OsString,
}

/// A directory entry for readdir.
#[derive(Debug, Clone)]
pub struct VfsDirEntry {
    pub inode: InodeId,
    pub file_type: FileType,
    pub name: OsString,
    pub offset: u64,
}

/// The core VFS interface. Both NFS and FUSE adapters target this trait.
/// Read-only for v0.
#[async_trait]
pub trait VfsHandler: Send + Sync {
    async fn lookup(&self, parent: InodeId, name: &OsStr) -> Result<VfsEntry>;
    async fn getattr(&self, inode: InodeId) -> Result<VfsAttr>;
    async fn read(&self, inode: InodeId, offset: u64, size: u32) -> Result<Bytes>;
    async fn readdir(&self, inode: InodeId, offset: u64) -> Result<Vec<VfsDirEntry>>;
    async fn readlink(&self, inode: InodeId) -> Result<OsString>;
    async fn getxattr(&self, inode: InodeId, name: &OsStr) -> Result<Vec<u8>>;
    async fn listxattr(&self, inode: InodeId) -> Result<Vec<OsString>>;
}
