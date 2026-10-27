use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Bidirectional map between inode IDs and relative paths.
///
/// Thread-safe. Inodes are allocated incrementally. Inode 1 is always the root.
pub struct InodeTable {
    inner: RwLock<Inner>,
}

struct Inner {
    next_id: u64,
    path_to_inode: HashMap<PathBuf, u64>,
    inode_to_path: HashMap<u64, PathBuf>,
}

impl InodeTable {
    pub fn new() -> Self {
        let mut path_to_inode = HashMap::new();
        let mut inode_to_path = HashMap::new();
        let root = PathBuf::from("");
        path_to_inode.insert(root.clone(), 1);
        inode_to_path.insert(1, root);

        Self {
            inner: RwLock::new(Inner {
                next_id: 2,
                path_to_inode,
                inode_to_path,
            }),
        }
    }

    /// Get or allocate an inode for the given relative path.
    pub fn get_or_insert(&self, path: &Path) -> u64 {
        // Fast path: read lock
        {
            let inner = self.inner.read().unwrap();
            if let Some(&id) = inner.path_to_inode.get(path) {
                return id;
            }
        }
        // Slow path: write lock
        let mut inner = self.inner.write().unwrap();
        if let Some(&id) = inner.path_to_inode.get(path) {
            return id;
        }
        let id = inner.next_id;
        inner.next_id += 1;
        inner.path_to_inode.insert(path.to_path_buf(), id);
        inner.inode_to_path.insert(id, path.to_path_buf());
        id
    }

    /// Look up the path for an inode.
    pub fn get_path(&self, inode: u64) -> Option<PathBuf> {
        let inner = self.inner.read().unwrap();
        inner.inode_to_path.get(&inode).cloned()
    }

    /// Look up the inode for a path (without allocating).
    pub fn get_inode(&self, path: &Path) -> Option<u64> {
        let inner = self.inner.read().unwrap();
        inner.path_to_inode.get(path).copied()
    }
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_root_is_inode_1() {
        let table = InodeTable::new();
        assert_eq!(table.get_or_insert(Path::new("")), 1);
        assert_eq!(table.get_path(1).unwrap(), PathBuf::from(""));
    }

    #[test]
    fn test_allocation() {
        let table = InodeTable::new();
        let id = table.get_or_insert(Path::new("foo/bar.txt"));
        assert_eq!(id, 2);
        assert_eq!(table.get_or_insert(Path::new("foo/bar.txt")), 2);
        let id2 = table.get_or_insert(Path::new("baz"));
        assert_eq!(id2, 3);
    }
}
