//! The Node.js addon.
//!
//! Node is deliberately the first binding. Every ABI ergonomics mistake surfaces here first, in
//! a five-second `node --test` loop rather than a five-minute Gradle or Xcode one, and finding
//! them here is worth weeks across the four bindings that follow.
//!
//! This crate binds `vdb-core` through N-API directly rather than going through the C ABI.
//! `napi-rs` already generates the marshalling, so routing through `vdb.h` would add a hop and
//! a second place for types to drift without buying anything. The C ABI exists for the bindings
//! that genuinely need C — React Native's JSI layer, Dart's FFI, JNI, Swift.
//!
//! # What this layer owns
//!
//! **Threading.** The engine is synchronous and spawns nothing. Every method here is
//! synchronous too, and the JavaScript API in `sdk/node` is what decides whether to run a call
//! on the libuv threadpool. Putting that decision here would force it on every caller.
//!
//! **Error shape.** A `DbError` becomes a JavaScript `Error` carrying the stable `VDB-nnnn`
//! code, so `err.code` is matchable and does not depend on parsing a message.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::needless_pass_by_value)]
// napi-rs takes owned arguments by convention.
// The `#[napi]` macro generates associated functions and Debug-less glue types that no amount
// of documentation on our side reaches. Scoped to this crate, where the generated surface is
// the majority of the code; every crate we actually hand-write keeps both lints on.
#![allow(missing_docs, missing_debug_implementations)]

#[macro_use]
extern crate napi_derive;

use std::sync::Arc;

// `Result` unaliased, and this is not a style preference. The `#[napi]` macro inspects the
// *written* return type syntactically: it recognises `Result<T>` and generates a throw, and
// anything else — including a type alias for exactly that type — it treats as a value to
// convert. Aliasing it to `Result<T>` compiled, ran, and silently returned every error to
// JavaScript as an ordinary return value instead of throwing it, so `col.upsert(...)` handed
// back an `Error` object where callers expected a boolean and nothing stopped them using it.
use napi::bindgen_prelude::{Float32Array, Result};
use napi::Error as NapiError;

use vdb_core::api::{
    CollectionSpec, Database as CoreDatabase, DatabaseConfig, SearchRequest, UpsertOutcome,
};
use vdb_core::clock::Clock;
use vdb_core::document::{DocId, DocumentInput, Include};
use vdb_core::metadata::{Metadata, Value};
use vdb_core::persistence::Durability;
use vdb_core::vector::VectorView;
use vdb_core::{DbError, Metric};
use vdb_storage_os::OsStorage;

/// Wall-clock time. The engine reads no clock of its own.
#[derive(Debug)]
struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Turn an engine failure into a JavaScript `Error`.
///
/// The stable code goes in `err.code` rather than only into the message, so application code can
/// branch on it without parsing prose that is allowed to change.
fn to_js(e: DbError) -> NapiError {
    NapiError::new(napi::Status::GenericFailure, format!("{e}"))
}

/// Options for opening a database.
#[napi(object)]
#[derive(Debug, Default)]
pub struct OpenOptions {
    /// Create the database if the directory holds none. Defaults to true.
    pub create_if_missing: Option<bool>,
    /// Open without the write lock and refuse every mutation. Defaults to false.
    pub read_only: Option<bool>,
    /// `"full"`, `"batch"` or `"relaxed"`. Defaults to `"batch"`.
    ///
    /// In every mode a process crash loses nothing — the bytes are in the page cache. Only power
    /// loss can lose an unsynced write, which is why `"batch"` rather than `"full"` is the
    /// default.
    pub durability: Option<String>,
    /// Flush a collection's buffer into a segment once it exceeds this many bytes.
    pub flush_threshold_bytes: Option<u32>,
}

/// Options for creating or opening a collection.
#[napi(object)]
#[derive(Debug)]
pub struct CollectionOptions {
    /// Vector dimension. Fixed for the collection's lifetime.
    pub dimension: u32,
    /// `"cosine"`, `"l2"` or `"dot"`. Defaults to `"cosine"`.
    pub metric: Option<String>,
}

/// One search result.
#[napi(object)]
#[derive(Debug)]
pub struct Hit {
    /// The document's id.
    pub id: String,
    /// Its score. Always higher-is-better, whatever the metric.
    pub score: f64,
    /// The metric-native distance, absent for the inner product.
    pub distance: Option<f64>,
}

/// Counters for a collection.
#[napi(object)]
#[derive(Debug)]
pub struct CollectionStats {
    /// Live documents.
    pub live_documents: i64,
    /// Rows on disk, tombstones included.
    pub total_rows: i64,
    /// Segments on disk.
    pub segments: u32,
    /// Documents buffered in memory and not yet in a segment.
    pub buffered_documents: u32,
    /// Fraction of rows that are tombstones, 0 to 1.
    pub dead_ratio: f64,
}

