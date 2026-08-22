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
use jni::sys::{jboolean, jfloat, jint, jlong, jsize, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

use vdb_core::api::{
    Collection, CollectionSpec, Database, DatabaseConfig, SearchRequest, SearchResponse,
    UpsertOutcome,
};
use vdb_core::clock::Clock;
use vdb_core::document::{DocId, DocumentInput, Include};
use vdb_core::persistence::Durability;
use vdb_core::vector::VectorView;
use vdb_core::{DbError, Metric};
use vdb_storage_os::OsStorage;

/// The exception class the Java layer defines.
const EXCEPTION: &str = "dev/vdb/VdbException";

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
