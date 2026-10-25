// HearthFS FSKit module: a read-only FSUnaryFileSystem over libhearthfs_ffi.a.
//
// mount -F -t hearthfs <source-dir> <mountpoint>
// The FSPathURLResource URL is the source directory; the volume opens the
// Rust VFS with hfs_open(source_dir, ~/.hearthfs/fskit-<fnv64-hash>) and
// serves the tree (including magic paths like @search/@fit, which are plain
// lookups/reads to the VFS).

import Foundation
import ExtensionFoundation
import FSKit
import os

// The subsystem is this extension's bundle id (`<prefix>.fskit`, set per
// developer in fskit/Local.xcconfig), so stream logs with
// `/usr/bin/log stream --predicate 'subsystem == "<prefix>.fskit"'`.
let hfsLog = Logger(subsystem: Bundle.main.bundleIdentifier ?? "hearthfs.fskit", category: "module")

@main
struct HearthFSExtensionMain: UnaryFileSystemExtension {
    var fileSystem: HearthFS { HearthFS() }
}

// MARK: - Errors

func hfsError(_ posixCode: Int32) -> NSError {
    // fs_errorForPOSIXError is declared NSError* in FSKitFunctions.h but is
    // imported into Swift as `any Error`.
    fs_errorForPOSIXError(posixCode) as NSError
}

/// Map hearthfs-ffi return codes (0 ok, -2 ENOENT, -5 EIO) to NSErrors.
func hfsError(fromRC rc: Int32) -> NSError {
    switch rc {
    case -2: return hfsError(ENOENT)
    default: return hfsError(EIO)
    }
}

// MARK: - Unary file system

final class HearthFS: FSUnaryFileSystem, FSUnaryFileSystemOperations {

    static func sourceURL(of resource: FSResource) -> URL? {
        if let path = resource as? FSPathURLResource {
            return path.url
        }
        if let generic = resource as? FSGenericURLResource {
            return generic.url
        }
        return nil
    }

    static func isUsableDirectory(_ url: URL) -> Bool {
        var isDir: ObjCBool = false
        let exists = FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir)
        return exists && isDir.boolValue
    }

    func probeResource(
        resource: FSResource,
        replyHandler reply: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        guard let url = Self.sourceURL(of: resource) else {
            hfsLog.info("probe: unsupported resource type \(String(describing: type(of: resource)))")
            reply(FSProbeResult.notRecognized, nil)
            return
        }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }

        guard Self.isUsableDirectory(url) else {
            hfsLog.info("probe: \(url.path) is not a directory")
            reply(FSProbeResult.notRecognized, nil)
            return
        }
        hfsLog.info("probe: usable directory \(url.path)")
        // A HearthFS "container" has no durable on-disk UUID; a random UUID
        // is explicitly allowed for unary file systems (FSResource.h).
        reply(
            FSProbeResult.usable(
                name: url.lastPathComponent,
                containerID: FSContainerIdentifier(uuid: UUID())
            ),
            nil
        )
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler reply: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        guard let url = Self.sourceURL(of: resource), Self.isUsableDirectory(url) else {
            containerStatus = FSContainerStatus.notReady(status: hfsError(ENOENT))
            reply(nil, hfsError(ENOENT))
            return
        }
        do {
            let volume = try HearthFSVolume(sourceURL: url)
            containerStatus = FSContainerStatus.ready
            hfsLog.info("loadResource: volume created for \(url.path)")
            reply(volume, nil)
        } catch {
            hfsLog.error("loadResource failed for \(url.path): \(error)")
            containerStatus = FSContainerStatus.notReady(status: error as NSError)
            reply(nil, error)
        }
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler reply: @escaping ((any Error)?) -> Void
    ) {
        hfsLog.info("unloadResource")
        // FSContainerStatus has no error-free notReady factory (only
        // notReady(status:) with a non-nil NSError); ENXIO = resource gone.
        containerStatus = FSContainerStatus.notReady(status: hfsError(ENXIO))
        reply(nil)
    }
}

// MARK: - Item

final class HearthFSItem: FSItem {
    let inode: UInt64
    let name: FSFileName
    var isDirectory: Bool
    var size: UInt64
    var modifiedSecs: UInt64

    init(inode: UInt64, name: FSFileName, isDirectory: Bool, size: UInt64, modifiedSecs: UInt64) {
        self.inode = inode
        self.name = name
        self.isDirectory = isDirectory
        self.size = size
        self.modifiedSecs = modifiedSecs
        super.init()
    }

