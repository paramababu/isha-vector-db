//! The error model.
//!
//! One type crosses the public API: [`DbError`]. It is a tree of structured variants, every one
//! of which carries enough context to diagnose the failure without a debugger, and every one of
//! which maps to a stable [`ErrorCode`] that survives the FFI boundary.
//!
//! Four rules, all enforceable in review:
//!
//! 1. **No stringly-typed errors.** [`InternalError`] is the only free-text variant, and reaching
//!    it is a bug to fix rather than a control-flow tool.
//! 2. **Every error names the thing** — which collection, which id, which path, which offset,
//!    expected versus actual.
//! 3. **Every enum is `#[non_exhaustive]`**, so adding a variant is not a breaking change.
//! 4. **Panics are bugs, not errors.** Predictable failure is a `Result`.

mod code;

pub use code::{ErrorCode, ALL_CODES};

use core::fmt;

use crate::path::{DbPath, PathRejection};

/// The crate-wide result type.
pub type Result<T, E = DbError> = core::result::Result<T, E>;

/// How a caller should react to a failure.
///
/// SDKs use this instead of re-deriving a classification by matching on codes, which is how
/// six bindings end up disagreeing about whether something is retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recoverability {
    /// The caller passed something invalid. Retrying unchanged will fail identically.
    UserError,
    /// A transient condition. The same call may succeed later.
    Retryable,
    /// The database is damaged; `verify`/`repair` or a restore is required.
    NeedsRepair,
    /// Unrecoverable for this handle.
    Fatal,
}

/// Every failure the engine can report.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum DbError {
    /// Bad configuration supplied at open time.
    Config(ConfigError),
    /// The handle is in the wrong state for the operation.
    Lifecycle(LifecycleError),
    /// A named thing does not exist.
    NotFound(NotFoundError),
    /// The operation conflicts with existing data.
    Conflict(ConflictError),
    /// Input failed validation.
    Validation(ValidationError),
    /// An index operation failed.
    Index(IndexError),
    /// The storage backend failed.
    Storage(StorageError),
    /// Persisted bytes are damaged or unreadable.
    Corruption(CorruptionError),
    /// A batch or transaction could not be committed.
    Transaction(TransactionError),
    /// A resource limit was reached.
    ResourceExhausted(ResourceError),
    /// The operation was cancelled through its budget.
    Cancelled,
    /// An invariant broke. A bug in vdb.
    Internal(InternalError),
}

/// Bad configuration supplied at open time.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The database path was unusable.
    InvalidDatabasePath {
        /// The path as given.
        path: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A configuration field was outside its permitted range.
    InvalidField {
        /// The field name, as it appears in `DatabaseConfig`.
        field: &'static str,
        /// The value that was supplied.
        value: String,
        /// The constraint it violated.
        constraint: &'static str,
    },
    /// The storage backend cannot support the requested configuration.
    UnsupportedByBackend {
        /// What was requested.
        requested: &'static str,
        /// The capability the backend does not have.
        missing_capability: &'static str,
    },
}

/// The handle is in the wrong state for the operation.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum LifecycleError {
    /// Another handle or process holds the lock.
    DatabaseAlreadyOpen {
        /// The database root.
        path: String,
        /// Owner details, when the lock file could be read.
        holder: Option<String>,
    },
    /// The directory does not exist and `create_if_missing` was false.
    DatabaseNotFound {
        /// The database root.
        path: String,
    },
    /// The handle has been closed.
    DatabaseClosed,
    /// A write was attempted on a read-only handle.
    ReadOnly {
        /// The operation that was attempted.
        operation: &'static str,
    },
    /// The directory exists but holds no recognisable database.
    NotADatabase {
        /// The database root.
        path: String,
    },
}

/// A named thing does not exist.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum NotFoundError {
    /// No collection with that name.
    Collection {
        /// The name that was looked up.
        name: String,
    },
    /// No document with that id.
    Document {
        /// The collection searched.
        collection: String,
        /// The id that was looked up.
        id: String,
    },
    /// No index snapshot of that kind.
    Index {
        /// The collection searched.
        collection: String,
        /// The index kind that was looked up.
        kind: String,
    },
}

/// The operation conflicts with existing data.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConflictError {
    /// A collection with that name already exists.
    CollectionExists {
        /// The name that was requested.
        name: String,
    },
    /// A document with that id already exists.
    DuplicateId {
        /// The collection written to.
        collection: String,
        /// The id that collided.
        id: String,
    },
}

