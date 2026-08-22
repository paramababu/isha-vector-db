// The OPFS adapter, against a stand-in for OPFS.
//
// See `fake-opfs.js` for what this does and does not prove.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

import { installFakeOpfs } from './fake-opfs.js';
import { load, Metric } from '../src/vdb.js';
import { opfsAdapter } from '../src/adapters/opfs.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const WASM = fs.readFileSync(path.join(here, '..', 'vdb.wasm'));

test('the engine runs on the OPFS adapter', async () => {
  installFakeOpfs();
  const adapter = await opfsAdapter('vdb-test', { slots: 32 });
  const vdb = await load(WASM, adapter);
  const db = vdb.open('/db');
  const docs = db.collection('docs', 4, Metric.Cosine);
  for (let i = 0; i < 6; i++) docs.upsert(`doc-${i}`, [i, 1, 0, -1]);
  docs.flush();
  assert.equal(docs.count(), 6);
  assert.equal(docs.search([5, 1, 0, -1], 1)[0].id, 'doc-5');
  db.close();
  vdb.dispose();
  adapter.close();
});

test('slot assignments survive a reload, so a database reopens', async () => {
  const store = installFakeOpfs();

  {
    const adapter = await opfsAdapter('vdb-reload', { slots: 32 });
    const vdb = await load(WASM, adapter);
    const db = vdb.open('/db');
    const docs = db.collection('docs', 3, Metric.Cosine);
    docs.upsert('kept', [1, 0, 0]);
    docs.flush();
    db.close();
    vdb.dispose();
    adapter.close();
  }

  // Everything is dropped and remounted from the bytes in the store: the path-to-slot mapping
  // has to come back from the slot headers alone.
  installFakeOpfs(store);
  const adapter = await opfsAdapter('vdb-reload', { slots: 32 });
  const vdb = await load(WASM, adapter);
  const db = vdb.open('/db', { createIfMissing: false });
  const docs = db.collection('docs', 3, Metric.Cosine);
  assert.equal(docs.count(), 1);
  assert.equal(docs.has('kept'), true);
  db.close();
  vdb.dispose();
  adapter.close();
});

test('a full slot pool reports a real error rather than corrupting', async () => {
  installFakeOpfs();
  // Far fewer slots than a database needs, so the pool runs out mid-write.
  const adapter = await opfsAdapter('vdb-tiny', { slots: 3 });
  const vdb = await load(WASM, adapter);
  assert.throws(
    () => {
      const db = vdb.open('/db');
      const docs = db.collection('docs', 3, Metric.Cosine);
      for (let i = 0; i < 50; i++) docs.upsert(`d${i}`, [i, 0, 0]);
      docs.flush();
    },
    (e) => {
      // The message must name the cause. "I/O error" would send someone hunting the wrong bug.
      assert.match(e.message, /slot pool|storage|space/i);
      return true;
    },
  );
  vdb.dispose();
  adapter.close();
});
