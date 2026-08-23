# isha-vector-db in C and C++

The C ABI is the contract every other binding is built on — Swift, Java, Node, Python, React
Native and the browser all go through this header. It is **frozen**: `vdb_abi_version()` returns
1 and a change to it would break every compiled caller.

## Build the library

```bash
cargo build -p isha-vector-db-ffi --release
```

That produces, in `target/release/`:

| file | for |
|---|---|
| `libisha_vector_db_ffi.a` | static linking, which is what you usually want |
| `libisha_vector_db_ffi.dylib` / `.so` / `.dll` | dynamic linking |

The header is `crates/isha-vector-db-ffi/include/vdb.h`. It is hand-written, not generated, and a test
asserts that every exported symbol appears in it.

## Compile against it

```bash
cc -I crates/isha-vector-db-ffi/include my_app.c target/release/libisha_vector_db_ffi.a -o my_app
```

On Linux add `-lpthread -ldl -lm`. On macOS add `-framework CoreFoundation -framework Security`.

## Your first program

```c
#include <stdio.h>
#include <string.h>
#include "vdb.h"

/* Every call reports failure through an out-parameter, and the caller owns what lands there. */
static int fail(const char *what, vdb_error_t *err) {
    fprintf(stderr, "%s: [%u] %s\n", what, vdb_error_code(err), vdb_error_message(err));
    vdb_error_free(err);
    return 1;
}

int main(void) {
    const char *path = "./my-notes";
    vdb_db_t *db = NULL;
    vdb_error_t *err = NULL;

    if (vdb_open((const uint8_t *)path, strlen(path), true, false,
                 VDB_DURABILITY_BATCH, &db, &err) != 0) {
        return fail("open", err);
    }

    vdb_collection_t *notes = NULL;
    if (vdb_collection_create(db, (const uint8_t *)"notes", 5, 4,
                              VDB_METRIC_COSINE, false, &notes, &err) != 0) {
        return fail("collection", err);
    }

    const float a[4] = {1.0f, 0.0f, 0.0f, 0.0f};
    const float b[4] = {0.9f, 0.1f, 0.0f, 0.0f};
    bool inserted = false;
    vdb_upsert(notes, (const uint8_t *)"note-1", 6, a, 4, NULL, &inserted, &err);
    vdb_upsert(notes, (const uint8_t *)"note-2", 6, b, 4, NULL, &inserted, &err);
    vdb_collection_flush(notes, &err);

    vdb_results_t *results = NULL;
    if (vdb_search(notes, a, 4, 2, &results, &err) != 0) {
        return fail("search", err);
    }
    for (size_t i = 0; i < vdb_results_len(results); i++) {
        size_t len = 0;
        const uint8_t *id = vdb_results_id(results, i, &len);
        printf("%.*s %f\n", (int)len, id, vdb_results_score(results, i));
    }

    vdb_results_free(results);
    vdb_collection_free(notes);
    vdb_close(db, &err);
    return 0;
}
```

A runnable version is `crates/isha-vector-db-ffi/examples/smoke.c`, compiled and executed by
`scripts/check-c-abi.sh`.

## The five rules

**1. Strings are pointer and length, never NUL-terminated.** An id may contain any bytes, and a
length is one less thing to get wrong.

**2. Every fallible call takes a trailing `vdb_error_t **`.** There is no "last error" global: it
would belong to whichever thread of yours happened to touch it last.

**3. You own what the out-parameter gives you.** `vdb_error_free`, `vdb_results_free`,
`vdb_collection_free`, `vdb_close`. Freeing twice is a bug; freeing `NULL` is fine.

**4. A negative return is a boundary rejection, a positive one is the engine.**

```c
#define VDB_NULL_POINTER    (-1)
#define VDB_INTERNAL        (-2)
#define VDB_INVALID_UTF8    (-3)
#define VDB_INVALID_ARGUMENT (-4)
```

A positive code comes with an error object and is in [error-codes.md](../api/error-codes.md).

**5. Ids returned by a search point into the results object.** They are invalid the moment
`vdb_results_free` runs. Copy anything you keep.

## Vectors

`const float *` plus a dimension, read during the call and not retained. There is no copy on the
way in, which is why this is the layer everything else is built on.

## Panics do not cross the boundary

Every entry point catches unwinding and converts it to `VDB_INTERNAL`. A panic reaching a C caller
would be undefined behaviour; the library will not do that to you.

## Thread safety

A `vdb_db_t *` and its collections may be used from several threads. The engine serialises writes
internally. What you may not do is use a handle after closing it, from any thread.

## C++

The header is `extern "C"`-guarded, so it includes directly. For a worked example of wrapping it
in RAII — handle maps, error translation, use-after-close — see
`sdk/react-native/cpp/vdb_bridge.cpp`, which is plain C++ over this header with no React Native in
it, and is compiled and tested by `scripts/test-react-native.sh`.
