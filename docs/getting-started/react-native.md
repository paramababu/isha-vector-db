# vdb in React Native

> **Read this first.** The native logic is tested (56 C++ checks against the real engine, 11 JS
> tests), but **nobody has built this into an app**. The JSI value conversion and the iOS and
> Android packaging are unverified — there is no Xcode or Gradle project in this repository to try
> them against. Expect to fix something in the podspec or the CMake wiring on first use, and
> please report what.

## Requirements

- React Native **0.73 or newer**, **New Architecture enabled**. The old bridge is not supported —
  it JSON-serialises everything, and a 768-float vector becomes a ~10 KB JSON array, which for a
  large batch insert *is* the operation.
- **Expo Go cannot load this**, or any custom native code. You need a development build
  (`npx expo prebuild` then `npx expo run:ios` / `run:android`).

## Install

```bash
npm install @vdb/react-native
cd ios && pod install        # iOS only
```

Rebuild the app. A JS-only reload will not pick up new native code, and the error message says so
if you forget.

## Your first database

```js
import { open, Metric } from '@vdb/react-native';
import RNFS from 'react-native-fs';

const db = open(`${RNFS.DocumentDirectoryPath}/notes`);
try {
  const notes = db.collection('notes', 4, Metric.Cosine);

  notes.upsert('note-1', new Float32Array([1, 0, 0, 0]));
  notes.upsert('note-2', new Float32Array([0.9, 0.1, 0, 0]));
  notes.flush();

  for (const hit of notes.search(new Float32Array([1, 0, 0, 0]), 2)) {
    console.log(hit.id, hit.score);
  }
} finally {
  db.close();
}
```

## Pass a Float32Array

This is the whole point of using JSI. A `Float32Array` crosses as a pointer with no copy; a plain
array is converted first.

```js
notes.upsert('id', new Float32Array(embedding));   // no copy
notes.upsert('id', [0.1, 0.2, 0.3]);               // converted
notes.upsert('id', 'oops');                        // refused before it reaches C++
```

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

Not implemented yet: the architecture specifies a dedicated C++ thread with results delivered
through the `CallInvoker`, and the current binding is synchronous throughout. A 40 ms search on
the JS thread is three dropped frames.

Until that lands, keep collections small on this platform, or move the work behind
`InteractionManager.runAfterInteractions` so it does not compete with an animation.

## Errors

```js
try {
  notes.upsert('bad', new Float32Array([1, 2]));
} catch (e) {
  console.log(e.code);      // 4003
  console.log(e.message);   // [VDB-4003] collection "notes" stores 4-dimensional vectors, got 2
}
```

Branch on `code`; [the full list](../api/error-codes.md) is banded.

## Where to put the database

| Platform | Directory | Why |
|---|---|---|
| iOS | `RNFS.LibraryDirectoryPath` | Not user-visible in the Files app |
| Android | `RNFS.DocumentDirectoryPath` | Maps to internal storage, private to the app |

Neither should be the cache directory: the system deletes it under pressure.

## If it does not build

The two unverified pieces are the podspec (`sdk/react-native/vdb.podspec`) and the Android CMake
(`sdk/react-native/android/CMakeLists.txt`). Both are short and commented. The static libraries
they expect come from `scripts/build-xcframework.sh` and `scripts/build-android.sh`.