    /// FSItemID 1 (parent-of-root) and 2 (root) are reserved by FSKit while
    /// the HearthFS VFS uses inode 1 for its root. Shift every inode by +1:
    /// injective, and it maps VFS root (1) exactly onto FSItemIDRootDirectory (2).
    var itemID: FSItem.Identifier {
        FSItem.Identifier(rawValue: inode &+ 1) ?? .invalid
    }
}

// MARK: - Volume

final class HearthFSVolume: FSVolume {
    private let handle: OpaquePointer
    private let sourceURL: URL
    private let root: HearthFSItem
    private var itemCache: [UInt64: HearthFSItem] = [:]
    private let cacheLock = NSLock()
    private let dirVerifier = FSDirectoryVerifier(rawValue: 0x4845_4152_5448_4653) // "HEARTHFS"

    struct Meta {
        let inode: UInt64
        let size: UInt64
        let isDir: Bool
        let modifiedSecs: UInt64
    }

    init(sourceURL: URL) throws {
        // The URL may be security-scoped (FSResource.h); keep access for the
        // lifetime of the volume.
        _ = sourceURL.startAccessingSecurityScopedResource()
        self.sourceURL = sourceURL

        let dbDir = Self.databaseDirectory(forSourcePath: sourceURL.path)
        try FileManager.default.createDirectory(
            atPath: dbDir, withIntermediateDirectories: true)

        guard let handle = hfs_open(sourceURL.path, dbDir) else {
            hfsLog.error("hfs_open failed for \(sourceURL.path) (db \(dbDir))")
            throw hfsError(EIO)
        }
        self.handle = handle

        let rootInode = hfs_root_inode()
        self.root = HearthFSItem(
            inode: rootInode,
            name: FSFileName(string: "/"),
            isDirectory: true,
            size: 0,
            modifiedSecs: 0)
        self.itemCache[rootInode] = root

        super.init(
            volumeID: FSVolume.Identifier(uuid: UUID()),
            volumeName: FSFileName(string: sourceURL.lastPathComponent.isEmpty
                ? "HearthFS" : sourceURL.lastPathComponent))
        hfsLog.info("volume open: source=\(sourceURL.path) db=\(dbDir)")
    }

    deinit {
        hfs_close(handle)
        sourceURL.stopAccessingSecurityScopedResource()
    }

    /// ~/.hearthfs/fskit-<fnv1a64 hex of source path>
    static func databaseDirectory(forSourcePath path: String) -> String {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in path.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x0000_0100_0000_01b3
        }
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return "\(home)/.hearthfs/fskit-\(String(hash, radix: 16))"
    }

    // MARK: FFI plumbing

    /// Run an FFI call that returns JSON through a char** out-param.
    private func jsonCall(
        _ body: (UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32
    ) throws -> Any {
        var out: UnsafeMutablePointer<CChar>? = nil
        let rc = withUnsafeMutablePointer(to: &out) { body($0) }
        guard rc == 0, let cstr = out else {
            throw hfsError(fromRC: rc)
        }
        defer { hfs_string_free(cstr) }
        let data = Data(bytes: cstr, count: strlen(cstr))
        return try JSONSerialization.jsonObject(with: data)
    }

    private static func meta(fromJSON json: Any) throws -> Meta {
        guard let dict = json as? [String: Any],
              let inode = (dict["inode"] as? NSNumber)?.uint64Value,
              let isDir = dict["is_dir"] as? Bool
        else { throw hfsError(EIO) }
        return Meta(
            inode: inode,
            size: (dict["size"] as? NSNumber)?.uint64Value ?? 0,
            isDir: isDir,
            modifiedSecs: (dict["modified_secs"] as? NSNumber)?.uint64Value ?? 0)
    }

    func getMeta(inode: UInt64) throws -> Meta {
        try Self.meta(fromJSON: jsonCall { hfs_getattr(handle, inode, $0) })
    }

    func lookupMeta(parent: UInt64, name: String) throws -> Meta {
        try Self.meta(fromJSON: jsonCall { out in
            name.withCString { hfs_lookup(handle, parent, $0, out) }
        })
    }

    func readdir(inode: UInt64) throws -> [(name: String, inode: UInt64, isDir: Bool)] {
        let json = try jsonCall { hfs_readdir(handle, inode, $0) }
        guard let list = json as? [[String: Any]] else { throw hfsError(EIO) }
        return list.compactMap { entry in
            guard let name = entry["name"] as? String,
                  let ino = (entry["inode"] as? NSNumber)?.uint64Value,
                  let isDir = entry["is_dir"] as? Bool
            else { return nil }
            return (name, ino, isDir)
        }
    }

    // MARK: Item cache

    /// Return the live FSItem for an inode, updating cached attrs, or make one.
    func item(for meta: Meta, name: FSFileName) -> HearthFSItem {
        cacheLock.lock()
        defer { cacheLock.unlock() }
        if let existing = itemCache[meta.inode] {
            existing.size = meta.size
            existing.isDirectory = meta.isDir
            existing.modifiedSecs = meta.modifiedSecs
            return existing
        }
        let item = HearthFSItem(
            inode: meta.inode,
            name: name,
            isDirectory: meta.isDir,
            size: meta.size,
            modifiedSecs: meta.modifiedSecs)
        itemCache[meta.inode] = item
        return item
    }

    func evict(_ item: HearthFSItem) {
        cacheLock.lock()
        defer { cacheLock.unlock() }
        if item !== root {
            itemCache.removeValue(forKey: item.inode)
        }
    }

    // MARK: Attributes

    func attributes(for item: HearthFSItem, meta: Meta?) -> FSItem.Attributes {
        let attrs = FSItem.Attributes()
        attrs.invalidateAllProperties()
        let isDir = meta?.isDir ?? item.isDirectory
        let size = meta?.size ?? item.size
        let mtime = meta?.modifiedSecs ?? item.modifiedSecs
        attrs.type = isDir ? .directory : .file
        attrs.mode = isDir ? 0o555 : 0o444 // read-only filesystem
        attrs.linkCount = 1
        attrs.uid = getuid()
        attrs.gid = getgid()
        attrs.size = isDir ? 4096 : size
        attrs.allocSize = max(4096, (size + 4095) / 4096 * 4096)
        attrs.fileID = item.itemID
        let ts = timespec(tv_sec: time_t(clamping: mtime), tv_nsec: 0)
        attrs.modifyTime = ts
        attrs.changeTime = ts
        attrs.accessTime = ts
        attrs.birthTime = ts
        return attrs
    }
}

