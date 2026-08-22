import CVdb
import Foundation

/// An embedded, offline-first vector database.
///
/// The API mirrors every other vdb SDK: naming follows Swift's conventions, but semantics,
/// argument order, defaults and error classification do not change. Semantic divergence between
/// SDKs is the fastest way to make a cross-platform library untrustworthy.

/// A failure from the database.
public struct VdbError: Error, CustomStringConvertible {
    /// The stable numeric code, documented in `docs/api/error-codes.md`.
    ///
    /// Match on this rather than on `message`, which is allowed to change between releases.
    public let code: UInt32
    /// A human-readable description.
    public let message: String

    public var description: String { message }

    /// Take ownership of an error the library produced, freeing it.
    ///
    /// Swift imports `typedef struct vdb_error vdb_error_t` — an incomplete type — as
    /// `OpaquePointer`, which is exactly what an opaque handle should look like from here.
    static func take(_ pointer: OpaquePointer?) -> VdbError {
        guard let pointer else {
            return VdbError(code: 0, message: "the operation failed without a description")
        }
        defer { vdb_error_free(pointer) }
        let code = vdb_error_code(pointer)
        let message = vdb_error_message(pointer).map { String(cString: $0) } ?? "unknown failure"
        return VdbError(code: code, message: message)
    }

    static func boundary(_ status: Int32) -> VdbError {
        let text: String
        switch status {
        case VDB_NULL_POINTER: text = "a required argument was missing"
        case VDB_INVALID_UTF8: text = "a string argument was not valid UTF-8"
        case VDB_INVALID_ARGUMENT: text = "an argument was outside its permitted range"
        case VDB_INTERNAL: text = "an internal error occurred; please report it"
        default: text = "the operation failed with status \(status)"
        }
        return VdbError(code: 0, message: text)
    }
}

/// How aggressively writes are made durable.
public enum Durability {
    /// Sync every write. Safe against power loss; slow on flash.
    case full
    /// Sync on batch commit, flush and close. The default.
    ///
    /// In every mode a process crash loses nothing — the bytes are already in the page cache.
    /// Only power loss can lose an unsynced write. iOS kills applications routinely and power
    /// loss is rare, so this is the sensible default rather than `.full`.
    case batch
    /// Sync on flush and close only. For bulk import.
    case relaxed

    // The C enum imports as `UInt32`-backed while the functions take `int32_t`, so the
    // conversion is explicit rather than left to an implicit widening that does not exist.
    var raw: Int32 {
        switch self {
        case .full: return Int32(VDB_DURABILITY_FULL.rawValue)
        case .batch: return Int32(VDB_DURABILITY_BATCH.rawValue)
        case .relaxed: return Int32(VDB_DURABILITY_RELAXED.rawValue)
        }
    }
}

/// Similarity metric.
public enum Metric {
    /// Cosine similarity. Ignores magnitude, which is usually what embeddings want.
    case cosine
    /// Euclidean distance.
    case l2
    /// Inner product. Rewards magnitude as well as direction, so a longer vector can outrank an
    /// exact match — that is what the inner product means, not a defect.
    case dot

    var raw: Int32 {
        switch self {
        case .cosine: return Int32(VDB_METRIC_COSINE.rawValue)
        case .l2: return Int32(VDB_METRIC_L2.rawValue)
        case .dot: return Int32(VDB_METRIC_DOT.rawValue)
        }
    }
}

/// One search result.
public struct Hit: Sendable, Equatable {
    /// The document's id.
    public let id: String
    /// Its score. Always higher-is-better, whatever the metric.
    public let score: Float
}

/// Library and format versions, for diagnostics and for refusing a mismatched binary.
public enum Version {
    /// The library version.
    public static var library: String { String(cString: vdb_version()) }
    /// The ABI revision this binary implements.
    public static var abi: UInt32 { vdb_abi_version() }
    /// The on-disk format version it writes.
    public static var format: UInt32 { vdb_format_version() }
}

/// Run a fallible C call, converting its status into a Swift error.
@inline(__always)
func check(_ body: (UnsafeMutablePointer<OpaquePointer?>) -> Int32) throws {
    var error: OpaquePointer?
    let status = body(&error)
    guard status == VDB_OK else {
        // A negative status is a boundary failure and fills no error slot; a positive one comes
        // from the engine and does.
        throw error != nil ? VdbError.take(error) : VdbError.boundary(status)
    }
}
