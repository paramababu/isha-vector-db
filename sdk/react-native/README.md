# @isais-logic/isha-vector-db-react-native

The embedded vector database in a React Native app, over JSI.

```js
import { open } from '@isais-logic/isha-vector-db-react-native';

const db = open(`${documentsDirectory}/notes`);
const notes = db.collection('notes', { dimension: 384, metric: 'cosine' });

notes.upsert('note-1', embedding, { folder: 'work', pinned: true });
notes.flush();

for (const { id, score } of notes.search(query, 10, { folder: 'work' })) {
  console.log(id, score);
}

notes.release();
db.close();   // explicit; see below
```

## What is verified, and what is not

Be aware of this before depending on it.

| layer | tested how |
|---|---|
| `src/*.js` — the API, filter and metadata compilers | 45 tests in Node against a mock host object |
| `cpp/vdb_bridge.cpp` — handles, errors, lifetimes, filters, batches | 123 checks, compiled **and run** against the real engine |
| `cpp/vdb_jsi.cpp` — JSI value conversion | **compiled** against React Native 0.73 and 0.87's own headers; never **run** |
| The published tarball's contents | checked against what the build files reference |
| iOS and Android packaging actually building | **not tested**; needs Xcode and Gradle with a real project |

All of it runs on every push: `scripts/test-react-native.sh` for the first three,
`scripts/check-react-native-package.sh` for the fourth.

The split is deliberate. A JSI `HostObject` cannot be *run* outside a React Native app, so
anything written inside one is unverifiable until it is on a device — a bad place to put logic.
`vdb_bridge.cpp` therefore holds all of it (handle lifetimes, error translation, use-after-close,
the create-or-open fallback, the postfix filter and metadata builders) in plain C++ that executes
on a development machine, and `vdb_jsi.cpp` is left with one job: convert JS values, call one
method, convert back.

Compiling that file against the real headers catches a moved signature or a conversion that does
not type-check. It cannot catch a conversion that type-checks and is wrong, which is why the
logic is not there.

**Nobody has built this into an app.** That is the remaining gap, and it is a real one.

### What went wrong in 0.1.0

Version 0.1.0 was published to npm and could not be installed by anybody. Its tarball held nine
files, and was missing:

- `android/build.gradle`, so autolinking found no Android module and contributed nothing;
- `android/src/main/cpp/vdb_module.cpp`, which `CMakeLists.txt` named as a source;
- everything under `ios/`, though the podspec globbed it;
- `vdb.h`, which every C++ file in the package includes;
- the engine itself, for every architecture.

Each of those is mechanically checkable from the tarball without an Android or iOS toolchain,
which is what `scripts/check-react-native-package.sh` now does — and the release workflow will
not publish if it fails. The platform builds still cannot be run in CI. "The package contains
what it says it contains" can be, and that is the failure that actually shipped.

## The API is the Node SDK's

Deliberately, and to the letter: same names, same argument order, same defaults, same error
codes. [`sdk/node/index.d.ts`](../node/index.d.ts) is the canonical shape of the JavaScript API
and this package mirrors it, because a query written against one should work against the other
unchanged and semantic divergence between SDKs is the fastest way to make a cross-platform
library untrustworthy.

Two things differ, because the platform made them differ:

- **A `Collection` holds a native handle** you should `release()`. React Native's garbage
  collector will not do it at any predictable moment.
- **Everything is synchronous** and runs on the calling thread. See *Threading*.

Filters are query objects (`{ price: { $lt: 50 } }`), metadata is a flat object of scalars, and
`upsertMany` takes a batch. [The getting-started
page](../../docs/getting-started/react-native.md) has examples of each;
[`docs/api/filters.md`](../../docs/api/filters.md) has the semantics.

### Migrating from 0.1.0

The 0.1.0 API could not run, so nothing is depending on it, and it has been changed rather than
kept for compatibility with a version that never worked:

| 0.1.0 | now |
|---|---|
| `db.collection('c', 384, Metric.Cosine)` | `db.collection('c', { dimension: 384, metric: 'cosine' })` |
| `Metric.Cosine`, `Metric.L2`, `Metric.Dot` | `'cosine'`, `'l2'`, `'dot'` |
| `collection.has(id)` | `collection.contains(id)` |
| — | `upsert(id, vector, metadata)`, `upsertMany`, `search(q, k, filter)`, `stats`, `db.compact`, `db.verify`, `db.openCollection`, `db.dropCollection`, `db.flush` |

## Why JSI and not the bridge

The legacy bridge JSON-serialises everything, so a 768-float vector becomes a ~10 KB JSON array.
For a batch insert of ten thousand vectors that serialisation *is* the operation. JSI hands the
C++ layer the `ArrayBuffer` backing store directly.

Pass a `Float32Array` and it crosses as a pointer with no copy. A plain array is converted first,
which is a copy — fine for a one-off, worth avoiding on a hot path.

`upsertMany` goes further: it packs the whole batch into one contiguous buffer and crosses once,
rather than paying a crossing and an `ArrayBuffer` lookup per document.

## New Architecture only

React Native ≥ 0.73, New Architecture enabled. Supporting the old bridge would mean maintaining
two transports for a library whose selling point is speed. CI compiles the JSI layer against both
ends of that range — 0.73 and the current release — so the oldest supported version cannot
quietly stop working.

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
small corpus, run it off the JS thread. `compact()` in particular can run for a long time.

The architecture (§9.1) specifies a dedicated C++ thread with a serial queue, delivering results
through the RN `CallInvoker`. **That is not implemented here.** The current binding is synchronous
throughout, which is correct and simple but puts the cost on the caller's thread.

## Package size

The tarball carries the engine for three Android ABIs and three Apple architectures, because an
app's build chooses its architectures — npm's `os`/`cpu` resolution, which lets the Node SDK ship
one platform per install, cannot help here. The archives are stripped of debug information, which
takes about a third off; what an app actually pays is far less again, since the linker discards
what is unreachable. `scripts/measure-ios-size.sh` reports that figure.

## Nitro Modules

ADR-0011 flagged [Nitro Modules](https://nitro.margelo.com/) as worth evaluating, to generate the
JSI bindings rather than hand-writing them. That evaluation has not happened. If it were adopted,
`vdb_jsi.cpp` is the file it would replace — which is another reason for keeping the logic out of
it.

## Working on this package

```bash
./scripts/test-react-native.sh            # all three test layers
./scripts/check-react-native-package.sh   # the tarball contains what the build names
./scripts/build-react-native.sh           # assemble the engine into the package
```

`build-react-native.sh` needs the NDK for Android and Xcode for iOS, and skips whichever it
cannot find rather than failing — the common case is a contributor who has one of the two.