// MARK: - PathConf

extension HearthFSVolume: FSVolume.PathConfOperations {
    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { 0 }
    var maximumFileSize: UInt64 { UInt64.max }
}

// MARK: - Core operations

extension HearthFSVolume: FSVolume.Operations {

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsPersistentObjectIDs = true
        caps.supports64BitObjectIDs = true
        caps.supportsSymbolicLinks = false
        caps.supportsHardLinks = false
        caps.supportsHiddenFiles = false
        caps.supports2TBFiles = true
        caps.doesNotSupportVolumeSizes = true
        caps.doesNotSupportImmutableFiles = true
        caps.doesNotSupportSettingFilePermissions = true
        caps.doesNotSupportRootTimes = true
        caps.caseFormat = .sensitive
        return caps
    }

    var volumeStatistics: FSStatFSResult {
        let stats = FSStatFSResult(fileSystemTypeName: "hearthfs")
        stats.blockSize = 4096
        stats.ioSize = 1 << 20
        stats.totalBytes = 1 << 38
        stats.usedBytes = 1 << 38
        stats.freeBytes = 0
        stats.availableBytes = 0
        stats.totalFiles = 0
        stats.freeFiles = 0
        return stats
    }

    func activate(options: FSTaskOptions) async throws -> FSItem {
        hfsLog.info("activate")
        return root
    }

    func deactivate(options: FSDeactivateOptions) async throws {
        hfsLog.info("deactivate")
    }

    func mount(options: FSTaskOptions) async throws {
        hfsLog.info("mount")
    }

    func unmount() async {
        hfsLog.info("unmount")
    }

    func synchronize(flags: FSSyncFlags) async throws {
        // Read-only: nothing to flush.
    }

    func attributes(
        _ desiredAttributes: FSItem.GetAttributesRequest,
        of item: FSItem
    ) async throws -> FSItem.Attributes {
        guard let item = item as? HearthFSItem else { throw hfsError(EIO) }
        let meta = try getMeta(inode: item.inode)
        return attributes(for: item, meta: meta)
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest,
        on item: FSItem
    ) async throws -> FSItem.Attributes {
        throw hfsError(EROFS)
    }

    func lookupItem(
        named name: FSFileName,
        inDirectory directory: FSItem
    ) async throws -> (FSItem, FSFileName) {
        guard let dir = directory as? HearthFSItem, let nameStr = name.string else {
            throw hfsError(ENOENT)
        }
        if nameStr == "." {
            return (dir, name)
        }
        let meta = try lookupMeta(parent: dir.inode, name: nameStr)
        let item = item(for: meta, name: name)
        return (item, name)
    }

    func reclaimItem(_ item: FSItem) async throws {
        if let item = item as? HearthFSItem {
            evict(item)
        }
    }

    func readSymbolicLink(_ item: FSItem) async throws -> FSFileName {
        throw hfsError(ENOTSUP)
    }

    func createItem(
        named name: FSFileName,
        type: FSItem.ItemType,
        inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest
    ) async throws -> (FSItem, FSFileName) {
        throw hfsError(EROFS)
    }

    func createSymbolicLink(
        named name: FSFileName,
        inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName
    ) async throws -> (FSItem, FSFileName) {
        throw hfsError(EROFS)
    }

    func createLink(
        to item: FSItem,
        named name: FSFileName,
        inDirectory directory: FSItem
    ) async throws -> FSFileName {
        throw hfsError(EROFS)
    }

    func removeItem(
        _ item: FSItem,
        named name: FSFileName,
        fromDirectory directory: FSItem
    ) async throws {
        throw hfsError(EROFS)
    }

    func renameItem(
        _ item: FSItem,
        inDirectory sourceDirectory: FSItem,
        named sourceName: FSFileName,
        to destinationName: FSFileName,
        inDirectory destinationDirectory: FSItem,
        overItem: FSItem?
    ) async throws -> FSFileName {
        throw hfsError(EROFS)
    }

    func enumerateDirectory(
        _ directory: FSItem,
        startingAt cookie: FSDirectoryCookie,
        verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?,
        packer: FSDirectoryEntryPacker
    ) async throws -> FSDirectoryVerifier {
        guard let dir = directory as? HearthFSItem else { throw hfsError(EIO) }

        let entries = try readdir(inode: dir.inode)

        // Absolute-index cookies. Positions 0 and 1 are "." and ".." (packed
        // only when no attributes were requested, per FSVolume.h).
        var index: UInt64 = 0
        let start = cookie.rawValue

        func pack(
            name: FSFileName,
            type: FSItem.ItemType,
            id: FSItem.Identifier,
            attrs: FSItem.Attributes?
        ) -> Bool {
            defer { index += 1 }
            if index < start { return true }
            return packer.packEntry(
                name: name,
                itemType: type,
                itemID: id,
                nextCookie: FSDirectoryCookie(rawValue: index + 1),
                attributes: attrs)
        }

        if attributes == nil {
            let parentID: FSItem.Identifier =
                dir.inode == hfs_root_inode() ? .parentOfRoot : dir.itemID
            if !pack(name: FSFileName(string: "."), type: .directory,
                     id: dir.itemID, attrs: nil) {
                return dirVerifier
            }
            if !pack(name: FSFileName(string: ".."), type: .directory,
                     id: parentID, attrs: nil) {
                return dirVerifier
            }
        } else {
            index = 2 // keep cookie space stable across attr/no-attr calls
        }

        for entry in entries {
            let name = FSFileName(string: entry.name)
            var attrs: FSItem.Attributes? = nil
            if attributes != nil {
                let meta = (try? getMeta(inode: entry.inode))
                    ?? Meta(inode: entry.inode, size: 0, isDir: entry.isDir, modifiedSecs: 0)
                attrs = self.attributes(for: item(for: meta, name: name), meta: meta)
            }
            let id = FSItem.Identifier(rawValue: entry.inode &+ 1) ?? .invalid
            if !pack(name: name, type: entry.isDir ? .directory : .file,
                     id: id, attrs: attrs) {
                return dirVerifier
            }
        }
        return dirVerifier
    }
}