/// Input failed validation.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ValidationError {
    /// The vector's dimension does not match the collection's.
    InvalidVectorDimension {
        /// The collection written to or searched.
        collection: String,
        /// The dimension the collection was created with.
        expected: u32,
        /// The dimension that was supplied.
        actual: u32,
    },
    /// A vector component was NaN or infinite.
    InvalidVectorData {
        /// Which kind of non-finite value.
        reason: NonFiniteKind,
        /// The position of the first offending component.
        index: usize,
    },
    /// A dimension was zero or above the limit.
    InvalidDimension {
        /// The dimension that was supplied.
        dimension: u32,
        /// The largest permitted dimension.
        max: u32,
    },
    /// The document id was rejected.
    InvalidDocumentId {
        /// Why it was rejected.
        reason: IdRejection,
        /// The length that was supplied, in bytes.
        len: usize,
        /// The largest permitted length, in bytes.
        max: usize,
    },
    /// The collection name was rejected.
    InvalidCollectionName {
        /// The name that was supplied.
        name: String,
        /// Why it was rejected.
        reason: NameRejection,
    },
    /// A path component was rejected. See [`crate::path`].
    InvalidPath {
        /// The path or component that was supplied.
        path: String,
        /// Why it was rejected.
        reason: PathRejection,
    },
    /// A metadata field or document exceeded a size limit.
    MetadataTooLarge {
        /// The field that pushed it over, or `"<document>"` for the whole record.
        field: String,
        /// The size that was supplied, in bytes.
        size: usize,
        /// The limit, in bytes.
        max: usize,
    },
    /// Metadata nesting exceeded the depth limit.
    MetadataDepthExceeded {
        /// The depth that was supplied.
        depth: usize,
        /// The limit.
        max: usize,
    },
    /// `top_k` was zero or above the limit.
    TopKOutOfRange {
        /// The value that was supplied.
        requested: usize,
        /// The limit.
        max: usize,
    },
    /// The filter had too many nodes or was nested too deeply.
    FilterTooComplex {
        /// Node count in the supplied filter.
        nodes: usize,
        /// Nesting depth of the supplied filter.
        depth: usize,
        /// The node limit.
        max_nodes: usize,
        /// The depth limit.
        max_depth: usize,
    },
    /// A batch exceeded the operation limit.
    BatchTooLarge {
        /// The number of operations supplied.
        ops: usize,
        /// The limit.
        max: usize,
    },
}

/// Which non-finite float was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NonFiniteKind {
    /// Not a number.
    Nan,
    /// Positive infinity.
    PosInf,
    /// Negative infinity.
    NegInf,
}

/// Why a document id was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdRejection {
    /// The id was empty.
    Empty,
    /// The id exceeded the length limit.
    TooLong,
    /// The id contained a NUL byte or a control character.
    IllegalCharacter,
    /// The id was not valid UTF-8.
    NotUtf8,
}

/// Why a collection name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NameRejection {
    /// The name was empty.
    Empty,
    /// The name exceeded the length limit.
    TooLong,
    /// The name used a character outside `[A-Za-z0-9_-]`.
    IllegalCharacter,
    /// The name was `.` or `..`, which would escape the database root.
    Reserved,
}

/// An index operation failed.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum IndexError {
    /// The index rejected an operation it cannot perform.
    OperationFailed {
        /// The index kind.
        kind: String,
        /// The operation attempted.
        operation: &'static str,
        /// What went wrong.
        detail: String,
    },
    /// Index construction failed.
    BuildFailed {
        /// The index kind.
        kind: String,
        /// What went wrong.
        detail: String,
    },
    /// The requested index kind is not compiled into this build.
    KindUnavailable {
        /// The kind that was requested.
        kind: String,
        /// The kinds this build does provide.
        available: Vec<String>,
    },
}

/// The storage backend failed.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum StorageError {
    /// An I/O failure.
    Io {
        /// The file involved.
        path: DbPath,
        /// The operation attempted.
        operation: StorageOp,
        /// The backend's description of the failure.
        detail: String,
    },
    /// The operating system denied access.
    PermissionDenied {
        /// The file involved.
        path: DbPath,
        /// The operation attempted.
        operation: StorageOp,
    },
    /// The storage volume is full.
    InsufficientStorage {
        /// Bytes the operation needed.
        required: u64,
        /// Bytes available, when the backend can report it.
        available: Option<u64>,
    },
    /// The file does not exist.
    NotFound {
        /// The file involved.
        path: DbPath,
    },
    /// The file exists and the open mode required creating it.
    AlreadyExists {
        /// The file involved.
        path: DbPath,
    },
    /// The backend does not implement a capability the engine needs.
    Unsupported {
        /// The operation attempted.
        operation: StorageOp,
        /// The backend's name.
        backend: &'static str,
    },
    /// A lock could not be acquired.
    LockUnavailable {
        /// The lock file.
        path: DbPath,
    },
}

/// Which storage operation failed. Kept as an enum so bindings need no string matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageOp {
    /// Opening a file.
    Open,
    /// Reading at an offset.
    Read,
    /// Writing at an offset.
    Write,
    /// Appending.
    Append,
    /// Flushing to durable storage.
    Sync,
    /// Truncating.
    Truncate,
    /// Querying length or metadata.
    Metadata,
    /// Removing a file.
    Remove,
    /// Renaming.
    Rename,
    /// Creating a directory.
    CreateDir,
    /// Listing a directory.
    ListDir,
    /// Memory-mapping.
    Map,
    /// Locking.
    Lock,
}

