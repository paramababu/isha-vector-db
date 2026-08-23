# vdb in Node.js

## Install

```bash
npm install @vdb/node
```

A native addon, so there is a prebuilt binary per platform. Node 18 or newer.

### From a checkout

```bash
./scripts/build-node.sh
```

That builds the addon and puts it where `sdk/node/index.js` expects it.

## Your first database

```js
const vdb = require('@vdb/node');

const db = vdb.open('./my-notes');
try {
  const notes = db.collection('notes', { dimension: 4 });

  notes.upsert('note-1', new Float32Array([1, 0, 0, 0]));
  notes.upsert('note-2', new Float32Array([0.9, 0.1, 0, 0]));
  notes.upsert('note-3', new Float32Array([0, 0, 1, 0]));
  notes.flush();

  for (const hit of notes.search(new Float32Array([1, 0, 0, 0]), 2)) {
    console.log(hit.id, hit.score.toFixed(4));
  }
} finally {
  db.close();
}
```

```text
note-1 1.0000
note-2 0.9939
```

The `try`/`finally` is not decoration. An open database holds a lock, and a throw that skips
`close()` leaves it held until the process exits.

## Vectors

**Use a `Float32Array`.** A plain `number[]` works and is converted for you, which is a copy per
call — fine occasionally, wasteful in a loop over ten thousand documents.

```js
notes.upsert('id', new Float32Array(embedding));   // no copy
notes.upsert('id', [0.1, 0.2, 0.3]);               // converted
```

## Metadata and filters

Metadata is stored alongside the vector and can narrow a search.

```js
notes.upsert('note-1', vector, { kind: 'meeting', year: 2026, starred: true });

// A filter is a plain object. A bare value means equality, and several keys mean "and".
notes.search(query, 10, { kind: 'meeting' });
notes.search(query, 10, { kind: 'meeting', starred: true });
notes.search(query, 10, { year: { $gte: 2026 } });
notes.search(query, 10, { $or: [{ kind: 'meeting' }, { kind: 'call' }] });
notes.search(query, 10, {});          // matches everything
```

Predicates are `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`, `$in`, and the combinators `$and`,
`$or`, `$not`.

Integers stay integers through the round trip — `2026` comes back as `2026`, not `2026.0`.

Three rules about filters surprise people, and all three are deliberate: an absent field equals
`null`; `$ne` is the exact negation of equality, so it *matches* documents lacking the field; and
`$gt` and `$lte` are both false where no ordering exists, so they are not negations of one
another. Comparing a string to a number is `false`, never an error.

A filter narrows *results*, not work: the engine still considers each document to decide whether
it matches, so a filtered search is not faster than an unfiltered one. See
[filters.md](../api/filters.md) for what it actually costs.

## Everything else

```js
notes.count();                 // how many documents
notes.contains('note-1');      // does it exist
notes.delete('note-1');        // → true if it existed
notes.flush();                 // write buffered changes
notes.stats();                 // rows, dead rows, bytes on disk
notes.name;  notes.dimension;  // what it was created as

db.flush();                    // every collection
db.listCollections();          // their names
db.openCollection('notes');    // an existing one, without a dimension
db.dropCollection('notes');    // irreversible
db.verify();                   // integrity check
db.compact();                  // reclaim space from deleted documents
db.isOpen;
```

There is no `get` by id: this is a search index, and fetching a document you already have the id
of is a job for whatever store the id came from. Metadata is observed through filters.

Hits carry a `distance` as well as a `score` for cosine and L2 — the metric's own distance, where
one is defined. Inner product has no corresponding distance and reports `null` rather than
inventing one.

## Errors

Failures are `Error` objects with a numeric `code` from the engine.

```js
try {
  notes.upsert('bad', new Float32Array([1, 2]));
} catch (e) {
  console.log(e.code);      // 4003  (a number)
  console.log(e.message);   // [VDB-4003] collection "notes" stores 4-dimensional vectors, got 2
}
```

Branch on `code`; the message is for a human and is not stable.
[The full list](../api/error-codes.md) is grouped by band, so `4xxx` is a validation mistake and
`5xxx` is storage trouble even if you do not recognise the specific code.

## Things that catch people out

**Calls are synchronous and block the event loop.** There is no `await` here, and a search over a
large collection stops your server answering anything else for its duration. For a busy service,
run vdb in a worker thread.

**One process at a time.** A second `open()` on the same directory fails with `VDB-2001` rather
than waiting.

**This is not the WebAssembly build.** `@vdb/node` is a native addon and is the right choice on a
server. [The web SDK](web.md) exists for browsers and is slower.

## Complete example

```js
const vdb = require('@vdb/node');
const { pipeline } = require('@xenova/transformers');

const embed = await pipeline('feature-extraction', 'Xenova/all-MiniLM-L6-v2');
const vector = async (text) => {
  const out = await embed(text, { pooling: 'mean', normalize: true });
  return new Float32Array(out.data);
};

const db = vdb.open('./notes-index');
try {
  const notes = db.collection('notes', { dimension: 384 });

  if (notes.count() === 0) {
    for (const { id, text } of documents) {
      notes.upsert(id, await vector(text), { indexed: Date.now() });
    }
    notes.flush();
  }

  for (const hit of notes.search(await vector('notes about the boat'), 5)) {
    console.log(hit.score.toFixed(3), hit.id);
  }
} finally {
  db.close();
}
```
