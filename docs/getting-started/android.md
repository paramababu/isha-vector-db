# isha-vector-db on Android

Kotlin or Java. The engine is a native library reached through JNI; you write ordinary Java.

## Install

```kotlin
// settings.gradle.kts
dependencyResolutionManagement {
    repositories { mavenCentral() }
}

// app/build.gradle.kts
dependencies {
    implementation("dev.vdb:vdb-android:0.1.0")
}
```

The AAR carries `libvdb.so` for `arm64-v8a`, `armeabi-v7a`, `x86_64` and `x86`. Nothing to
configure and no NDK needed to *use* it.

Minimum SDK 21.

> **Not published to Maven Central yet.** Build from a checkout for now — see below, and
> [why](README.md#not-yet-published).

### From a checkout

```bash
./scripts/build-android.sh      # builds .so for every ABI
./scripts/test-java.sh          # runs the JVM tests against a host build
```

## Where to put the database

Use `Context.filesDir`. Not external storage, which other apps can read and which may not be
mounted; not the cache directory, which the system deletes under pressure — losing an index is
recoverable, losing the only copy of the data is not.

```kotlin
val path = File(context.filesDir, "notes").absolutePath
```

## Your first database

```kotlin
import dev.isha.vectordb.Vdb
import dev.isha.vectordb.Metric

val db = Vdb.open(File(context.filesDir, "notes").absolutePath)
try {
    val notes = db.collection("notes", 4, Metric.COSINE)

    notes.upsert("note-1", floatArrayOf(1f, 0f, 0f, 0f))
    notes.upsert("note-2", floatArrayOf(0.9f, 0.1f, 0f, 0f))
    notes.upsert("note-3", floatArrayOf(0f, 0f, 1f, 0f))
    notes.flush()

    for (hit in notes.search(floatArrayOf(1f, 0f, 0f, 0f), 2)) {
        Log.d("vdb", "${hit.id} ${hit.score}")
    }
} finally {
    db.close()
}
```

`Database` and `Collection` are `AutoCloseable`, so Kotlin's `use` is tidier:

```kotlin
Vdb.open(path).use { db ->
    db.collection("notes", 384).use { notes ->
        // ...
    }
}
```

## Never on the main thread

A search is a blocking call, and a large one takes tens of milliseconds. On the main thread that
is dropped frames, and Android will show an ANR if it is bad enough.

```kotlin
class NoteSearch(context: Context) {
    private val db = Vdb.open(File(context.filesDir, "notes").absolutePath)
    private val notes = db.collection("notes", 384)

    suspend fun search(query: FloatArray): List<Hit> = withContext(Dispatchers.IO) {
        notes.search(query, 10)
    }

    fun close() = db.close()
}
```

`Dispatchers.IO` rather than `Default`: the work is a mix of computation and file reads, and
`Default` is sized for pure CPU work.

## Metadata and filters

```java
Metadata meta = new Metadata()
    .put("kind", "meeting")
    .put("year", 2026)
    .put("starred", true);
notes.upsert("note-1", vector, meta);

List<Hit> hits = notes.search(query, 10,
    Filter.and(Filter.eq("kind", "meeting"), Filter.gte("year", 2026)));
```

A filter narrows results, not work — the engine still considers each document to decide whether it
matches. See [filters.md](../api/filters.md).

## Lifecycle

Tie the database to something that outlives a screen rotation. An `Activity` is recreated on
configuration change, and reopening a database whose previous handle has not been closed fails
with `VDB-2001`.

```kotlin
class SearchViewModel(app: Application) : AndroidViewModel(app) {
    private val db = Vdb.open(File(app.filesDir, "notes").absolutePath)

    override fun onCleared() {
        db.close()
    }
}
```

An `Application`-scoped singleton is the other reasonable choice.

## Errors

Every failure is a `VdbException` carrying the engine's code.

```kotlin
try {
    notes.upsert("bad", floatArrayOf(1f, 2f))
} catch (e: VdbException) {
    Log.e("vdb", "code=${e.code} ${e.message}")
    // code=4003  [VDB-4003] collection "notes" stores 4-dimensional vectors, got 2
}
```

Branch on `code`, not the message. [The full list](../api/error-codes.md) is banded, so `4xxx` is
a validation mistake and `5xxx` is storage trouble.

## Things that catch people out

**`FloatArray`, not `DoubleArray`.** Kotlin's `doubleArrayOf` will not compile against these
signatures, which is the intended outcome — the engine stores 32-bit floats.

**One handle per database, process-wide.** Two `Vdb.open` calls on the same directory fail.

**App backup can copy a database mid-write.** If you use Android's auto-backup, exclude the
database directory or accept that a restored copy may need `verify()`. The engine's recovery
handles a torn write, but a backup taken between two files is a different thing.

**ProGuard.** The JNI layer looks up classes by name; keep them:

```proguard
-keep class dev.isha.vectordb.** { *; }
```