/// Persisted bytes are damaged or unreadable.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CorruptionError {
    /// A file did not begin with the expected magic bytes.
    BadMagic {
        /// The file involved.
        path: DbPath,
        /// The magic the reader required.
        expected: [u8; 8],
        /// The magic that was found.
        found: [u8; 8],
    },
    /// A CRC did not match the data it covers.
    ChecksumMismatch {
        /// The file involved.
        path: DbPath,
        /// Byte offset of the block whose CRC failed.
        offset: u64,
        /// The CRC stored in the file.
        expected: u32,
        /// The CRC computed over the bytes read.
        found: u32,
    },
    /// A file ended before its declared length.
    TruncatedFile {
        /// The file involved.
        path: DbPath,
        /// The length the header declared.
        expected_len: u64,
        /// The length the file actually has.
        actual_len: u64,
    },
    /// Neither manifest slot was readable.
    NoValidManifest {
        /// The database root.
        path: DbPath,
        /// Why slot A was rejected.
        slot_a: String,
        /// Why slot B was rejected.
        slot_b: String,
    },
    /// A segment referenced by the manifest is missing.
    MissingSegment {
        /// The collection involved.
        collection: String,
        /// The segment id the manifest referenced.
        segment: u64,
    },
    /// The file format version is outside the readable range.
    UnsupportedFormatVersion {
        /// The file involved.
        path: DbPath,
        /// The version the file declares.
        found: u16,
        /// The oldest version this build can read.
        min_readable: u16,
        /// The version this build writes.
        current: u16,
    },
    /// A structural field held a value the format does not define.
    MalformedStructure {
        /// The file involved.
        path: DbPath,
        /// Byte offset of the field.
        offset: u64,
        /// What the reader expected there.
        detail: String,
    },
    /// The index disagrees with the data it indexes.
    InconsistentIndex {
        /// The collection involved.
        collection: String,
        /// What disagreed.
        detail: String,
    },
}

/// A batch or transaction could not be committed.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TransactionError {
    /// The batch was rolled back; nothing in it was applied.
    Aborted {
        /// Index of the operation that failed.
        failed_at: usize,
        /// The number of operations in the batch.
        total_ops: usize,
        /// The failure that caused the abort.
        cause: Box<DbError>,
    },
    /// A conflicting write was committed concurrently.
    WriteConflict {
        /// The collection involved.
        collection: String,
        /// The id that conflicted.
        id: String,
    },
}

/// A resource limit was reached.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ResourceError {
    /// A limit defined in `validation::limits` was reached.
    LimitReached {
        /// The limit's name.
        resource: &'static str,
        /// The value that was requested.
        requested: u64,
        /// The limit.
        limit: u64,
    },
    /// An allocation would have exceeded what the caller permitted.
    OutOfMemory {
        /// Bytes requested.
        requested: u64,
    },
}

/// An invariant broke. Always a bug in vdb.
#[derive(Debug, Clone, PartialEq)]
pub struct InternalError {
    /// What was violated.
    pub message: String,
    /// Where, as `file:line`.
    pub location: &'static str,
}

/// Construct an [`InternalError`] carrying the call site.
#[macro_export]
macro_rules! internal_error {
    ($($arg:tt)*) => {
        $crate::error::DbError::Internal($crate::error::InternalError {
            message: format!($($arg)*),
            location: concat!(file!(), ":", line!()),
        })
    };
}

