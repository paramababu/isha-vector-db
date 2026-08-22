import CVdb
import Foundation

/// A handle to one collection.
public final class Collection: @unchecked Sendable {
    private var handle: OpaquePointer?
    /// The collection's name.
    public let name: String

    init(handle: OpaquePointer?, name: String) {
        self.handle = handle
        self.name = name
    }

    deinit {
        if let handle { vdb_collection_free(handle) }
    }

    /// Insert or replace a document. Returns true when it was new.
    ///
    /// The array is read in place; nothing is copied until the bytes reach the log.
    @discardableResult
    public func upsert(
        _ id: String, vector: [Float], metadata: [String: Filter.Value] = [:]
    ) throws -> Bool {
        let live = try alive()
        var inserted = false
        var idBytes = Array(id.utf8)

        let handle: OpaquePointer? = metadata.isEmpty ? nil : vdb_metadata_new()
        defer { if let handle { vdb_metadata_free(handle) } }
        if let handle {
            for (key, value) in metadata {
                try Self.set(handle, key, value)
            }
        }

        try check { error in
            idBytes.withUnsafeMutableBufferPointer { idBuffer in
                vector.withUnsafeBufferPointer { vectorBuffer in
                    vdb_upsert(
                        live, idBuffer.baseAddress, idBuffer.count,
                        vectorBuffer.baseAddress, UInt32(vectorBuffer.count),
                        handle, &inserted, error
                    )
                }
            }
        }
        return inserted
    }

    /// Set one metadata field.
    private static func set(_ handle: OpaquePointer, _ key: String, _ value: Filter.Value) throws {
        var keyBytes = Array(key.utf8)
        try check { error in
            keyBytes.withUnsafeMutableBufferPointer { k in
                switch value {
                case let .string(s):
                    var bytes = Array(s.utf8)
                    return bytes.withUnsafeMutableBufferPointer { v in
                        vdb_metadata_set_string(
                            handle, k.baseAddress, k.count, v.baseAddress, v.count, error
                        )
                    }
                case let .int(i):
                    return vdb_metadata_set_i64(handle, k.baseAddress, k.count, i, error)
                case let .double(d):
                    return vdb_metadata_set_f64(handle, k.baseAddress, k.count, d, error)
                case let .bool(b):
                    return vdb_metadata_set_bool(handle, k.baseAddress, k.count, b, error)
                }
            }
        }
    }

    /// Remove a document. Returns whether it existed; removing an absent one is not an error.
    @discardableResult
    public func delete(_ id: String) throws -> Bool {
        let live = try alive()
        var existed = false
        var bytes = Array(id.utf8)
        try check { error in
            bytes.withUnsafeMutableBufferPointer { buffer in
                vdb_delete(live, buffer.baseAddress, buffer.count, &existed, error)
            }
        }
        return existed
    }

    /// Whether a document exists.
    public func contains(_ id: String) throws -> Bool {
        let live = try alive()
        var found = false
        var bytes = Array(id.utf8)
        try check { error in
            bytes.withUnsafeMutableBufferPointer { buffer in
                vdb_contains(live, buffer.baseAddress, buffer.count, &found, error)
            }
        }
        return found
    }

    /// Live documents.
    public func count() throws -> UInt64 {
        let live = try alive()
        var value: UInt64 = 0
        try check { vdb_collection_count(live, &value, $0) }
        return value
    }

    /// Find the nearest documents.
    ///
    /// Ordered by score descending, ties broken by ascending id.
    public func search(_ query: [Float], topK: Int) throws -> [Hit] {
        let live = try alive()
        var results: OpaquePointer?
        try check { error in
            query.withUnsafeBufferPointer { buffer in
                vdb_search(
                    live, buffer.baseAddress, UInt32(buffer.count), topK, &results, error
                )
            }
        }
        guard let results else { return [] }
        // Freed on every path, including a throw: the result holds engine memory, and a loop of
        // searches would otherwise accumulate it.
        defer { vdb_results_free(results) }
        return Self.collect(results)
    }

    /// Find the nearest documents whose metadata matches a filter.
    ///
    /// `topK` counts *matches*, not candidates: a filter excluding most of the collection still
    /// returns up to `topK` results rather than however many happened to survive among the
    /// nearest few.
    public func search(_ query: [Float], topK: Int, filter: Filter) throws -> [Hit] {
        let live = try alive()
        guard let builder = vdb_filter_new() else {
            throw VdbError(code: 0, message: "could not allocate a filter")
        }
        defer { vdb_filter_free(builder) }
        try filter.encode(into: builder)

        var results: OpaquePointer?
        try check { error in
            query.withUnsafeBufferPointer { buffer in
                vdb_search_filtered(
                    live, buffer.baseAddress, UInt32(buffer.count), topK, builder, &results, error
                )
            }
        }
        guard let results else { return [] }
        defer { vdb_results_free(results) }
        return Self.collect(results)
    }

    /// Turn a result handle into hits.
    private static func collect(_ results: OpaquePointer) -> [Hit] {
        let count = vdb_results_len(results)
        var hits: [Hit] = []
        hits.reserveCapacity(count)
        for index in 0..<count {
            var length = 0
            guard let bytes = vdb_results_id(results, index, &length) else { continue }
            let id = String(decoding: UnsafeBufferPointer(start: bytes, count: length), as: UTF8.self)
            hits.append(Hit(id: id, score: vdb_results_score(results, index)))
        }
        return hits
    }

    /// Release the handle. The collection itself is unaffected. Idempotent.
    public func close() {
        if let live = handle {
            handle = nil
            vdb_collection_free(live)
        }
    }

    private func alive() throws -> OpaquePointer {
        guard let handle else {
            throw VdbError(code: 2003, message: "[VDB-2003] the collection is closed")
        }
        return handle
    }
}
