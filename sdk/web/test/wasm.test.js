// The web stack, end to end: JavaScript to the C ABI to the engine to storage and back.
//
// This runs the real WebAssembly module against a node:fs adapter. What it cannot cover is OPFS
// itself, which needs a browser; what it does cover is everything else, which is where the bugs
// are — pointer marshalling, the error out-parameter dance, the listing encoding, and the engine
// running on a backend with neither atomic rename nor durable sync.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

import { load, VdbError } from '../src/vdb.js';
import { nodeAdapter } from '../src/adapters/node.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const WASM = path.join(here, '..', 'vdb.wasm');

function scratch() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'vdb-web-'));
  return { dir, cleanup: () => fs.rmSync(dir, { recursive: true, force: true }) };
}

async function engine(dir) {
  return load(fs.readFileSync(WASM), nodeAdapter(dir));
}

test('reports its versions', async () => {
  const s = scratch();
  try {
    const vdb = await engine(s.dir);
    assert.ok(vdb.version.length > 0);
    // The ABI is frozen at 1; the on-disk format moves independently.
    assert.equal(vdb.abiVersion, 1);
    assert.ok(vdb.formatVersion >= 1);
    vdb.dispose();
  } finally {
    s.cleanup();
  }
});

test('writes, searches and reopens', async () => {
  const s = scratch();
  try {
    const vdb = await engine(s.dir);
    const db = vdb.open('/db');
    const docs = db.collection('docs', 4);

    for (let i = 0; i < 8; i++) {
      assert.equal(docs.upsert(`doc-${i}`, [i, 1, 0, -1]), true);
    }
    assert.equal(docs.count(), 8);
    assert.equal(docs.has('doc-3'), true);
    assert.equal(docs.has('nope'), false);

    const hits = docs.search([7, 1, 0, -1], 3);
    assert.equal(hits.length, 3);
    assert.equal(hits[0].id, 'doc-7');
    // Scores are higher-is-better, so they descend.
    assert.ok(hits[0].score >= hits[1].score);

    docs.flush();
    db.close();
    vdb.dispose();

    // A second instance, reading only what reached storage.
    const again = await engine(s.dir);
    const db2 = again.open('/db', { createIfMissing: false });
    const docs2 = db2.collection('docs', 4);
    assert.equal(docs2.count(), 8, 'the data survived the round trip through storage');
    assert.equal(docs2.search([7, 1, 0, -1], 1)[0].id, 'doc-7');
    db2.close();
    again.dispose();
  } finally {
    s.cleanup();
  }
});

test('deletes', async () => {
  const s = scratch();
  try {
    const vdb = await engine(s.dir);
    const db = vdb.open('/db');
    const docs = db.collection('docs', 3);
    docs.upsert('a', [1, 0, 0]);
    docs.upsert('b', [0, 1, 0]);
    assert.equal(docs.delete('a'), true);
    assert.equal(docs.delete('a'), false, 'deleting an absent document is not an error');
    assert.equal(docs.count(), 1);
    db.close();
    vdb.dispose();
  } finally {
    s.cleanup();
  }
});

test('surfaces the engine\'s own error message and code', async () => {
  const s = scratch();
  try {
    const vdb = await engine(s.dir);
    const db = vdb.open('/db');
    const docs = db.collection('docs', 3);
    assert.throws(
      () => docs.upsert('bad', [1, 2]),
      (e) => {
        assert.ok(e instanceof VdbError);
        // Not a flattened "upsert failed": the structured message has to reach the developer.
        assert.match(e.message, /3-dimensional/);
        assert.ok(e.code > 0);
        return true;
      },
    );
    db.close();
    vdb.dispose();
  } finally {
    s.cleanup();
  }
});