// MARK: - Open/close (no-ops; the FFI is stateless per read)

extension HearthFSVolume: FSVolume.OpenCloseOperations {
    func openItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws {
        if modes.contains(.write) {
            throw hfsError(EROFS)
        }
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws {}
}

// MARK: - Read/write

extension HearthFSVolume: FSVolume.ReadWriteOperations {
    func read(
        from item: FSItem,
        at offset: off_t,
        length: Int,
        into buffer: FSMutableFileDataBuffer
    ) async throws -> Int {
        guard let item = item as? HearthFSItem else { throw hfsError(EIO) }
        guard offset >= 0 else { throw hfsError(EINVAL) }

        return try buffer.withUnsafeMutableBytes { raw -> Int in
            guard let base = raw.baseAddress else { return 0 }
            let want = min(length, raw.count)
            var total = 0
            while total < want {
                var got: UInt32 = 0
                var eof: UInt8 = 0
                let chunk = UInt32(clamping: want - total)
                let rc = hfs_read(
                    handle,
                    item.inode,
                    UInt64(offset) + UInt64(total),
                    chunk,
                    (base + total).assumingMemoryBound(to: UInt8.self),
                    &got,
                    &eof)
                guard rc == 0 else { throw hfsError(fromRC: rc) }
                total += Int(got)
                if eof != 0 || got == 0 { break }
            }
            return total
        }
    }

    func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> Int {
        throw hfsError(EROFS)
    }
}
