//! The JNI shim.
//!
//! Deliberately thin: it converts Java types to Rust ones, calls the engine, and turns failures
//! into Java exceptions. No decisions are made here. Anything that looks like policy — how to
//! name things, what to default to, when to close — belongs in the Java layer above, where it
//! can be read by the people who will actually use it.
//!
//! # Handles are longs
//!
//! An open database is a `long` on the Java side. That is how JNI bindings do it, and the
//! alternative — a Java object holding native state — means the JVM's garbage collector decides
//! when a database closes, which is to say nobody does. A `long` plus an explicit `close()` is
//! blunt and predictable, and `AutoCloseable` in the Java layer makes it ergonomic.
//!
//! # Errors become exceptions
//!
//! Every failure throws `dev.vdb.VdbException` carrying the stable `VDB-nnnn` code. A JNI method
//! that both throws and returns leaves the exception pending until the JVM checks it, so each
//! entry point returns a harmless default after throwing.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(non_snake_case)] // JNI symbol names are dictated by the Java class they bind to.
#![warn(missing_docs)]

use std::sync::Arc;

use jni::objects::{JClass, JFloatArray, JObjectArray, JString};
use jni::sys::{jboolean, jdouble, jfloat, jint, jlong, jsize, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

use vdb_core::api::{
    Collection, CollectionSpec, Database, DatabaseConfig, SearchRequest, SearchResponse,
    UpsertOutcome,
};
use vdb_core::clock::Clock;
use vdb_core::document::{DocId, DocumentInput, Include};
use vdb_core::metadata::Value;
use vdb_core::persistence::Durability;
use vdb_core::vector::VectorView;
use vdb_core::{DbError, Metric};
use vdb_storage_os::OsStorage;

/// The exception class the Java layer defines.
const EXCEPTION: &str = "dev/vdb/VdbException";

// Discriminants shared with `Native.java`. Kept as bare constants on both sides rather than
// marshalled enums: JNI turns every object argument into a lookup, and these are read on the
// hot path of building a filter.
const VALUE_STRING: jint = 1;
const VALUE_I64: jint = 2;
const VALUE_F64: jint = 3;
const VALUE_BOOL: jint = 4;
const VALUE_NULL: jint = 5;

const OP_EQ: jint = 1;
const OP_NE: jint = 2;
const OP_GT: jint = 3;
const OP_GTE: jint = 4;
const OP_LT: jint = 5;
const OP_LTE: jint = 6;
const OP_STARTS_WITH: jint = 7;
const OP_CONTAINS: jint = 8;

const UNARY_EXISTS: jint = 1;
const UNARY_IS_NULL: jint = 2;

const COMBINE_AND: jint = 1;
const COMBINE_OR: jint = 2;
const COMBINE_NOT: jint = 3;

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

/// Throw a `VdbException` describing an engine failure.
fn throw(env: &mut JNIEnv<'_>, e: &DbError) {
    // If throwing itself fails the JVM is already in trouble; there is nothing useful to do but
    // let the original error be lost, which `let _` records rather than hides.
    let _ = env.throw_new(EXCEPTION, format!("{e}"));
}

/// Throw a `VdbException` describing a problem with the arguments.
fn throw_msg(env: &mut JNIEnv<'_>, message: &str) {
    let _ = env.throw_new(EXCEPTION, message);
}

/// Convert a handle back into a reference.
///
/// # Safety
/// `handle` must be a value previously returned by the matching `into_handle`, and the object
/// it names must not have been freed.
unsafe fn as_ref<'a, T>(handle: jlong) -> Option<&'a T> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the caller guarantees the handle came from `into_handle` and is still live.
    Some(unsafe { &*(handle as *const T) })
}

/// Convert a handle back into a mutable reference.
///
/// # Safety
/// As [`as_ref`], and no other reference to the same object may be outstanding. The Java layer
/// guarantees that: a builder is owned by one thread between `new` and `free`.
unsafe fn as_mut<'a, T>(handle: jlong) -> Option<&'a mut T> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the caller guarantees the handle is live and exclusively held.
    Some(unsafe { &mut *(handle as *mut T) })
}