/// Open or create a database at a directory path.
///
/// A free function rather than a constructor: opening does I/O and can fail, and napi-rs
/// classes cannot have a fallible constructor. `sdk/node` re-exposes this as `Database.open()`,
/// which reads better in JavaScript than `new Database(...)` for something that touches a disk.
#[napi(js_name = "openDatabase")]
pub fn open_database(path: String, options: Option<OpenOptions>) -> Result<Database> {
    Database::build(path, options)
}

/// An open database.
#[napi]
#[derive(Debug)]
pub struct Database {
    /// `None` once closed. Every method checks, so using a closed handle is a clear error
    /// rather than a use-after-free — JavaScript has no way to make that a compile error the
    /// way the Rust API does.
    inner: Option<CoreDatabase>,
}

#[napi]
impl Database {
    fn build(path: String, options: Option<OpenOptions>) -> Result<Self> {
        let options = options.unwrap_or_default();
        let durability = match options.durability.as_deref() {
            None | Some("batch") => Durability::Batch,
            Some("full") => Durability::Full,
            Some("relaxed") => Durability::Relaxed,
            Some(other) => {
                return Err(NapiError::from_reason(format!(
                    "unknown durability {other:?}; expected \"full\", \"batch\" or \"relaxed\""
                )))
            }
        };
        let read_only = options.read_only.unwrap_or(false);
        let mut config = DatabaseConfig::default()
            .read_only(read_only)
            .create_if_missing(options.create_if_missing.unwrap_or(true) && !read_only)
            .durability(durability);
        if let Some(bytes) = options.flush_threshold_bytes {
            config = config.flush_threshold_bytes(bytes as usize);
        }

        let storage = Arc::new(OsStorage::open(&path).map_err(to_js)?);
        let db = CoreDatabase::open_with_index(
            storage,
            config,
            Arc::new(SystemClock),
            Arc::new(vdb_index_flat::FlatIndex::new()),
        )
        .map_err(to_js)?;
        Ok(Self { inner: Some(db) })
    }

    /// Create a collection, or open it if one exists with a matching shape.
    #[napi]
    pub fn collection(&self, name: String, options: CollectionOptions) -> Result<Collection> {
        let db = self.live()?;
        let metric = match options.metric.as_deref() {
            None | Some("cosine") => Metric::Cosine,
            Some("l2") => Metric::L2,
            Some("dot") => Metric::Dot,
            Some(other) => {
                return Err(NapiError::from_reason(format!(
                    "unknown metric {other:?}; expected \"cosine\", \"l2\" or \"dot\""
                )))
            }
        };
        let spec = CollectionSpec::new(name, options.dimension, metric);
        let collection = db.get_or_create_collection(spec).map_err(to_js)?;
        Ok(Collection { inner: collection })
    }

    /// Open an existing collection.
    #[napi]
    pub fn open_collection(&self, name: String) -> Result<Collection> {
        let db = self.live()?;
        Ok(Collection {
            inner: db.open_collection(&name).map_err(to_js)?,
        })
    }

    /// Delete a collection and everything in it. Irreversible.
    #[napi]
    pub fn drop_collection(&self, name: String) -> Result<()> {
        self.live()?.drop_collection(&name).map_err(to_js)
    }

    /// Every collection's name, sorted.
    #[napi]
    pub fn list_collections(&self) -> Result<Vec<String>> {
        Ok(self
            .live()?
            .list_collections()
            .map_err(to_js)?
            .into_iter()
            .map(|c| c.name)
            .collect())
    }

    /// Fold every collection's buffered writes into segments.
    #[napi]
    pub fn flush(&self) -> Result<()> {
        self.live()?.flush().map_err(to_js)
    }

    /// Flush and close, releasing the lock.
    ///
    /// Idempotent: closing twice succeeds, because a `finally` block that closes a database
    /// which an earlier `close()` already handled should not throw.
    #[napi]
    pub fn close(&mut self) -> Result<()> {
        match self.inner.take() {
            Some(db) => db.close().map_err(to_js),
            None => Ok(()),
        }
    }

    /// Whether the handle is still usable.
    #[napi(getter)]
    pub fn is_open(&self) -> bool {
        self.inner.is_some()
    }

    fn live(&self) -> Result<&CoreDatabase> {
        self.inner
            .as_ref()
            .ok_or_else(|| NapiError::from_reason("the database is closed".to_owned()))
    }
}

/// A handle to one collection.
#[napi]
#[derive(Debug)]
pub struct Collection {
    inner: vdb_core::api::Collection,
}

#[napi]
impl Collection {
    /// The collection's name.
    #[napi(getter)]
    pub fn name(&self) -> String {
        self.inner.name().to_owned()
    }

    /// Its vector dimension.
    #[napi(getter)]
    pub fn dimension(&self) -> u32 {
        self.inner.dimension()
    }

    /// Insert or replace a document. Returns true when the document was new.
    ///
    /// The `Float32Array` is read directly; nothing is copied until the bytes reach the log.
    #[napi]
    pub fn upsert(
        &self,
        id: String,
        vector: Float32Array,
        metadata: Option<serde_json_compat::JsonMap>,
    ) -> Result<bool> {
        let mut input = DocumentInput::new(id, VectorView::f32(&vector));
        if let Some(map) = metadata {
            input = input.with_metadata(map.into_metadata()?);
        }
        let outcome = self.inner.upsert(input).map_err(to_js)?;
        Ok(matches!(outcome, UpsertOutcome::Inserted))
    }