impl DbError {
    /// The stable numeric code for this failure.
    pub fn code(&self) -> ErrorCode {
        use DbError as E;
        match self {
            E::Config(e) => match e {
                ConfigError::InvalidDatabasePath { .. } => ErrorCode::INVALID_DATABASE_PATH,
                ConfigError::InvalidField { .. } => ErrorCode::INVALID_CONFIG,
                ConfigError::UnsupportedByBackend { .. } => ErrorCode::UNSUPPORTED_CONFIGURATION,
            },
            E::Lifecycle(e) => match e {
                LifecycleError::DatabaseAlreadyOpen { .. } => ErrorCode::DATABASE_ALREADY_OPEN,
                LifecycleError::DatabaseNotFound { .. } => ErrorCode::DATABASE_NOT_FOUND,
                LifecycleError::DatabaseClosed => ErrorCode::DATABASE_CLOSED,
                LifecycleError::ReadOnly { .. } => ErrorCode::READ_ONLY,
                LifecycleError::NotADatabase { .. } => ErrorCode::NOT_A_DATABASE,
            },
            E::NotFound(e) => match e {
                NotFoundError::Collection { .. } => ErrorCode::COLLECTION_NOT_FOUND,
                NotFoundError::Document { .. } => ErrorCode::DOCUMENT_NOT_FOUND,
                NotFoundError::Index { .. } => ErrorCode::INDEX_NOT_FOUND,
            },
            E::Conflict(e) => match e {
                ConflictError::CollectionExists { .. } => ErrorCode::COLLECTION_ALREADY_EXISTS,
                ConflictError::DuplicateId { .. } => ErrorCode::DUPLICATE_ID,
            },
            E::Validation(e) => match e {
                ValidationError::InvalidVectorDimension { .. } => {
                    ErrorCode::INVALID_VECTOR_DIMENSION
                }
                ValidationError::InvalidVectorData { .. } => ErrorCode::INVALID_VECTOR_DATA,
                ValidationError::InvalidDimension { .. } => ErrorCode::INVALID_DIMENSION,
                ValidationError::InvalidDocumentId { .. } => ErrorCode::INVALID_DOCUMENT_ID,
                ValidationError::InvalidCollectionName { .. } => ErrorCode::INVALID_COLLECTION_NAME,
                ValidationError::InvalidPath { .. } => ErrorCode::INVALID_PATH,
                ValidationError::MetadataTooLarge { .. } => ErrorCode::METADATA_TOO_LARGE,
                ValidationError::MetadataDepthExceeded { .. } => ErrorCode::METADATA_DEPTH_EXCEEDED,
                ValidationError::TopKOutOfRange { .. } => ErrorCode::TOP_K_OUT_OF_RANGE,
                ValidationError::FilterTooComplex { .. } => ErrorCode::FILTER_TOO_COMPLEX,
                ValidationError::BatchTooLarge { .. } => ErrorCode::BATCH_TOO_LARGE,
            },
            E::Index(e) => match e {
                IndexError::OperationFailed { .. } => ErrorCode::INDEX_OPERATION_FAILED,
                IndexError::BuildFailed { .. } => ErrorCode::INDEX_BUILD_FAILED,
                IndexError::KindUnavailable { .. } => ErrorCode::INDEX_KIND_UNAVAILABLE,
            },
            E::Storage(e) => match e {
                StorageError::Io { .. } => ErrorCode::STORAGE_IO,
                StorageError::PermissionDenied { .. } => ErrorCode::PERMISSION_DENIED,
                StorageError::InsufficientStorage { .. } => ErrorCode::INSUFFICIENT_STORAGE,
                StorageError::NotFound { .. } => ErrorCode::FILE_NOT_FOUND,
                StorageError::AlreadyExists { .. } => ErrorCode::FILE_ALREADY_EXISTS,
                StorageError::Unsupported { .. } => ErrorCode::STORAGE_UNSUPPORTED,
                StorageError::LockUnavailable { .. } => ErrorCode::LOCK_UNAVAILABLE,
            },
            E::Corruption(e) => match e {
                CorruptionError::BadMagic { .. } => ErrorCode::BAD_MAGIC,
                CorruptionError::ChecksumMismatch { .. } => ErrorCode::CHECKSUM_MISMATCH,
                CorruptionError::TruncatedFile { .. } => ErrorCode::TRUNCATED_FILE,
                CorruptionError::NoValidManifest { .. } => ErrorCode::NO_VALID_MANIFEST,
                CorruptionError::MissingSegment { .. } => ErrorCode::MISSING_SEGMENT,
                CorruptionError::UnsupportedFormatVersion { .. } => {
                    ErrorCode::UNSUPPORTED_FORMAT_VERSION
                }
                CorruptionError::MalformedStructure { .. } => ErrorCode::MALFORMED_STRUCTURE,
                CorruptionError::InconsistentIndex { .. } => ErrorCode::INCONSISTENT_INDEX,
            },
            E::Transaction(e) => match e {
                TransactionError::Aborted { .. } => ErrorCode::BATCH_ABORTED,
                TransactionError::WriteConflict { .. } => ErrorCode::WRITE_CONFLICT,
            },
            E::ResourceExhausted(_) => ErrorCode::RESOURCE_EXHAUSTED,
            E::Cancelled => ErrorCode::CANCELLED,
            E::Internal(_) => ErrorCode::INTERNAL,
        }
    }

    /// How a caller should react.
    pub fn recoverability(&self) -> Recoverability {
        use DbError as E;
        use Recoverability as R;
        match self {
            E::Config(_) | E::Validation(_) | E::Conflict(_) => R::UserError,
            E::NotFound(_) => R::UserError,
            E::Lifecycle(LifecycleError::DatabaseAlreadyOpen { .. }) => R::Retryable,
            E::Lifecycle(_) => R::Fatal,
            E::Storage(StorageError::LockUnavailable { .. })
            | E::Storage(StorageError::Io { .. }) => R::Retryable,
            E::Storage(StorageError::InsufficientStorage { .. }) => R::Retryable,
            E::Storage(_) => R::Fatal,
            // An index is derived data: it can always be rebuilt from the segments.
            E::Index(_) | E::Corruption(CorruptionError::InconsistentIndex { .. }) => {
                R::NeedsRepair
            }
            E::Corruption(_) => R::NeedsRepair,
            E::Transaction(_) => R::Retryable,
            E::ResourceExhausted(_) => R::Retryable,
            E::Cancelled => R::Retryable,
            E::Internal(_) => R::Fatal,
        }
    }

    /// Whether this failure indicates damaged persisted data.
    pub fn is_corruption(&self) -> bool {
        matches!(self, DbError::Corruption(_))
    }
}

