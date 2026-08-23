//! Configuration for opening a database and creating a collection.

use isha_vector_db_format::{IdKind, IndexSpec, Metric, VectorDType};

use crate::error::{ConfigError, Result};
use crate::persistence::Durability;
use crate::validation::{self, limits};
use crate::vector;

/// How to open a database.
///
/// Note what is *absent*: a path. The database root belongs to the [`Storage`](crate::storage)
/// implementation, which is the only component that knows what a filesystem is. That is not an
/// omission — it is the mechanism by which the engine stays platform-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DatabaseConfig {
    /// Create the database if the storage holds none. Default `true`.
    pub create_if_missing: bool,
    /// Open without taking the write lock and refuse every mutation. Default `false`.
    pub read_only: bool,
    /// How aggressively writes are made durable. Default [`Durability::Batch`].
    pub durability: Durability,
    /// Flush a collection's memtable into a segment once it exceeds this many bytes.
    ///
    /// The trade-off is bounded memory against write amplification: a small threshold produces
    /// many small segments that compaction must later merge; a large one holds more unflushed
    /// data, which a crash turns into a longer log replay on the next open. 8 MiB is chosen for
    /// a phone, where a 64 MiB memtable would be a meaningful fraction of the app's budget.
    pub flush_threshold_bytes: usize,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            create_if_missing: true,
            read_only: false,
            durability: Durability::default(),
            flush_threshold_bytes: 8 * 1024 * 1024,
        }
    }
}

impl DatabaseConfig {
    // These builders are not sugar. `DatabaseConfig` is `#[non_exhaustive]` so that adding a
    // field later is not a breaking change — which also means no code outside this crate can
    // write a struct literal for it. Without builders the type would be unconstructible except
    // through `Default`, so they are part of the API, not a convenience on top of it.

    /// Create the database if the storage holds none.
    #[must_use]
    pub fn create_if_missing(mut self, yes: bool) -> Self {
        self.create_if_missing = yes;
        self
    }

    /// Open without the write lock, refusing every mutation.
    ///
    /// Also clears `create_if_missing`, since creating requires writing and the two together
    /// are a contradiction the caller almost certainly did not intend.
    #[must_use]
    pub fn read_only(mut self, yes: bool) -> Self {
        self.read_only = yes;
        if yes {
            self.create_if_missing = false;
        }
        self
    }

    /// Set the durability policy.
    #[must_use]
    pub fn durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    /// Set the memtable size at which a flush is triggered.
    #[must_use]
    pub fn flush_threshold_bytes(mut self, bytes: usize) -> Self {
        self.flush_threshold_bytes = bytes;
        self
    }

    /// Check the configuration.
    ///
    /// # Errors
    /// [`ConfigError::InvalidField`] naming the field and the constraint it violated.
    pub fn validate(&self) -> Result<()> {
        if self.flush_threshold_bytes == 0 {
            return Err(ConfigError::InvalidField {
                field: "flush_threshold_bytes",
                value: "0".to_owned(),
                constraint: "at least 1",
            }
            .into());
        }
        if self.read_only && self.create_if_missing {
            // Creating requires writing. Rather than silently ignoring one of the two, say so:
            // a caller who set both has a mistaken expectation about what will happen.
            return Err(ConfigError::InvalidField {
                field: "create_if_missing",
                value: "true".to_owned(),
                constraint: "false when read_only is set, since creating requires writing",
            }
            .into());
        }
        Ok(())
    }
}

/// What a collection is, fixed at creation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CollectionSpec {
    /// The collection's name. `[A-Za-z0-9_-]`, at most 64 bytes.
    pub name: String,
    /// Vector dimension. Immutable once the collection exists.
    pub dimension: u32,
    /// Similarity metric. Immutable once the collection exists.
    pub metric: Metric,
    /// Component type. Only `F32` in v1.
    pub dtype: VectorDType,
    /// Document id representation.
    pub id_kind: IdKind,
    /// Index configuration.
    pub index: IndexSpec,
}

impl CollectionSpec {
    /// A collection with the usual defaults: string ids, `f32` vectors, a flat index.
    pub fn new(name: impl Into<String>, dimension: u32, metric: Metric) -> Self {
        Self {
            name: name.into(),
            dimension,
            metric,
            dtype: VectorDType::F32,
            id_kind: IdKind::Str {
                max_len: limits::MAX_DOC_ID_LEN as u32,
            },
            index: IndexSpec::Flat,
        }
    }

    /// Use integer ids, which cost less per document in the in-memory id map.
    #[must_use]
    pub fn with_u64_ids(mut self) -> Self {
        self.id_kind = IdKind::U64;
        self
    }

    /// Cap the length of string ids, below the engine's own limit.
    #[must_use]
    pub fn with_id_max_len(mut self, max_len: u32) -> Self {
        self.id_kind = IdKind::Str { max_len };
        self
    }

    /// Choose the index.
    #[must_use]
    pub fn with_index(mut self, index: IndexSpec) -> Self {
        self.index = index;
        self
    }

    /// Check the specification.
    ///
    /// # Errors
    /// [`crate::error::ValidationError`] for a bad name or dimension.
    pub fn validate(&self) -> Result<()> {
        validation::check_collection_name(&self.name)?;
        vector::check_dimension(self.dimension)
    }
}
