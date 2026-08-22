/*
 * vdb — an embedded, offline-first vector database.
 *
 * This header is the contract. Every native SDK — React Native, Flutter, Android, iOS — is
 * built on exactly these declarations, so a change here is a change to all of them.
 *
 * FROZEN AT 0.2, ADDITIVE-ONLY AFTERWARDS. Call vdb_abi_version() at load and refuse a
 * mismatch: an application that ships a prebuilt library and an SDK built at a different time
 * should get a clear error, not a crash.
 *
 * RULES THAT APPLY THROUGHOUT
 *
 *   Return values. Every fallible function returns int32_t: 0 (VDB_OK) on success, otherwise a
 *   code. Negative codes are boundary failures (a null pointer, bad UTF-8); positive codes come
 *   from the engine and are documented in docs/api/error-codes.md.
 *
 *   Errors. Pass a vdb_error_t** to receive detail, or NULL if you do not want it. A non-NULL
 *   slot is only written on failure, and what it receives must be released with
 *   vdb_error_free(). There is no "last error" global: it would belong to whichever thread of
 *   your pool happened to run the call.
 *
 *   Strings. Pointer plus length, never NUL-terminated. Lengths are in bytes and strings are
 *   UTF-8. This avoids a strlen on every call, and means an id containing a NUL is rejected by
 *   validation rather than silently truncated.
 *
 *   Vectors. const float* plus a dimension. Nothing is copied at the boundary; the engine copies
 *   once, into its log. Pass your Float32Array, Float32List or FloatBuffer straight through.
 *
 *   Ownership. Anything this library allocates has an explicit free, named in the function that
 *   produced it. Every free accepts NULL as a no-op.
 *
 *   Threading. Every call is synchronous and blocking. The library spawns no threads and owns no
 *   runtime; run these on whatever executor your platform prefers. One writer at a time — a
 *   second vdb_open on the same directory fails while the first is live.
 *
 *   Panics. None cross this boundary. An internal fault becomes VDB_INTERNAL, which is a bug
 *   worth reporting rather than a crash in your process.
 */

#ifndef VDB_H
#define VDB_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- status codes ------------------------------------------------------- */

#define VDB_OK 0
#define VDB_NULL_POINTER (-1)
#define VDB_INTERNAL (-2)
#define VDB_INVALID_UTF8 (-3)
#define VDB_INVALID_ARGUMENT (-4)

/* ---- enumerations ------------------------------------------------------- */

typedef enum {
  VDB_METRIC_COSINE = 1,
  VDB_METRIC_L2 = 2,
  VDB_METRIC_DOT = 3
} vdb_metric_t;

/*
 * How aggressively writes are made durable.
 *
 * In every mode a process crash loses nothing: the bytes are already in the operating system's
 * page cache. Only power loss or a kernel panic can lose an unsynced write. On a phone, process
 * death is routine and power loss is rare, which is why BATCH is the sensible default rather
 * than FULL.
 */
typedef enum {
  VDB_DURABILITY_FULL = 1,    /* sync every write; safe against power loss, slow on flash */
  VDB_DURABILITY_BATCH = 2,   /* sync on batch commit, flush and close — the default */
  VDB_DURABILITY_RELAXED = 3  /* sync on flush and close only; for bulk import */
} vdb_durability_t;

/* ---- opaque handles ----------------------------------------------------- */

typedef struct vdb_db vdb_db_t;
typedef struct vdb_collection vdb_collection_t;
typedef struct vdb_metadata vdb_metadata_t;
typedef struct vdb_results vdb_results_t;
typedef struct vdb_error vdb_error_t;

/* ---- version ------------------------------------------------------------ */

/* Library version, NUL-terminated, owned by the library. */
const char *vdb_version(void);

/* ABI revision. Check at load; refuse a mismatch. */
uint32_t vdb_abi_version(void);

/* On-disk format version this library writes. */
uint32_t vdb_format_version(void);

/* ---- errors ------------------------------------------------------------- */

/* Stable numeric code. See docs/api/error-codes.md. Returns 0 for NULL. */
uint32_t vdb_error_code(const vdb_error_t *error);

/* Description, NUL-terminated, valid until vdb_error_free(). NULL for NULL. */
const char *vdb_error_message(const vdb_error_t *error);

/* Release an error. NULL is a no-op. */
void vdb_error_free(vdb_error_t *error);

/* ---- database ----------------------------------------------------------- */

/*
 * Open or create a database at a directory path.
 *
 * A read-only handle takes no lock, so it can inspect a database another process has open.
 * create_if_missing is ignored when read_only is set, since creating requires writing.
 *
 * Release with vdb_close().
 */
int32_t vdb_open(const uint8_t *path, size_t path_len, bool create_if_missing, bool read_only,
                 int32_t durability, vdb_db_t **out_db, vdb_error_t **err);

