//! Stable numeric error codes.
//!
//! These cross the FFI boundary and are part of the public contract. Two rules:
//!
//! 1. **Codes are append-only.** A released code keeps its meaning forever.
//! 2. **Codes are never reused.** If a variant is removed, its code is retired, not recycled.
//!
//! Bands mirror the error tree so a caller can classify by range without an exhaustive match.

use core::fmt;

/// A stable, machine-matchable identifier for a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrorCode(pub u32);

impl ErrorCode {
    /// The band this code belongs to, e.g. `4` for validation errors.
    pub const fn band(self) -> u32 {
        self.0 / 1000
    }
}

impl ErrorCode {
    /// What this code's band means, for grouping in documentation and in a caller's own
    /// classification.
    ///
    /// Derived from the band rather than listed per code, because the bands are the contract:
    /// a caller that does not recognise a specific code can still tell storage trouble from a
    /// validation mistake.
    pub const fn band_name(self) -> &'static str {
        match self.band() {
            1 => "configuration",
            2 => "lifecycle",
            3 => "not found",
            4 => "conflict and validation",
            5 => "storage",
            6 => "corruption",
            7 => "index and search",
            8 => "transaction",
            9 => "internal and resource",
            _ => "unassigned",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VDB-{:04}", self.0)
    }
}

macro_rules! codes {
    ($($(#[doc = $doc:literal])* $name:ident = $v:expr;)*) => {
        impl ErrorCode { $($(#[doc = $doc])* pub const $name: ErrorCode = ErrorCode($v);)* }
        /// Every code, as `(code, constant name, description)`.
        ///
        /// The description is the constant's own doc comment, captured by the macro rather than
        /// written out a second time. `docs/api/error-codes.md` is generated from this, so the
        /// published table cannot drift from the code the way a hand-maintained one does — and
        /// three SDKs point developers at that table.
        pub const ALL_CODES: &[(ErrorCode, &str, &str)] =
            &[$((ErrorCode($v), stringify!($name), concat!($($doc),*)),)*];
    };
}

codes! {
    // ---- 1xxx configuration -------------------------------------------------
    /// The configured database path was unusable.
    INVALID_DATABASE_PATH = 1001;
    /// A configuration field was outside its permitted range.
    INVALID_CONFIG = 1002;
    /// The storage backend cannot support the requested configuration.
    UNSUPPORTED_CONFIGURATION = 1003;

    // ---- 2xxx lifecycle -----------------------------------------------------
    /// Another handle (or another process) already has this database open.
    DATABASE_ALREADY_OPEN = 2001;
    /// The database directory does not exist and `create_if_missing` was false.
    DATABASE_NOT_FOUND = 2002;
    /// The handle has been closed.
    DATABASE_CLOSED = 2003;
    /// A write was attempted on a read-only handle.
    READ_ONLY = 2004;
    /// The database directory exists but is not a vdb database.
    NOT_A_DATABASE = 2005;

    // ---- 3xxx not found -----------------------------------------------------
    /// No collection with that name.
    COLLECTION_NOT_FOUND = 3001;
    /// No document with that id.
    DOCUMENT_NOT_FOUND = 3002;
    /// No index snapshot of that kind.
    INDEX_NOT_FOUND = 3003;

    // ---- 4xxx conflict / validation ----------------------------------------
    /// A collection with that name already exists.
    COLLECTION_ALREADY_EXISTS = 4001;
    /// A document with that id already exists and the operation was `insert`, not `upsert`.
    DUPLICATE_ID = 4002;
    /// The vector's dimension does not match the collection's.
    INVALID_VECTOR_DIMENSION = 4003;
    /// The vector contained a NaN or infinite component.
    INVALID_VECTOR_DATA = 4004;
    /// The document id was empty, over-long, or not valid UTF-8.
    INVALID_DOCUMENT_ID = 4005;
    /// The collection name was empty, over-long, or used illegal characters.
    INVALID_COLLECTION_NAME = 4006;
    /// A metadata value or document exceeded a size limit.
    METADATA_TOO_LARGE = 4007;
    /// Metadata nesting exceeded the depth limit.
    METADATA_DEPTH_EXCEEDED = 4008;
    /// `top_k` was zero or above the limit.
    TOP_K_OUT_OF_RANGE = 4009;
    /// The filter had too many nodes or was nested too deeply.
    FILTER_TOO_COMPLEX = 4010;
    /// A batch exceeded the operation limit.
    BATCH_TOO_LARGE = 4011;
    /// A path component was empty, relative, over-long, or contained a separator.
    INVALID_PATH = 4012;
    /// A dimension was zero or above the limit.
    INVALID_DIMENSION = 4013;

    // ---- 5xxx storage -------------------------------------------------------
    /// The underlying storage reported an I/O failure.
    STORAGE_IO = 5001;
    /// The operating system denied access.
    PERMISSION_DENIED = 5002;
    /// The storage volume is full.
    INSUFFICIENT_STORAGE = 5003;
    /// The file does not exist.
    FILE_NOT_FOUND = 5004;
    /// The file already exists and the mode required creating it.
    FILE_ALREADY_EXISTS = 5005;
    /// The backend does not implement a capability the engine needs.
    STORAGE_UNSUPPORTED = 5006;
    /// A lock could not be acquired.
    LOCK_UNAVAILABLE = 5007;

    // ---- 6xxx corruption ----------------------------------------------------
    /// A file did not begin with the expected magic bytes.
    BAD_MAGIC = 6001;
    /// A CRC did not match the data it covers.
    CHECKSUM_MISMATCH = 6002;
    /// A file ended before its declared length.
    TRUNCATED_FILE = 6003;
    /// Neither manifest slot was readable.
    NO_VALID_MANIFEST = 6004;
    /// A segment referenced by the manifest is missing.
    MISSING_SEGMENT = 6005;
    /// The file format version is outside the readable range.
    UNSUPPORTED_FORMAT_VERSION = 6006;
    /// A structural field held a value the format does not define.
    MALFORMED_STRUCTURE = 6007;
    /// The index disagrees with the data it indexes; rebuild required.
    INCONSISTENT_INDEX = 6008;

    // ---- 7xxx index / search ------------------------------------------------
    /// The index rejected an operation it cannot perform.
    INDEX_OPERATION_FAILED = 7001;
    /// Index construction failed.
    INDEX_BUILD_FAILED = 7002;
    /// The requested index kind is not compiled into this build.
    INDEX_KIND_UNAVAILABLE = 7003;

    // ---- 8xxx transaction ---------------------------------------------------
    /// The batch was rolled back; no operation in it was applied.
    BATCH_ABORTED = 8001;
    /// A conflicting write was committed concurrently.
    WRITE_CONFLICT = 8002;

    // ---- 9xxx internal / resource -------------------------------------------
    /// The operation was cancelled through its `Budget`.
    CANCELLED = 9001;
    /// A resource limit was reached.
    RESOURCE_EXHAUSTED = 9002;
    /// The operation is not implemented in this version.
    UNSUPPORTED_OPERATION = 9003;
    /// An invariant broke. This is a bug in vdb; please report it.
    INTERNAL = 9999;
}
