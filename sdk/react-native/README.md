# @vdb/react-native

The embedded vector database in a React Native app, over JSI.

```js
import { open, Metric } from '@vdb/react-native';

const db = open(`${documentsDirectory}/notes`);
const notes = db.collection('notes', 384, Metric.Cosine);

notes.upsert('note-1', embedding);   // a Float32Array crosses as a pointer
notes.flush();

for (const { id, score } of notes.search(query, 10)) {
  console.log(id, score);
}

db.close();   // explicit; see below
```

## What is verified, and what is not

Be aware of this before depending on it.

| layer | tested how |
|---|---|
| `src/index.js` — the JS API | 11 tests in Node against a mock host object |
| `cpp/vdb_bridge.cpp` — handles, errors, lifetimes | 56 checks, compiled and run against the real engine by `scripts/test-react-native.sh` |
| `cpp/vdb_jsi.cpp` — JSI value conversion | **not tested**; needs a running app |
| iOS and Android packaging | **not tested**; needs Xcode and Gradle with a real project |

The split is deliberate. A JSI `HostObject` cannot be compiled outside a React Native app, so
anything written inside one is unverifiable until it is on a device — a bad place to put logic.
`vdb_bridge.cpp` therefore holds all of it (handle lifetimes, error translation, use-after-close,
the create-or-open fallback) in plain C++ that runs on a development machine, and `vdb_jsi.cpp` is
left with one job: convert JS values, call one method, convert back. If a change needs a branch in
the JSI file, it probably belongs in the bridge where it can be tested.

That leaves value conversion and the platform packaging genuinely unverified. **Nobody has built
this into an app.** Expect to fix something in the podspec or the Gradle wiring on first use.

## Why JSI and not the bridge

The legacy bridge JSON-serialises everything, so a 768-float vector becomes a ~10 KB JSON array.
For a batch insert of ten thousand vectors that serialisation *is* the operation. JSI hands the
C++ layer the `ArrayBuffer` backing store directly.

Pass a `Float32Array` and it crosses as a pointer with no copy. A plain array is converted first,
which is a copy — fine for a one-off, worth avoiding on a hot path.

## New Architecture only

React Native ≥ 0.73, New Architecture enabled. Supporting the old bridge would mean maintaining
two transports for a library whose selling point is speed.

**Expo Go cannot load this**, or any custom native code. You need a development build. The error
message says so, because otherwise it is the first issue anyone files.

## Closing is not optional

`db.close()` is explicit and you should call it. A `HostObject` is destroyed by the JavaScript
garbage collector, which is not deterministic and may never run before the app is killed — so an
unclosed database can hold its lock file until the process dies, and the next launch finds it held
by a process that no longer exists.

The C++ bridge releases anything outstanding when it is destroyed, which covers app teardown, and
`__vdb.liveHandles` reports what is open so a development build can warn.

## Threading

Calls are synchronous and run on the calling thread. A search over a large collection will block
JavaScript, and a 40 ms search on the JS thread is three dropped frames — so for anything but a
small corpus, run it off the JS thread.

The architecture (§9.1) specifies a dedicated C++ thread with a serial queue, delivering results
through the RN `CallInvoker`. **That is not implemented here.** The current binding is synchronous
throughout, which is correct and simple but puts the cost on the caller's thread.

## Nitro Modules

ADR-0011 flagged [Nitro Modules](https://nitro.margelo.com/) as worth evaluating, to generate the
JSI bindings rather than hand-writing them. That evaluation has not happened. If it were adopted,
`vdb_jsi.cpp` is the file it would replace — which is another reason for keeping the logic out of
it.