fn into_handle<T>(value: T) -> jlong {
    Box::into_raw(Box::new(value)) as jlong
}

/// Reclaim and drop a handle.
///
/// # Safety
/// As [`as_ref`], and the handle must not be used again.
unsafe fn drop_handle<T>(handle: jlong) -> Option<T> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the caller guarantees the handle came from `into_handle` and is not reused.
    Some(*unsafe { Box::from_raw(handle as *mut T) })
}

/// Read a Java string.
fn read_string(env: &mut JNIEnv<'_>, value: &JString<'_>) -> Option<String> {
    match env.get_string(value) {
        Ok(s) => Some(s.into()),
        Err(_) => {
            throw_msg(env, "a string argument was null or unreadable");
            None
        }
    }
}

/// Copy a Java float array.
///
/// A copy rather than a critical-section borrow. `GetPrimitiveArrayCritical` would avoid it, but
/// it suspends the garbage collector for the duration, and the duration here includes a write to
/// the log. Pausing GC across an fsync is a worse trade than copying a few kilobytes; the
/// zero-copy path belongs on the read side, where `ByteBuffer` gives it without the pause.
fn read_floats(env: &mut JNIEnv<'_>, array: &JFloatArray<'_>) -> Option<Vec<f32>> {
    let len = match env.get_array_length(array) {
        Ok(n) => n as usize,
        Err(_) => {
            throw_msg(env, "the vector argument was null");
            return None;
        }
    };
    let mut out = vec![0f32; len];
    match env.get_float_array_region(array, 0, &mut out) {
        Ok(()) => Some(out),
        Err(_) => {
            throw_msg(env, "could not read the vector argument");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// database
// ---------------------------------------------------------------------------

/// Open or create a database. Returns a handle, or 0 with an exception pending.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_openDatabase<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
    create_if_missing: jboolean,
    read_only: jboolean,
    durability: jint,
) -> jlong {
    let Some(path) = read_string(&mut env, &path) else {
        return 0;
    };
    let durability = match durability {
        1 => Durability::Full,
        2 => Durability::Batch,
        3 => Durability::Relaxed,
        _ => {
            throw_msg(
                &mut env,
                "unknown durability; expected FULL, BATCH or RELAXED",
            );
            return 0;
        }
    };
    let read_only = read_only == JNI_TRUE;
    let config = DatabaseConfig::default()
        .read_only(read_only)
        .create_if_missing(create_if_missing == JNI_TRUE && !read_only)
        .durability(durability);

    let storage = match OsStorage::open(&path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            throw(&mut env, &e);
            return 0;
        }
    };
    match Database::open_with_index(
        storage,
        config,
        Arc::new(SystemClock),
        Arc::new(vdb_index_flat::FlatIndex::new()),
    ) {
        Ok(db) => into_handle(db),
        Err(e) => {
            throw(&mut env, &e);
            0
        }
    }
}

/// Flush and close. The handle is invalid afterwards either way.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_closeDatabase<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    // SAFETY: the Java layer guarantees the handle came from `openDatabase` and is used once.
    if let Some(db) = unsafe { drop_handle::<Database>(handle) } {
        if let Err(e) = db.close() {
            throw(&mut env, &e);
        }
    }
}

/// Fold every collection's buffered writes into segments.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_flushDatabase<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(db) = (unsafe { as_ref::<Database>(handle) }) else {
        throw_msg(&mut env, "the database is closed");
        return;
    };
    if let Err(e) = db.flush() {
        throw(&mut env, &e);
    }
}

/// Every collection's name, sorted.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_listCollections<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObjectArray<'local> {
    let empty = env
        .new_object_array(0, "java/lang/String", jni::objects::JObject::null())
        .unwrap_or_default();
    // SAFETY: the Java layer guarantees a live handle.
    let Some(db) = (unsafe { as_ref::<Database>(handle) }) else {
        throw_msg(&mut env, "the database is closed");
        return empty;
    };
    let names = match db.list_collections() {
        Ok(n) => n,
        Err(e) => {
            throw(&mut env, &e);
            return empty;
        }
    };
    let Ok(array) = env.new_object_array(
        names.len() as jsize,
        "java/lang/String",
        jni::objects::JObject::null(),
    ) else {
        throw_msg(&mut env, "could not allocate the result array");
        return empty;
    };
    for (i, info) in names.iter().enumerate() {
        let Ok(name) = env.new_string(&info.name) else {
            continue;
        };
        let _ = env.set_object_array_element(&array, i as jsize, name);
    }
    array
}

