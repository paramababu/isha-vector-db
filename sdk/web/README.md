# @vdb/web

The same engine, compiled to WebAssembly, storing in the Origin Private File System.

```js
import { load, Metric } from '@vdb/web';
import { opfsAdapter } from '@vdb/web/adapters/opfs.js';

const adapter = await opfsAdapter('my-app');
const vdb = await load(fetch('/vdb.wasm'), adapter);

const db = vdb.open('/notes');
const notes = db.collection('notes', 384, Metric.Cosine);

notes.upsert('note-1', embedding);
notes.flush();

for (const { id, score } of notes.search(query, 10)) {
  console.log(id, score);
}
```

## How this is built

There is no `wasm-bindgen` and no bundler step. This SDK drives `include/vdb.h` — the identical
C ABI the iOS, Android and Node SDKs use — so the whole toolchain is `cargo build --target
wasm32-unknown-unknown`. `scripts/build-web.sh` does it.

Two functions exist that the C header does not declare, `vdb_wasm_alloc` and `vdb_wasm_free`.
JavaScript cannot allocate inside the module's linear memory, so the module has to hand it a
region; a C caller has its own allocator and never needs them. They are a calling convention, not
database functionality, and `vdb_abi_version()` does not cover them.

## Storage adapters

The engine calls storage synchronously. OPFS can do synchronous I/O through
`FileSystemSyncAccessHandle`, but *obtaining* one is asynchronous, so a file cannot be opened at
the moment the engine asks for it. `opfsAdapter` therefore opens a pool of handles at start-up
and assigns them to paths on demand, with each slot carrying a header naming the path it holds so
the mapping is recovered on reload without a separate index to keep consistent.

Set `slots` to comfortably exceed the number of files your database will hold; running out raises
an error naming the pool rather than failing obscurely.

`adapters/node.js` is a second adapter over `node:fs`, used to test this stack without a browser.
It is not a supported way to run vdb on a server — use the native addon in `sdk/node`, which has
no WebAssembly boundary.

## Run it in a Worker

OPFS synchronous access handles are only available in a Worker, and a search is a blocking call:
running the engine on the main thread would block rendering for its duration. Put this SDK in a
dedicated Worker and post messages to it.

## What is tested, and what is not

`test/wasm.test.js` and `test/opfs.test.js` run the real WebAssembly module — 7 tests covering
writes, search, deletes, reopening from persisted bytes, structured errors, slot-mapping recovery
across a remount, and pool exhaustion. The Rust side adds the 25-check storage conformance suite
and a full engine test against the same backend.

**Real OPFS is not covered by any of that.** `test/opfs.test.js` runs against a stand-in that
implements the specified semantics, which by construction agrees with the adapter's assumptions.
`test/browser.html` is the check that only a browser can perform: serve this directory over HTTP,
open it, and reload once to confirm persistence. Run it before relying on OPFS in production.

## Durability

OPFS `flush()` is best-effort — it is not a barrier against power loss the way `fsync` is. The
storage backend reports `durable_sync: false` and the engine downgrades what it promises rather
than claiming a guarantee the platform cannot keep. The write-ahead log still protects against
the failure that actually happens in a browser: the tab going away.