// ---------------------------------------------------------------------------
// Display. Written by hand rather than derived, so no dependency sits between
// a user's data and the message they get when something goes wrong.
// ---------------------------------------------------------------------------

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] ", self.code())?;
        match self {
            Self::Config(e) => write!(f, "{e}"),
            Self::Lifecycle(e) => write!(f, "{e}"),
            Self::NotFound(e) => write!(f, "{e}"),
            Self::Conflict(e) => write!(f, "{e}"),
            Self::Validation(e) => write!(f, "{e}"),
            Self::Index(e) => write!(f, "{e}"),
            Self::Storage(e) => write!(f, "{e}"),
            Self::Corruption(e) => write!(f, "{e}"),
            Self::Transaction(e) => write!(f, "{e}"),
            Self::ResourceExhausted(e) => write!(f, "{e}"),
            Self::Cancelled => write!(f, "operation cancelled"),
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDatabasePath { path, reason } => {
                write!(f, "invalid database path {path:?}: {reason}")
            }
            Self::InvalidField {
                field,
                value,
                constraint,
            } => {
                write!(f, "config field `{field}` = {value}: must be {constraint}")
            }
            Self::UnsupportedByBackend {
                requested,
                missing_capability,
            } => write!(
                f,
                "storage backend cannot provide {requested}: it lacks `{missing_capability}`"
            ),
        }
    }
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseAlreadyOpen { path, holder } => match holder {
                Some(h) => write!(f, "database {path:?} is already open (held by {h})"),
                None => write!(f, "database {path:?} is already open"),
            },
            Self::DatabaseNotFound { path } => write!(
                f,
                "no database at {path:?} and `create_if_missing` is false"
            ),
            Self::DatabaseClosed => write!(f, "database handle is closed"),
            Self::ReadOnly { operation } => {
                write!(f, "cannot {operation}: the database is open read-only")
            }
            Self::NotADatabase { path } => {
                write!(f, "{path:?} exists but contains no vdb database")
            }
        }
    }
}

impl fmt::Display for NotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collection { name } => write!(f, "collection {name:?} not found"),
            Self::Document { collection, id } => {
                write!(f, "document {id:?} not found in collection {collection:?}")
            }
            Self::Index { collection, kind } => {
                write!(f, "no {kind} index for collection {collection:?}")
            }
        }
    }
}

impl fmt::Display for ConflictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CollectionExists { name } => write!(f, "collection {name:?} already exists"),
            Self::DuplicateId { collection, id } => write!(
                f,
                "document {id:?} already exists in collection {collection:?}; use upsert to replace it"
            ),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVectorDimension { collection, expected, actual } => write!(
                f,
                "collection {collection:?} stores {expected}-dimensional vectors, got {actual}"
            ),
            Self::InvalidVectorData { reason, index } => {
                write!(f, "vector component {index} is {reason}")
            }
            Self::InvalidDimension { dimension, max } => {
                write!(f, "dimension {dimension} must be between 1 and {max}")
            }
            Self::InvalidDocumentId { reason, len, max } => {
                write!(f, "document id {reason} (length {len}, maximum {max})")
            }
            Self::InvalidCollectionName { name, reason } => {
                write!(f, "collection name {name:?} {reason}")
            }
            Self::InvalidPath { path, reason } => write!(f, "path {path:?} {reason}"),
            Self::MetadataTooLarge { field, size, max } => write!(
                f,
                "metadata field {field:?} is {size} bytes, maximum is {max}"
            ),
            Self::MetadataDepthExceeded { depth, max } => {
                write!(f, "metadata nested {depth} deep, maximum is {max}")
            }
            Self::TopKOutOfRange { requested, max } => {
                write!(f, "top_k must be between 1 and {max}, got {requested}")
            }
            Self::FilterTooComplex { nodes, depth, max_nodes, max_depth } => write!(
                f,
                "filter has {nodes} nodes at depth {depth}; limits are {max_nodes} nodes, depth {max_depth}"
            ),
            Self::BatchTooLarge { ops, max } => {
                write!(f, "batch has {ops} operations, maximum is {max}")
            }
        }
    }
}

impl fmt::Display for NonFiniteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Nan => "NaN",
            Self::PosInf => "+infinity",
            Self::NegInf => "-infinity",
        })
    }
}

impl fmt::Display for IdRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "is empty",
            Self::TooLong => "is too long",
            Self::IllegalCharacter => "contains a NUL or control character",
            Self::NotUtf8 => "is not valid UTF-8",
        })
    }
}

impl fmt::Display for NameRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "is empty",
            Self::TooLong => "is too long",
            Self::IllegalCharacter => "contains a character outside [A-Za-z0-9_-]",
            Self::Reserved => "is reserved",
        })
    }
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OperationFailed {
                kind,
                operation,
                detail,
            } => {
                write!(f, "{kind} index failed to {operation}: {detail}")
            }
            Self::BuildFailed { kind, detail } => write!(f, "{kind} index build failed: {detail}"),
            Self::KindUnavailable { kind, available } => write!(
                f,
                "index kind {kind:?} is not in this build; available: {}",
                available.join(", ")
            ),
        }
    }
}