// ---------------------------------------------------------------------------
// collections
// ---------------------------------------------------------------------------

/// Create or open a collection. Returns a handle, or 0 with an exception pending.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_openCollection<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    db_handle: jlong,
    name: JString<'local>,
    dimension: jint,
    metric: jint,
    create: jboolean,
) -> jlong {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(db) = (unsafe { as_ref::<Database>(db_handle) }) else {
        throw_msg(&mut env, "the database is closed");
        return 0;
    };
    let Some(name) = read_string(&mut env, &name) else {
        return 0;
    };

    let result = if create == JNI_TRUE {
        let metric = match metric {
            1 => Metric::Cosine,
            2 => Metric::L2,
            3 => Metric::Dot,
            _ => {
                throw_msg(&mut env, "unknown metric; expected COSINE, L2 or DOT");
                return 0;
            }
        };
        if dimension <= 0 {
            throw_msg(&mut env, "dimension must be positive");
            return 0;
        }
        db.get_or_create_collection(CollectionSpec::new(name, dimension as u32, metric))
    } else {
        db.open_collection(&name)
    };
    match result {
        Ok(c) => into_handle(c),
        Err(e) => {
            throw(&mut env, &e);
            0
        }
    }
}

/// Release a collection handle. The collection itself is unaffected.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_freeCollection<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    // SAFETY: the Java layer guarantees the handle came from `openCollection` and is used once.
    drop(unsafe { drop_handle::<Collection>(handle) });
}

/// Delete a collection and everything in it.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_dropCollection<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    db_handle: jlong,
    name: JString<'local>,
) {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(db) = (unsafe { as_ref::<Database>(db_handle) }) else {
        throw_msg(&mut env, "the database is closed");
        return;
    };
    let Some(name) = read_string(&mut env, &name) else {
        return;
    };
    if let Err(e) = db.drop_collection(&name) {
        throw(&mut env, &e);
    }
}

/// Insert or replace. Returns true when the document was new.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_upsert<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    id: JString<'local>,
    vector: JFloatArray<'local>,
) -> jboolean {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(c) = (unsafe { as_ref::<Collection>(handle) }) else {
        throw_msg(&mut env, "the collection is closed");
        return JNI_FALSE;
    };
    let Some(id) = read_string(&mut env, &id) else {
        return JNI_FALSE;
    };
    let Some(values) = read_floats(&mut env, &vector) else {
        return JNI_FALSE;
    };

    match c.upsert(DocumentInput::new(id, VectorView::f32(&values))) {
        Ok(UpsertOutcome::Inserted) => JNI_TRUE,
        Ok(UpsertOutcome::Updated) => JNI_FALSE,
        Err(e) => {
            throw(&mut env, &e);
            JNI_FALSE
        }
    }
}

/// Remove a document. Returns whether it existed.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_delete<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    id: JString<'local>,
) -> jboolean {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(c) = (unsafe { as_ref::<Collection>(handle) }) else {
        throw_msg(&mut env, "the collection is closed");
        return JNI_FALSE;
    };
    let Some(id) = read_string(&mut env, &id) else {
        return JNI_FALSE;
    };
    match c.delete(id) {
        Ok(true) => JNI_TRUE,
        Ok(false) => JNI_FALSE,
        Err(e) => {
            throw(&mut env, &e);
            JNI_FALSE
        }
    }
}

/// Whether a document exists.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_contains<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    id: JString<'local>,
) -> jboolean {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(c) = (unsafe { as_ref::<Collection>(handle) }) else {
        throw_msg(&mut env, "the collection is closed");
        return JNI_FALSE;
    };
    let Some(id) = read_string(&mut env, &id) else {
        return JNI_FALSE;
    };
    match c.contains(&DocId::from(id)) {
        Ok(true) => JNI_TRUE,
        Ok(false) => JNI_FALSE,
        Err(e) => {
            throw(&mut env, &e);
            JNI_FALSE
        }
    }
}

