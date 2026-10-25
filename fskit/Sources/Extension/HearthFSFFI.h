// C ABI of libhearthfs_ffi.a (crates/hearthfs-ffi/src/lib.rs).
// Bridged into the Swift extension via SWIFT_OBJC_BRIDGING_HEADER.
//
// Return convention: 0 = success, -2 = not found (ENOENT), -5 = EIO.
// Strings handed back through out_json are freed with hfs_string_free.

#ifndef HEARTHFS_FFI_H
#define HEARTHFS_FFI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct HfsHandle HfsHandle;

/// Root inode of every HearthFS tree (always 1).
uint64_t hfs_root_inode(void);

/// Open a HearthFS view over `source_dir`, indexes under `db_dir`.
/// Returns NULL on failure. Indexing starts in the background.
HfsHandle *hfs_open(const char *source_dir, const char *db_dir);

/// Release a handle from hfs_open.
void hfs_close(HfsHandle *handle);

/// Attributes of an inode as JSON: {"inode","size","is_dir","modified_secs"}.
int32_t hfs_getattr(HfsHandle *handle, uint64_t inode, char **out_json);

/// Look up `name` in directory `parent`. JSON as in hfs_getattr.
int32_t hfs_lookup(HfsHandle *handle, uint64_t parent, const char *name,
                   char **out_json);

/// Directory listing as JSON: [{"name","inode","is_dir"}].
int32_t hfs_readdir(HfsHandle *handle, uint64_t inode, char **out_json);

/// Read up to `len` bytes at `offset` into `buf`; bytes written to *out_len,
/// *out_eof set to 1 at end of file.
int32_t hfs_read(HfsHandle *handle, uint64_t inode, uint64_t offset,
                 uint32_t len, uint8_t *buf, uint32_t *out_len,
                 uint8_t *out_eof);

/// Free a string returned via any out_json parameter.
void hfs_string_free(char *s);

#ifdef __cplusplus
}
#endif

#endif /* HEARTHFS_FFI_H */
