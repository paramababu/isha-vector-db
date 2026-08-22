import CVdb
import Foundation

/// An open database.
///
/// Closed by `deinit` as well as by ``close()``, so a database released without an explicit
/// close still gives up its lock. That is a backstop rather than the intended path: `deinit`
/// runs when ARC decides it does, and on iOS a lock held past its usefulness is the difference
/// between an application reopening its own data and reporting it as unavailable.
public final class Database: @unchecked Sendable {
    // `@unchecked Sendable` because the engine is genuinely thread-safe — one writer, many
    // readers, enforced internally — but the raw pointer cannot express that to the compiler.
    private var handle: OpaquePointer?

    /// Open or create a database at a directory path.
    ///
    /// A read-only database takes no lock, so it can inspect one another process has open.
    /// `createIfMissing` is ignored when `readOnly` is set, since creating requires writing.
    ///
    /// On iOS the sensible location is Application Support, excluded from backup. A database
    /// restored onto a different device is not corrupt, but it is another device's data, and
    /// the lock file and identity travel with it.
    public init(
        path: String,
        createIfMissing: Bool = true,
        readOnly: Bool = false,
        durability: Durability = .batch
    ) throws {
        var opened: OpaquePointer?
        var bytes = Array(path.utf8)
        try check { error in
            bytes.withUnsafeMutableBufferPointer { buffer in
                vdb_open(
                    buffer.baseAddress, buffer.count,
                    createIfMissing && !readOnly, readOnly, durability.raw,
                    &opened, error
                )
            }
        }
        handle = opened
    }

    deinit {
        if let handle {
            var error: OpaquePointer?
            // Nothing useful can be done with a failure here, and throwing from `deinit` is not
            // possible. The lock is released either way, which is the part that matters.
            _ = vdb_close(handle, &error)
            if let error { vdb_error_free(error) }
        }
    }

    /// Whether the handle is still usable.
    public var isOpen: Bool { handle != nil }

    /// Flush and close, releasing the lock. Idempotent.
    public func close() throws {
        guard let live = handle else { return }
        handle = nil
        try check { vdb_close(live, $0) }
    }

    /// Fold every collection's buffered writes into segments.
    public func flush() throws {
        let live = try alive()
        try check { vdb_flush(live, $0) }
    }

    /// Create a collection, or open it if one exists with a matching shape.
    public func collection(
        _ name: String,
        dimension: UInt32,
        metric: Metric = .cosine
    ) throws -> Collection {
        let live = try alive()
        var created: OpaquePointer?
        var bytes = Array(name.utf8)
        try check { error in
            bytes.withUnsafeMutableBufferPointer { buffer in
                vdb_collection_create(
                    live, buffer.baseAddress, buffer.count,
                    dimension, metric.raw, false, &created, error
                )
            }
        }
        return Collection(handle: created, name: name)
    }

    /// Open an existing collection.
    public func openCollection(_ name: String) throws -> Collection {
        let live = try alive()
        var opened: OpaquePointer?
        var bytes = Array(name.utf8)
        try check { error in
            bytes.withUnsafeMutableBufferPointer { buffer in
                vdb_collection_open(live, buffer.baseAddress, buffer.count, &opened, error)
            }
        }
        return Collection(handle: opened, name: name)
    }

    /// Delete a collection and everything in it. Irreversible.
    public func dropCollection(_ name: String) throws {
        let live = try alive()
        var bytes = Array(name.utf8)
        try check { error in
            bytes.withUnsafeMutableBufferPointer { buffer in
                vdb_collection_drop(live, buffer.baseAddress, buffer.count, error)
            }
        }
    }

    private func alive() throws -> OpaquePointer {
        guard let handle else {
            throw VdbError(code: 2003, message: "[VDB-2003] the database is closed")
        }
        return handle
    }
}