/// Live documents, or -1 with an exception pending.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_count<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(c) = (unsafe { as_ref::<Collection>(handle) }) else {
        throw_msg(&mut env, "the collection is closed");
        return -1;
    };
    match c.count() {
        Ok(n) => n as jlong,
        Err(e) => {
            throw(&mut env, &e);
            -1
        }
    }
}

/// Fold buffered writes into a segment.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_flushCollection<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(c) = (unsafe { as_ref::<Collection>(handle) }) else {
        throw_msg(&mut env, "the collection is closed");
        return;
    };
    if let Err(e) = c.flush() {
        throw(&mut env, &e);
    }
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// Run a search, returning a result handle. 0 with an exception pending on failure.
///
/// A handle rather than a Java object array, because building `Hit` objects from Rust means
/// resolving a class, a constructor and four field types across JNI — slow, and a second place
/// for the Java shape to drift from the Rust one. The Java layer builds its own objects from
/// two cheap accessor calls per hit.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_search<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    query: JFloatArray<'local>,
    top_k: jint,
) -> jlong {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(c) = (unsafe { as_ref::<Collection>(handle) }) else {
        throw_msg(&mut env, "the collection is closed");
        return 0;
    };
    let Some(values) = read_floats(&mut env, &query) else {
        return 0;
    };
    if top_k <= 0 {
        throw_msg(&mut env, "topK must be positive");
        return 0;
    }

    let request =
        SearchRequest::new(VectorView::f32(&values), top_k as usize).with_include(Include::NONE);
    match c.search(&request) {
        Ok(response) => into_handle(response),
        Err(e) => {
            throw(&mut env, &e);
            0
        }
    }
}

/// Hits held. 0 for a null handle.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_resultCount<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jint {
    // SAFETY: the Java layer guarantees a live handle.
    match unsafe { as_ref::<SearchResponse>(handle) } {
        Some(r) => r.hits.len() as jint,
        None => 0,
    }
}

/// A hit's id, or null out of range.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_resultId<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    index: jint,
) -> JString<'local> {
    // SAFETY: the Java layer guarantees a live handle.
    let hit =
        unsafe { as_ref::<SearchResponse>(handle) }.and_then(|r| r.hits.get(index.max(0) as usize));
    match hit {
        Some(h) => env.new_string(h.id.display()).unwrap_or_default(),
        None => JString::default(),
    }
}

/// A hit's score. Always higher-is-better. 0 out of range.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_resultScore<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    index: jint,
) -> jfloat {
    // SAFETY: the Java layer guarantees a live handle.
    unsafe { as_ref::<SearchResponse>(handle) }
        .and_then(|r| r.hits.get(index.max(0) as usize))
        .map_or(0.0, |h| h.score)
}

/// Release a search result.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_freeResult<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    // SAFETY: the Java layer guarantees the handle came from `search` and is used once.
    drop(unsafe { drop_handle::<SearchResponse>(handle) });
}

// ---------------------------------------------------------------------------
// metadata
// ---------------------------------------------------------------------------

/// Create a metadata builder. Returns a handle.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_metadataNew<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jlong {
    into_handle(vdb_core::metadata::Metadata::new())
}

/// Release a metadata builder.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_metadataFree<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    // SAFETY: the Java layer guarantees the handle came from `metadataNew` and is used once.
    drop(unsafe { drop_handle::<vdb_core::metadata::Metadata>(handle) });
}

