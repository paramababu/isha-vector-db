// The IndexedDB fallback, verified in a Worker against the real thing.
//
// Unlike OPFS, IndexedDB works on the main thread too — but the engine belongs in a Worker
// regardless, because a search is a blocking call and would otherwise stall rendering.

import { load, Metric } from '../src/vdb.js';
import { indexedDbAdapter } from '../src/adapters/indexeddb.js';

const report = (ok, text) => postMessage({ kind: 'result', ok, text });
const check = (ok, text) => {
  report(!!ok, text);
  if (!ok) throw new Error(text);
};

async function main() {
  check(typeof indexedDB !== 'undefined', 'IndexedDB is available in this Worker');

  const adapter = await indexedDbAdapter('vdb-idb-check');
  const wasm = await fetch(new URL('../vdb.wasm', import.meta.url));
  const vdb = await load(wasm, adapter);

  const db = vdb.open('/db');
  const docs = db.collection('docs', 4, Metric.Cosine);

  const before = docs.count();
  report(true, `documents already present: ${before}`);

  for (let i = 0; i < 20; i++) docs.upsert(`doc-${i}`, [i, 1, 0, -1]);
  docs.flush();
  check(docs.count() >= 20, `at least 20 documents after writing (got ${docs.count()})`);

  const hits = docs.search([19, 1, 0, -1], 3);
  check(hits[0].id === 'doc-19', `nearest neighbour is doc-19 (got ${hits[0].id})`);
  check(hits[0].score >= hits[1].score, 'scores descend');

  check(docs.delete('doc-0') === true, 'delete reports it existed');
  check(docs.has('doc-0') === false, 'the deleted document is gone');

  report(true, `resident: ${Math.round(adapter.bytesResident() / 1024)} KiB`);

  db.close();
  vdb.dispose();

  // The whole point of the write-back cache: nothing is durable until this resolves.
  await adapter.close();
  check(true, 'the write-back queue drained');

  check(
    before === 0 || before >= 19,
    `a previous run left a consistent collection (${before} documents)`,
  );
  report(
    true,
    before > 0
      ? 'Persistence confirmed: this run found data written by a previous one.'
      : 'First run complete. Reload to confirm the data persisted.',
  );
  postMessage({ kind: 'done' });
}

main().catch((e) => {
  report(false, `${e.name}: ${e.message}`);
  postMessage({ kind: 'done', error: true });
});