impl fmt::Display for StorageOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::Write => "write",
            Self::Append => "append",
            Self::Sync => "sync",
            Self::Truncate => "truncate",
            Self::Metadata => "stat",
            Self::Remove => "remove",
            Self::Rename => "rename",
            Self::CreateDir => "create directory",
            Self::ListDir => "list directory",
            Self::Map => "memory-map",
            Self::Lock => "lock",
        })
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                path,
                operation,
                detail,
            } => {
                write!(f, "failed to {operation} {path}: {detail}")
            }
            Self::PermissionDenied { path, operation } => {
                write!(f, "permission denied trying to {operation} {path}")
            }
            Self::InsufficientStorage {
                required,
                available,
            } => match available {
                Some(a) => write!(f, "need {required} bytes, only {a} available"),
                None => write!(f, "need {required} bytes; storage is full"),
            },
            Self::NotFound { path } => write!(f, "file {path} not found"),
            Self::AlreadyExists { path } => write!(f, "file {path} already exists"),
            Self::Unsupported { operation, backend } => {
                write!(f, "the {backend} storage backend cannot {operation}")
            }
            Self::LockUnavailable { path } => write!(f, "could not acquire lock {path}"),
        }
    }
}

impl fmt::Display for CorruptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { path, expected, found } => write!(
                f,
                "{path} is not the expected file kind: magic {} != {}",
                Magic(found),
                Magic(expected)
            ),
            Self::ChecksumMismatch { path, offset, expected, found } => write!(
                f,
                "checksum mismatch in {path} at offset {offset}: stored {expected:#010x}, computed {found:#010x}"
            ),
            Self::TruncatedFile { path, expected_len, actual_len } => write!(
                f,
                "{path} is truncated: header declares {expected_len} bytes, file has {actual_len}"
            ),
            Self::NoValidManifest { path, slot_a, slot_b } => write!(
                f,
                "no valid manifest in {path}: slot A {slot_a}, slot B {slot_b}"
            ),
            Self::MissingSegment { collection, segment } => write!(
                f,
                "segment {segment:06} of collection {collection:?} is referenced by the manifest but missing"
            ),
            Self::UnsupportedFormatVersion { path, found, min_readable, current } => write!(
                f,
                "{path} is format version {found}; this build reads {min_readable}..={current}"
            ),
            Self::MalformedStructure { path, offset, detail } => {
                write!(f, "malformed structure in {path} at offset {offset}: {detail}")
            }
            Self::InconsistentIndex { collection, detail } => write!(
                f,
                "index for collection {collection:?} is inconsistent ({detail}); rebuild it"
            ),
        }
    }
}

/// Renders magic bytes readably, so a mismatch message is diagnosable at a glance.
struct Magic<'a>(&'a [u8; 8]);

impl fmt::Display for Magic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for &b in self.0 {
            if b.is_ascii_graphic() {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        f.write_str("\"")
    }
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aborted { failed_at, total_ops, cause } => write!(
                f,
                "batch aborted at operation {failed_at} of {total_ops}; nothing was applied: {cause}"
            ),
            Self::WriteConflict { collection, id } => write!(
                f,
                "write conflict on {id:?} in collection {collection:?}"
            ),
        }
    }
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitReached {
                resource,
                requested,
                limit,
            } => {
                write!(
                    f,
                    "{resource} limit reached: requested {requested}, limit {limit}"
                )
            }
            Self::OutOfMemory { requested } => {
                write!(f, "allocation of {requested} bytes refused")
            }
        }
    }
}

impl fmt::Display for InternalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "internal error at {}: {} (this is a bug in vdb; please report it)",
            self.location, self.message
        )
    }
}

impl std::error::Error for DbError {}

/// Convert a format-level decode failure into a [`DbError`] with no path attached.
///
/// `vdb-format` decodes byte slices and has no concept of a file, so the layer that *does* know
/// the path is responsible for attaching it — see [`from_format_at`]. This pathless form is for
/// decoding buffers that never came from a file, such as metadata handed in by a caller.
pub fn from_format(e: vdb_format::FormatError) -> DbError {
    from_format_at(e, &DbPath::root())
}

/// Convert a format-level decode failure, attaching the file it came from.
pub fn from_format_at(e: vdb_format::FormatError, path: &DbPath) -> DbError {
    use vdb_format::FormatError as F;
    match e {
        F::BadMagic { expected, found } => CorruptionError::BadMagic {
            path: path.clone(),
            expected,
            found,
        }
        .into(),
        F::UnsupportedVersion {
            found,
            min_readable,
            current,
        } => CorruptionError::UnsupportedFormatVersion {
            path: path.clone(),
            found,
            min_readable,
            current,
        }
        .into(),
        F::ChecksumMismatch {
            offset,
            expected,
            found,
        } => CorruptionError::ChecksumMismatch {
            path: path.clone(),
            offset,
            expected,
            found,
        }
        .into(),
        F::Truncated {
            offset,
            needed,
            available,
        } => CorruptionError::TruncatedFile {
            path: path.clone(),
            expected_len: offset.saturating_add(needed),
            actual_len: available,
        }
        .into(),
        // A length field that exceeds the input is the condition that turns a naive parser into
        // an out-of-memory crash, so it keeps its own distinct description rather than being
        // folded into "truncated".
        F::LengthExceedsInput {
            offset,
            claimed,
            available,
        } => CorruptionError::MalformedStructure {
            path: path.clone(),
            offset,
            detail: format!("length field claims {claimed} bytes, {available} available"),
        }
        .into(),
        F::Malformed { offset, kind } => CorruptionError::MalformedStructure {
            path: path.clone(),
            offset,
            detail: kind.to_string(),
        }
        .into(),
        // `FormatError` is #[non_exhaustive] so the format crate can add variants without
        // breaking us. Anything unrecognised is still corruption, and still reported with the
        // format crate's own description rather than swallowed.
        other => CorruptionError::MalformedStructure {
            path: path.clone(),
            offset: 0,
            detail: other.to_string(),
        }
        .into(),
    }
}