/// Set one metadata field.
///
/// The value is whichever of the four is selected by `kind`, matching `Native.VALUE_*`. One
/// function rather than four because JNI declarations are verbose on both sides and a tag is
/// cheaper to read than four near-identical signatures.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_metadataSet<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    key: JString<'local>,
    kind: jint,
    text: JString<'local>,
    number: jlong,
    real: jdouble,
    flag: jboolean,
) {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(metadata) = (unsafe { as_mut::<vdb_core::metadata::Metadata>(handle) }) else {
        throw_msg(&mut env, "the metadata builder is closed");
        return;
    };
    let Some(key) = read_string(&mut env, &key) else {
        return;
    };
    let value = match kind {
        VALUE_STRING => match read_string(&mut env, &text) {
            Some(s) => Value::Str(s),
            None => return,
        },
        VALUE_I64 => Value::I64(number),
        VALUE_F64 => Value::F64(real),
        VALUE_BOOL => Value::Bool(flag == JNI_TRUE),
        VALUE_NULL => Value::Null,
        _ => {
            throw_msg(&mut env, "unknown metadata value kind");
            return;
        }
    };
    metadata.insert(key, value);
}

/// Insert or replace a document with metadata. Returns true when it was new.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_upsertWithMetadata<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    id: JString<'local>,
    vector: JFloatArray<'local>,
    metadata: jlong,
) -> jboolean {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(c) = (unsafe { as_ref::<Collection>(handle) }) else {
        throw_msg(&mut env, "the collection is closed");
        return JNI_FALSE;
    };
    let Some(id) = read_string(&mut env, &id) else {
        return JNI_FALSE;
    };
    let Some(values) = read_floats(&mut env, &vector) else {
        return JNI_FALSE;
    };

    let mut input = DocumentInput::new(id, VectorView::f32(&values));
    if metadata != 0 {
        // SAFETY: a non-zero handle came from `metadataNew` and is still live.
        let Some(m) = (unsafe { as_ref::<vdb_core::metadata::Metadata>(metadata) }) else {
            throw_msg(&mut env, "the metadata builder is closed");
            return JNI_FALSE;
        };
        input = input.with_metadata(m.clone());
    }
    match c.upsert(input) {
        Ok(UpsertOutcome::Inserted) => JNI_TRUE,
        Ok(UpsertOutcome::Updated) => JNI_FALSE,
        Err(e) => {
            throw(&mut env, &e);
            JNI_FALSE
        }
    }
}

// ---------------------------------------------------------------------------
// filters
// ---------------------------------------------------------------------------

/// A filter under construction: a stack, exactly as the C ABI builds one.
///
/// The Java layer never sees the stack — its `Filter` is a tree that flattens itself by a
/// post-order walk — but the JNI surface stays postfix for the same reason the C one does: a
/// tree cannot cross this boundary without either reflection on every node or a great many
/// declarations.
#[derive(Debug, Default)]
struct FilterStack {
    stack: Vec<vdb_core::filter::Filter>,
}

/// Create a filter builder.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_filterNew<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jlong {
    into_handle(FilterStack::default())
}

/// Release a filter builder.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_filterFree<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    // SAFETY: the Java layer guarantees the handle came from `filterNew` and is used once.
    drop(unsafe { drop_handle::<FilterStack>(handle) });
}

/// Push a comparison. `kind` selects which of the value arguments is meaningful.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_filterCompare<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    field: JString<'local>,
    op: jint,
    kind: jint,
    text: JString<'local>,
    number: jlong,
    real: jdouble,
    flag: jboolean,
) {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(builder) = (unsafe { as_mut::<FilterStack>(handle) }) else {
        throw_msg(&mut env, "the filter builder is closed");
        return;
    };
    let Some(field) = read_string(&mut env, &field) else {
        return;
    };
    let value = match kind {
        VALUE_STRING => match read_string(&mut env, &text) {
            Some(s) => Value::Str(s),
            None => return,
        },
        VALUE_I64 => Value::I64(number),
        VALUE_F64 => Value::F64(real),
        VALUE_BOOL => Value::Bool(flag == JNI_TRUE),
        VALUE_NULL => Value::Null,
        _ => {
            throw_msg(&mut env, "unknown filter value kind");
            return;
        }
    };

    use vdb_core::filter::Filter as F;
    let built = match op {
        OP_EQ => F::eq(field, value),
        OP_NE => F::ne(field, value),
        OP_GT => F::gt(field, value),
        OP_GTE => F::gte(field, value),
        OP_LT => F::lt(field, value),
        OP_LTE => F::lte(field, value),
        OP_CONTAINS => F::contains(field, value),
        OP_STARTS_WITH => match value {
            Value::Str(prefix) => F::starts_with(field, prefix),
            // A prefix test on a number is a mistake rather than a coercion question, and
            // saying so beats matching nothing forever.
            _ => {
                throw_msg(&mut env, "startsWith takes a string");
                return;
            }
        },
        _ => {
            throw_msg(&mut env, "unknown filter operator");
            return;
        }
    };
    builder.stack.push(built);
}