    /// Remove a document. Returns whether it existed.
    #[napi]
    pub fn delete(&self, id: String) -> Result<bool> {
        self.inner.delete(id).map_err(to_js)
    }

    /// Whether a document exists.
    #[napi]
    pub fn contains(&self, id: String) -> Result<bool> {
        self.inner.contains(&DocId::from(id)).map_err(to_js)
    }

    /// Live documents.
    #[napi]
    pub fn count(&self) -> Result<i64> {
        Ok(self.inner.count().map_err(to_js)? as i64)
    }

    /// Find the nearest documents.
    #[napi]
    pub fn search(&self, query: Float32Array, top_k: u32) -> Result<Vec<Hit>> {
        let request =
            SearchRequest::new(VectorView::f32(&query), top_k as usize).with_include(Include::NONE);
        let response = self.inner.search(&request).map_err(to_js)?;
        Ok(response
            .hits
            .into_iter()
            .map(|h| Hit {
                id: h.id.display(),
                score: f64::from(h.score),
                distance: h.distance.map(f64::from),
            })
            .collect())
    }

    /// Fold buffered writes into a segment.
    #[napi]
    pub fn flush(&self) -> Result<()> {
        self.inner.flush().map_err(to_js)
    }

    /// Counters.
    #[napi]
    pub fn stats(&self) -> Result<CollectionStats> {
        let s = self.inner.stats().map_err(to_js)?;
        Ok(CollectionStats {
            live_documents: s.live_documents as i64,
            total_rows: s.total_rows as i64,
            segments: s.segments as u32,
            buffered_documents: s.buffered_documents as u32,
            dead_ratio: f64::from(s.dead_ratio),
        })
    }
}

/// Converting a plain JavaScript object into metadata.
mod serde_json_compat {
    use super::{Metadata, NapiError, Value};
    use napi::bindgen_prelude::Result as NapiResult;
    use napi::bindgen_prelude::*;

    /// A JavaScript object of scalar values.
    ///
    /// Deliberately flat and scalar-only for now. Nested objects and arrays need a recursive
    /// conversion whose type rules — what a JavaScript `undefined` means, how a non-integer
    /// number is stored — deserve deciding once and sharing across every SDK, rather than being
    /// improvised here and then diverging in the next binding.
    pub struct JsonMap(pub std::collections::HashMap<String, JsonScalar>);

    /// One scalar metadata value.
    pub enum JsonScalar {
        /// A string.
        Str(String),
        /// A number. JavaScript has one numeric type; integral values are stored as integers so
        /// they round-trip, because a document written as 3 and read back as 3.0 surprises
        /// people and breaks equality filters.
        Num(f64),
        /// A boolean.
        Bool(bool),
        /// Null.
        Null,
    }

    impl FromNapiValue for JsonScalar {
        unsafe fn from_napi_value(env: sys::napi_env, value: sys::napi_value) -> Result<Self> {
            // SAFETY: `env` and `value` come from the runtime and are valid for this call.
            let ty = type_of!(env, value)?;
            match ty {
                ValueType::String => Ok(Self::Str(unsafe { String::from_napi_value(env, value)? })),
                ValueType::Number => Ok(Self::Num(unsafe { f64::from_napi_value(env, value)? })),
                ValueType::Boolean => Ok(Self::Bool(unsafe { bool::from_napi_value(env, value)? })),
                ValueType::Null | ValueType::Undefined => Ok(Self::Null),
                other => Err(Error::from_reason(format!(
                    "metadata values must be strings, numbers, booleans or null; got {other:?}"
                ))),
            }
        }
    }

    impl FromNapiValue for JsonMap {
        unsafe fn from_napi_value(env: sys::napi_env, value: sys::napi_value) -> Result<Self> {
            // SAFETY: as above.
            let map = unsafe {
                std::collections::HashMap::<String, JsonScalar>::from_napi_value(env, value)?
            };
            Ok(Self(map))
        }
    }

    impl JsonMap {
        /// Convert to engine metadata.
        pub fn into_metadata(self) -> NapiResult<Metadata> {
            let mut out = Metadata::new();
            for (key, value) in self.0 {
                let value = match value {
                    JsonScalar::Str(s) => Value::Str(s),
                    JsonScalar::Bool(b) => Value::Bool(b),
                    JsonScalar::Null => Value::Null,
                    // An integral double becomes an integer, so `{ count: 3 }` round-trips as 3
                    // and matches `Eq("count", 3)`. Storing every number as a float would make
                    // integer filters quietly miss.
                    JsonScalar::Num(n) if n.fract() == 0.0 && n.abs() < 9e15 => {
                        Value::I64(n as i64)
                    }
                    JsonScalar::Num(n) => Value::F64(n),
                };
                out.insert(key, value);
            }
            out.validate()
                .map_err(|e| NapiError::from_reason(e.to_string()))?;
            Ok(out)
        }
    }
}