/* Flush and close, releasing the lock. The handle is invalid afterwards either way. */
int32_t vdb_close(vdb_db_t *db, vdb_error_t **err);

/* Fold every collection's buffered writes into segments. */
int32_t vdb_flush(const vdb_db_t *db, vdb_error_t **err);

/* ---- collections -------------------------------------------------------- */

/*
 * Create a collection, or open it if one already exists with a matching specification.
 *
 * A mismatch — different dimension, metric or id kind — is an error rather than a silent
 * substitution, because a differently-shaped collection returns results that look plausible and
 * are wrong.
 *
 * Release the handle with vdb_collection_free(); the collection itself is unaffected.
 */
int32_t vdb_collection_create(const vdb_db_t *db, const uint8_t *name, size_t name_len,
                              uint32_t dimension, int32_t metric, bool u64_ids,
                              vdb_collection_t **out, vdb_error_t **err);

/* Open an existing collection. */
int32_t vdb_collection_open(const vdb_db_t *db, const uint8_t *name, size_t name_len,
                            vdb_collection_t **out, vdb_error_t **err);

/* Delete a collection and everything in it. Irreversible. */
int32_t vdb_collection_drop(const vdb_db_t *db, const uint8_t *name, size_t name_len,
                            vdb_error_t **err);

/* Release a collection handle. NULL is a no-op. */
void vdb_collection_free(vdb_collection_t *collection);

/* Live documents. */
int32_t vdb_collection_count(const vdb_collection_t *collection, uint64_t *out,
                             vdb_error_t **err);

/* ---- documents ---------------------------------------------------------- */

/*
 * Insert or replace a document.
 *
 * id is interpreted according to the collection's id kind: UTF-8 for string ids, eight
 * little-endian bytes for integer ids. metadata may be NULL. out_inserted may be NULL; when it
 * is not, it receives whether the document was new rather than replaced.
 *
 * Nothing passed in is retained after the call returns.
 */
int32_t vdb_upsert(const vdb_collection_t *collection, const uint8_t *id, size_t id_len,
                   const float *vector, uint32_t dimension, const vdb_metadata_t *metadata,
                   bool *out_inserted, vdb_error_t **err);

/* Remove a document. Deleting one that is absent succeeds and reports false. */
int32_t vdb_delete(const vdb_collection_t *collection, const uint8_t *id, size_t id_len,
                   bool *out_existed, vdb_error_t **err);

/* Whether a document exists. */
int32_t vdb_contains(const vdb_collection_t *collection, const uint8_t *id, size_t id_len,
                     bool *out, vdb_error_t **err);

/* ---- search ------------------------------------------------------------- */

/*
 * Find the top_k nearest documents.
 *
 * Results are ordered by score descending, ties broken by ascending id. Release with
 * vdb_results_free().
 */
int32_t vdb_search(const vdb_collection_t *collection, const float *query, uint32_t dimension,
                   size_t top_k, vdb_results_t **out, vdb_error_t **err);

/* Hits held. 0 for NULL. */
size_t vdb_results_len(const vdb_results_t *results);

/* A hit's score. ALWAYS higher-is-better, whatever the metric. 0 out of range. */
float vdb_results_score(const vdb_results_t *results, size_t index);

/*
 * A hit's id, borrowed from the result and valid until vdb_results_free().
 *
 * Interpretation follows the collection's id kind. Returns NULL out of range.
 */
const uint8_t *vdb_results_id(const vdb_results_t *results, size_t index, size_t *out_len);

/* Release a search result. NULL is a no-op. */
void vdb_results_free(vdb_results_t *results);

/* ---- metadata ----------------------------------------------------------- */

/*
 * Build a flat metadata map to attach to a document.
 *
 * Flat only in this revision: nested maps and arrays, and the filter expression tree, need a
 * richer encoding than a handful of setters can express, and will arrive as an encoded payload
 * rather than as twenty more functions.
 */
vdb_metadata_t *vdb_metadata_new(void);
void vdb_metadata_free(vdb_metadata_t *metadata);

int32_t vdb_metadata_set_string(vdb_metadata_t *metadata, const uint8_t *key, size_t key_len,
                                const uint8_t *value, size_t value_len, vdb_error_t **err);
int32_t vdb_metadata_set_i64(vdb_metadata_t *metadata, const uint8_t *key, size_t key_len,
                             int64_t value, vdb_error_t **err);
int32_t vdb_metadata_set_f64(vdb_metadata_t *metadata, const uint8_t *key, size_t key_len,
                             double value, vdb_error_t **err);
int32_t vdb_metadata_set_bool(vdb_metadata_t *metadata, const uint8_t *key, size_t key_len,
                              bool value, vdb_error_t **err);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* VDB_H */