/// Push a unary predicate: exists or isNull.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_filterUnary<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    field: JString<'local>,
    predicate: jint,
) {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(builder) = (unsafe { as_mut::<FilterStack>(handle) }) else {
        throw_msg(&mut env, "the filter builder is closed");
        return;
    };
    let Some(field) = read_string(&mut env, &field) else {
        return;
    };
    use vdb_core::filter::Filter as F;
    let built = match predicate {
        UNARY_EXISTS => F::exists(field),
        UNARY_IS_NULL => F::is_null(field),
        _ => {
            throw_msg(&mut env, "unknown unary predicate");
            return;
        }
    };
    builder.stack.push(built);
}

/// Pop `count` expressions and push their combination.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_filterCombine<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    combinator: jint,
    count: jint,
) {
    // SAFETY: the Java layer guarantees a live handle.
    let Some(builder) = (unsafe { as_mut::<FilterStack>(handle) }) else {
        throw_msg(&mut env, "the filter builder is closed");
        return;
    };
    let count = count.max(0) as usize;
    if count == 0 || count > builder.stack.len() {
        throw_msg(&mut env, "cannot combine more expressions than were pushed");
        return;
    }
    if combinator == COMBINE_NOT && count != 1 {
        throw_msg(&mut env, "not takes exactly one operand");
        return;
    }
    let operands: Vec<vdb_core::filter::Filter> =
        builder.stack.split_off(builder.stack.len() - count);
    use vdb_core::filter::Filter as F;
    let combined = match combinator {
        COMBINE_AND => F::all(operands),
        COMBINE_OR => F::any(operands),
        COMBINE_NOT => match operands.into_iter().next() {
            Some(only) => F::negate(only),
            None => {
                throw_msg(&mut env, "not takes exactly one operand");
                return;
            }
        },
        _ => {
            throw_msg(&mut env, "unknown combinator");
            return;
        }
    };
    builder.stack.push(combined);
}

/// Expressions on the builder's stack. A complete filter has exactly one.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_filterDepth<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jint {
    // SAFETY: the Java layer guarantees a live handle.
    match unsafe { as_ref::<FilterStack>(handle) } {
        Some(b) => b.stack.len() as jint,
        None => 0,
    }
}

/// Search, restricted to documents matching a filter. Returns a result handle.
#[no_mangle]
pub extern "system" fn Java_dev_vdb_Native_searchFiltered<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    query: JFloatArray<'local>,
    top_k: jint,
    filter: jlong,
) -> jlong {
    // SAFETY: the Java layer guarantees live handles.
    let Some(c) = (unsafe { as_ref::<Collection>(handle) }) else {
        throw_msg(&mut env, "the collection is closed");
        return 0;
    };
    let Some(builder) = (unsafe { as_ref::<FilterStack>(filter) }) else {
        throw_msg(&mut env, "the filter builder is closed");
        return 0;
    };
    let built = match builder.stack.as_slice() {
        [only] => only,
        // An unbalanced builder is refused rather than interpreted generously: a filter missing
        // a clause returns documents the caller asked to exclude, and says nothing about it.
        _ => {
            throw_msg(
                &mut env,
                "the filter is incomplete; check filterDepth() is 1",
            );
            return 0;
        }
    };
    let Some(values) = read_floats(&mut env, &query) else {
        return 0;
    };
    if top_k <= 0 {
        throw_msg(&mut env, "topK must be positive");
        return 0;
    }

    let request = SearchRequest::new(VectorView::f32(&values), top_k as usize)
        .with_filter(built)
        .with_include(Include::NONE);
    match c.search(&request) {
        Ok(response) => into_handle(response),
        Err(e) => {
            throw(&mut env, &e);
            0
        }
    }
}
