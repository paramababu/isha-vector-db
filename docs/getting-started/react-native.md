# isha-vector-db in React Native

> **Read this first.** Version 0.1.0 was published to npm and **could not be installed by
> anybody** — the tarball was missing the Gradle module, the iOS sources, a C++ file its own
> CMakeLists named, and the C ABI header. All four are now present and a CI check refuses to
> publish a tarball missing any of them, but **nobody has yet built this into an app**. The JSI
> layer is compiled against React Native's real headers on every push; it has not been *run*.
> Expect to fix something on first use, and please report what.
>
> What is and is not verified is set out in
> [the SDK's README](../../sdk/react-native/README.md#what-is-verified-and-what-is-not).

## Requirements

- React Native **0.73 or newer**, **New Architecture enabled**. The old bridge is not supported —
  it JSON-serialises everything, and a 768-float vector becomes a ~10 KB JSON array, which for a
  large batch insert *is* the operation.
- **Expo Go cannot load this**, or any custom native code. You need a development build
  (`npx expo prebuild` then `npx expo run:ios` / `run:android`).

## Install

```bash
npm install @isais-logic/isha-vector-db-react-native
cd ios && pod install        # iOS only
```

Then rebuild the app. A JS-only reload will not pick up new native code, and the error message
says so if you forget.

The package carries the engine for three Android ABIs and three Apple architectures, which is why
it is large. An app's build chooses its architectures, so npm's `os`/`cpu` resolution — the trick
the Node SDK uses to ship one platform per install — cannot help here.

## Your first database

```js
import { open } from '@isais-logic/isha-vector-db-react-native';
import RNFS from 'react-native-fs';

const db = open(`${RNFS.LibraryDirectoryPath}/notes`);
try {
  const notes = db.collection('notes', { dimension: 4, metric: 'cosine' });

  notes.upsert('note-1', new Float32Array([1, 0, 0, 0]), { folder: 'work', pinned: true });
  notes.upsert('note-2', new Float32Array([0.9, 0.1, 0, 0]), { folder: 'personal' });
  notes.flush();

  for (const hit of notes.search(new Float32Array([1, 0, 0, 0]), 2)) {
    console.log(hit.id, hit.score);
  }

  notes.release();
} finally {
  db.close();
}
```

## The API is the Node SDK's

Deliberately, and to the letter: same names, same argument order, same defaults, same error
codes. A query written against `@isais-logic/isha-vector-db-node` works here unchanged, and
[`sdk/node/index.d.ts`](../../sdk/node/index.d.ts) is the canonical reference for both.

Two things differ, because the platform made them differ:

- A `Collection` holds a native handle you should `release()`. React Native's garbage collector
  will not do it at any predictable moment.
- Every call is synchronous and runs on the calling thread. See below.

## Filters

Metadata predicates are query objects, the same shape Node takes:

```js
notes.search(query, 10, { folder: 'work' });
notes.search(query, 10, { folder: 'work', pinned: true });          // both must hold
notes.search(query, 10, { $or: [{ folder: 'work' }, { pinned: true }] });
notes.search(query, 10, { $not: { archived: true } });
notes.search(query, 10, { updated: { $gt: 1_700_000_000 } });
notes.search(query, 10, { folder: { $in: ['work', 'personal'] } });
notes.search(query, 10, { tags: { $contains: 'urgent' } });         // array membership
notes.search(query, 10, { reminder: { $exists: true } });
```

`topK` counts **matches**, not candidates: a filter excluding most of the collection still
returns up to `topK` results.

Evaluation is total — comparing a string to a number is `false`, never an error — and three rules
surprise people. [`docs/api/filters.md`](../api/filters.md) is the reference.

Everything above is compiled to the postfix sequence the C ABI takes *in JavaScript*, in
[`src/filter.js`](../../sdk/react-native/src/filter.js), which is why it has real tests rather
than only a device to try it on.

## Batches

One JSI crossing for the whole batch, with every vector packed into one contiguous buffer. This
is what JSI is here for; a loop of `upsert` calls pays a crossing and an `ArrayBuffer` lookup per
document.

```js
const inserted = notes.upsertMany([
  { id: 'a', vector: new Float32Array(embeddingA), metadata: { folder: 'work' } },
  { id: 'b', vector: new Float32Array(embeddingB) },
]);
```

Every vector in one batch must be the same length; a short one is refused rather than silently
shifting every subsequent document's floats. It is **not** a transaction — the C ABI has no batch
write, so a failure part-way leaves the documents before it written, and the error names the one
that stopped it.

## Pass a Float32Array

A `Float32Array` crosses as a pointer with no copy; a plain array is converted first.

```js
notes.upsert('id', new Float32Array(embedding));   // no copy
notes.upsert('id', [0.1, 0.2, 0.3, 0.4]);          // converted
notes.upsert('id', 'oops');                        // refused before it reaches C++
```

## Maintenance

```js
const { deadRatio, liveDocuments } = notes.stats();
if (deadRatio > 0.3) {
  db.compact();                    // rewrites segments at least 20% dead by default
}

const { errors, warnings } = db.verify('checksums');
```

Compaction is explicit rather than automatic: rewriting hundreds of megabytes is a decision about
when to spend I/O and battery, and your app knows more about that than the engine does — when the
device is charging, when the screen is off, when the user is not waiting.

## Closing is not optional

A JSI host object is destroyed by the JavaScript garbage collector, which is not deterministic and
may never run before the app is killed. An unclosed database can hold its lock file until the
process dies, and the next launch finds it held by a process that no longer exists.

```js
useEffect(() => {
  const db = open(path);
  return () => db.close();
}, []);
```

`globalThis.__vdb.liveHandles` reports how many handles are outstanding, which is worth asserting
in a development build.

## Calls block the JS thread

Still not implemented: the architecture specifies a dedicated C++ thread with results delivered
through the `CallInvoker`, and the binding is synchronous throughout. A 40 ms search on the JS
thread is three dropped frames.

Until that lands, keep collections small on this platform, or move the work behind
`InteractionManager.runAfterInteractions` so it does not compete with an animation. `compact()`
in particular can run for a long time and should not be called from a screen the user is looking
at.

## Errors

```js
try {
  notes.upsert('bad', new Float32Array([1, 2]));
} catch (e) {
  console.log(e.code);      // 4003
  console.log(e.message);   // [VDB-4003] collection "notes" stores 4-dimensional vectors, got 2
}
```

Branch on `code`; [the full list](../api/error-codes.md) is banded. A `code` of `0` means the call
never reached the engine — a closed handle, a bad argument caught in JavaScript, or the native
module not being installed.

## Where to put the database

| Platform | Directory | Why |
|---|---|---|
| iOS | `RNFS.LibraryDirectoryPath` | Not user-visible in the Files app |
| Android | `RNFS.DocumentDirectoryPath` | Maps to internal storage, private to the app |

Neither should be the cache directory: the system deletes it under pressure.

## If it does not build

The unverified pieces are the podspec, the Gradle module and the two native modules that install
the bindings:

| File | What it does |
|---|---|
| [`isha-vector-db.podspec`](../../sdk/react-native/isha-vector-db.podspec) | iOS autolinking |
| [`ios/Vdb.mm`](../../sdk/react-native/ios/Vdb.mm) | Hands the JSI runtime to C++ on iOS |
| [`android/build.gradle`](../../sdk/react-native/android/build.gradle) | Android autolinking |
| [`android/CMakeLists.txt`](../../sdk/react-native/android/CMakeLists.txt) | Builds `libvdb.so` |
| [`android/src/main/java/.../VdbModule.java`](../../sdk/react-native/android/src/main/java/dev/isha/vectordb/reactnative/VdbModule.java) | Hands the JSI runtime to C++ on Android |

All are short and commented. Building the engine into a local checkout is
`./scripts/build-react-native.sh`, which needs the NDK for Android and Xcode for iOS and skips
whichever it cannot find.

If `globalThis.__vdb` is undefined after a clean rebuild, the native module did not install — the
first thing to check is whether this is a development build rather than Expo Go.
