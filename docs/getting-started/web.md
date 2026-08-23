# isha-vector-db in a browser (React, Vue, plain JavaScript)

The engine compiled to WebAssembly, storing in the browser's own filesystem. The database lives on
the user's device; nothing is uploaded.

## Install

```bash
npm install @isha-vector-db/web
```

> **Not published yet.** Build from a checkout for now — see below, and
> [why](README.md#not-yet-published).

You also need to serve `vdb.wasm` (366 KB) as a static asset. Most bundlers handle this with a URL
import:

```js
import wasmUrl from '@isha-vector-db/web/vdb.wasm?url';        // Vite
```

## Run it in a Worker — this is not optional

Two reasons, and the first is a hard requirement:

1. **OPFS synchronous access handles only exist on a Worker thread.** On the main thread the
   method is simply absent and the adapter fails with `createSyncAccessHandle is not a function`.
2. A search blocks for its duration. On the main thread that is dropped frames.

`worker.js`:

```js
import { load, Metric } from '@isha-vector-db/web';
import { opfsAdapter } from '@isha-vector-db/web/adapters/opfs.js';

let notes;

async function init() {
  const adapter = await opfsAdapter('my-app');
  const vdb = await load(fetch(new URL('./vdb.wasm', import.meta.url)), adapter);
  const db = vdb.open('/notes');
  notes = db.collection('notes', 384, Metric.Cosine);
}

const ready = init();

onmessage = async ({ data }) => {
  await ready;
  if (data.type === 'add') {
    notes.upsert(data.id, new Float32Array(data.vector));
    notes.flush();
    postMessage({ id: data.id, done: true });
  }
  if (data.type === 'search') {
    postMessage({ hits: notes.search(new Float32Array(data.vector), data.k ?? 10) });
  }
};
```

Create it as a module worker:

```js
const worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
```

## In React

```jsx
import { useEffect, useRef, useState } from 'react';

function useVdb() {
  const worker = useRef(null);
  useEffect(() => {
    worker.current = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
    return () => worker.current.terminate();
  }, []);
  return worker;
}

export function Search({ embed }) {
  const worker = useVdb();
  const [hits, setHits] = useState([]);

  async function onSearch(text) {
    const vector = await embed(text);
    worker.current.onmessage = ({ data }) => setHits(data.hits ?? []);
    worker.current.postMessage({ type: 'search', vector: Array.from(vector), k: 10 });
  }

  return (
    <>
      <input onChange={(e) => onSearch(e.target.value)} />
      <ul>{hits.map((h) => <li key={h.id}>{h.id} — {h.score.toFixed(3)}</li>)}</ul>
    </>
  );
}
```

The `terminate()` in the cleanup matters under React 18 StrictMode, which mounts effects twice in
development — without it you get two workers and two handles on the same database.

## Storage backends

**OPFS** is the default and the one to use. It is real file storage, fast, and available in
Chrome, Edge, Safari 15.2+ and Firefox 111+.

```js
const adapter = await opfsAdapter('my-app', { slots: 64 });
```

`slots` is a pool of pre-opened file handles — OPFS handles are acquired asynchronously but used
synchronously, so they must exist before the engine runs. Raise it if you see
`the OPFS slot pool is full`.

**IndexedDB** is the fallback, for older Safari and private-browsing modes where OPFS is
restricted:

```js
import { indexedDbAdapter } from '@isha-vector-db/web/adapters/indexeddb.js';
const adapter = await indexedDbAdapter('my-app');
```

Two things to know before choosing it. The whole database is held **in memory** — IndexedDB has no
synchronous API at all, so there is no other way to answer a synchronous read — and durability is
weaker: `await adapter.flush()` resolves when the write-back queue drains, and the adapter also
drains on `pagehide`. It does not survive a crash.

Detect and choose:

```js
const adapter = 'storage' in navigator && 'getDirectory' in navigator.storage
  ? await opfsAdapter('my-app')
  : await indexedDbAdapter('my-app');
```

## Errors

```js
try {
  notes.upsert('bad', new Float32Array([1, 2]));
} catch (e) {
  console.log(e.code);     // 4003
  console.log(e.message);  // [VDB-4003] collection "notes" stores 384-dimensional vectors, got 2
}
```

Branch on `code`; [the full list](../api/error-codes.md) is banded.

## Things that catch people out

**A secure context is required.** OPFS needs HTTPS or `localhost`. Opening an HTML file with
`file://` will not work.

**The browser can delete it.** Origin-private storage is evictable under pressure. Call
`navigator.storage.persist()` if the data matters, and treat the database as a cache you can
rebuild if it does not.

**Two tabs cannot share it.** Both will try to hold the same handles and the second will fail.
Use a `SharedWorker`, or a `BroadcastChannel` to elect one tab.

**Durability is weaker than a real filesystem.** OPFS `flush()` is best-effort, and the engine
reports that honestly rather than claiming a guarantee the platform cannot keep. The write-ahead
log still protects against the tab going away, which is the failure that actually happens.