// Convenience constructors for the sub-enums, so call sites read
// `DbError::from(StorageError::NotFound { .. })` rather than nesting by hand.
macro_rules! from_sub {
    ($($sub:ty => $variant:ident),* $(,)?) => {
        $(impl From<$sub> for DbError {
            fn from(e: $sub) -> Self { DbError::$variant(e) }
        })*
    };
}

from_sub! {
    ConfigError => Config,
    LifecycleError => Lifecycle,
    NotFoundError => NotFound,
    ConflictError => Conflict,
    ValidationError => Validation,
    IndexError => Index,
    StorageError => Storage,
    CorruptionError => Corruption,
    TransactionError => Transaction,
    ResourceError => ResourceExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_unique() {
        let mut seen: HashSet<u32> = HashSet::new();
        for (code, name) in ALL_CODES {
            assert!(
                seen.insert(code.0),
                "duplicate error code {} on {name}",
                code.0
            );
        }
    }

    #[test]
    fn codes_are_banded_consistently() {
        for (code, name) in ALL_CODES {
            let band = code.band();
            assert!(
                (1..=9).contains(&band),
                "{name} has code {} outside the bands",
                code.0
            );
        }
    }

    /// Every error the engine can produce must resolve to a code that exists in the table.
    #[test]
    fn every_variant_maps_to_a_registered_code() {
        let known: HashSet<u32> = ALL_CODES.iter().map(|(c, _)| c.0).collect();
        for e in sample_of_every_variant() {
            assert!(
                known.contains(&e.code().0),
                "{e:?} produced unregistered code {}",
                e.code().0
            );
        }
    }

    /// The property that makes error messages useful: they name the thing that went wrong.
    #[test]
    fn messages_include_their_context() {
        let e = DbError::Validation(ValidationError::InvalidVectorDimension {
            collection: "products".into(),
            expected: 768,
            actual: 384,
        });
        let s = e.to_string();
        assert!(s.contains("products"), "{s}");
        assert!(s.contains("768"), "{s}");
        assert!(s.contains("384"), "{s}");
        assert!(s.contains("VDB-4003"), "{s}");
    }

    #[test]
    fn corruption_messages_render_magic_readably() {
        let e = DbError::Corruption(CorruptionError::BadMagic {
            path: DbPath::parse("MANIFEST-A").unwrap(),
            expected: *b"VDB1MANI",
            found: [0x00, 0x01, b'V', b'D', b'B', 0xff, 0x00, 0x00],
        });
        let s = e.to_string();
        assert!(s.contains("VDB1MANI"), "{s}");
        assert!(s.contains("\\x00\\x01VDB\\xff"), "{s}");
    }

    #[test]
    fn recoverability_classifies_the_cases_sdks_care_about() {
        use Recoverability as R;
        assert_eq!(
            DbError::Validation(ValidationError::TopKOutOfRange {
                requested: 0,
                max: 10_000
            })
            .recoverability(),
            R::UserError
        );
        assert_eq!(
            DbError::Corruption(CorruptionError::MissingSegment {
                collection: "c".into(),
                segment: 7
            })
            .recoverability(),
            R::NeedsRepair
        );
        assert_eq!(DbError::Cancelled.recoverability(), R::Retryable);
        assert_eq!(
            internal_error!("invariant broken").recoverability(),
            R::Fatal
        );
    }

    #[test]
    fn internal_error_carries_its_call_site() {
        let e = internal_error!("segment {} vanished", 3);
        match &e {
            DbError::Internal(i) => {
                assert!(i.message.contains("segment 3"));
                assert!(i.location.contains("error/mod.rs"), "{}", i.location);
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn is_corruption_only_for_corruption() {
        assert!(DbError::Corruption(CorruptionError::MissingSegment {
            collection: "c".into(),
            segment: 1
        })
        .is_corruption());
        assert!(!DbError::Cancelled.is_corruption());
    }

    /// One instance of each leaf, kept exhaustive by review. Adding a variant without adding it
    /// here is caught by `every_variant_maps_to_a_registered_code` only if the code is missing —
    /// so the real guard is that this list is part of the error-model review checklist.
    fn sample_of_every_variant() -> Vec<DbError> {
        let p = DbPath::parse("f").unwrap();
        vec![
            ConfigError::InvalidDatabasePath {
                path: "x".into(),
                reason: "r".into(),
            }
            .into(),
            ConfigError::InvalidField {
                field: "top_k",
                value: "0".into(),
                constraint: "> 0",
            }
            .into(),
            ConfigError::UnsupportedByBackend {
                requested: "mmap",
                missing_capability: "mmap",
            }
            .into(),
            LifecycleError::DatabaseAlreadyOpen {
                path: "x".into(),
                holder: None,
            }
            .into(),
            LifecycleError::DatabaseNotFound { path: "x".into() }.into(),
            LifecycleError::DatabaseClosed.into(),
            LifecycleError::ReadOnly {
                operation: "insert",
            }
            .into(),
            LifecycleError::NotADatabase { path: "x".into() }.into(),
            NotFoundError::Collection { name: "c".into() }.into(),
            NotFoundError::Document {
                collection: "c".into(),
                id: "d".into(),
            }
            .into(),
            NotFoundError::Index {
                collection: "c".into(),
                kind: "flat".into(),
            }
            .into(),
            ConflictError::CollectionExists { name: "c".into() }.into(),
            ConflictError::DuplicateId {
                collection: "c".into(),
                id: "d".into(),
            }
            .into(),
            ValidationError::InvalidVectorDimension {
                collection: "c".into(),
                expected: 3,
                actual: 4,
            }
            .into(),
            ValidationError::InvalidVectorData {
                reason: NonFiniteKind::Nan,
                index: 0,
            }
            .into(),
            ValidationError::InvalidDimension {
                dimension: 0,
                max: 65_536,
            }
            .into(),
            ValidationError::InvalidDocumentId {
                reason: IdRejection::Empty,
                len: 0,
                max: 512,
            }
            .into(),
            ValidationError::InvalidCollectionName {
                name: "..".into(),
                reason: NameRejection::Reserved,
            }
            .into(),
            ValidationError::InvalidPath {
                path: "..".into(),
                reason: PathRejection::Empty,
            }
            .into(),
            ValidationError::MetadataTooLarge {
                field: "f".into(),
                size: 1,
                max: 0,
            }
            .into(),
            ValidationError::MetadataDepthExceeded { depth: 17, max: 16 }.into(),
            ValidationError::TopKOutOfRange {
                requested: 0,
                max: 1,
            }
            .into(),
            ValidationError::FilterTooComplex {
                nodes: 1,
                depth: 1,
                max_nodes: 0,
                max_depth: 0,
            }
            .into(),
            ValidationError::BatchTooLarge { ops: 1, max: 0 }.into(),
            IndexError::OperationFailed {
                kind: "flat".into(),
                operation: "add",
                detail: "d".into(),
            }
            .into(),
            IndexError::BuildFailed {
                kind: "flat".into(),
                detail: "d".into(),
            }
            .into(),
            IndexError::KindUnavailable {
                kind: "hnsw".into(),
                available: vec!["flat".into()],
            }
            .into(),
            StorageError::Io {
                path: p.clone(),
                operation: StorageOp::Read,
                detail: "d".into(),
            }
            .into(),
            StorageError::PermissionDenied {
                path: p.clone(),
                operation: StorageOp::Open,
            }
            .into(),
            StorageError::InsufficientStorage {
                required: 1,
                available: Some(0),
            }
            .into(),
            StorageError::NotFound { path: p.clone() }.into(),
            StorageError::AlreadyExists { path: p.clone() }.into(),
            StorageError::Unsupported {
                operation: StorageOp::Map,
                backend: "memory",
            }
            .into(),
            StorageError::LockUnavailable { path: p.clone() }.into(),
            CorruptionError::BadMagic {
                path: p.clone(),
                expected: *b"VDB1MANI",
                found: [0; 8],
            }
            .into(),
            CorruptionError::ChecksumMismatch {
                path: p.clone(),
                offset: 0,
                expected: 1,
                found: 2,
            }
            .into(),
            CorruptionError::TruncatedFile {
                path: p.clone(),
                expected_len: 2,
                actual_len: 1,
            }
            .into(),
            CorruptionError::NoValidManifest {
                path: p.clone(),
                slot_a: "bad crc".into(),
                slot_b: "missing".into(),
            }
            .into(),
            CorruptionError::MissingSegment {
                collection: "c".into(),
                segment: 1,
            }
            .into(),
            CorruptionError::UnsupportedFormatVersion {
                path: p.clone(),
                found: 2,
                min_readable: 1,
                current: 1,
            }
            .into(),
            CorruptionError::MalformedStructure {
                path: p.clone(),
                offset: 0,
                detail: "d".into(),
            }
            .into(),
            CorruptionError::InconsistentIndex {
                collection: "c".into(),
                detail: "d".into(),
            }
            .into(),
            TransactionError::Aborted {
                failed_at: 1,
                total_ops: 2,
                cause: Box::new(DbError::Cancelled),
            }
            .into(),
            TransactionError::WriteConflict {
                collection: "c".into(),
                id: "d".into(),
            }
            .into(),
            ResourceError::LimitReached {
                resource: "batch",
                requested: 2,
                limit: 1,
            }
            .into(),
            ResourceError::OutOfMemory { requested: 1 }.into(),
            DbError::Cancelled,
            internal_error!("boom"),
        ]
    }

    /// Nothing is worse than an error whose Display is empty or panics.
    #[test]
    fn every_variant_renders_non_empty() {
        for e in sample_of_every_variant() {
            let s = e.to_string();
            assert!(s.len() > 10, "{e:?} rendered as {s:?}");
            assert!(s.starts_with("[VDB-"), "{s}");
        }
    }
}
