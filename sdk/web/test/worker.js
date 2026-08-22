// The verification run, inside a Worker.
//
// This is not a testing detail: `createSyncAccessHandle()` exists only on a Worker thread. On
// the main thread the method is simply absent, and the first attempt at this page failed with
// "handle.createSyncAccessHandle is not a function" — which is what the SDK README already said
// would happen, and what no amount of Node testing could have shown.
//
// So this file doubles as the usage example. A search is a blocking call; running it here also
// keeps it off the thread that paints.

import { load, Metric } from '../src/vdb.js';
import { opfsAdapter } from '../src/adapters/opfs.js';

const report = (ok, text) => postMessage({ kind: 'result', ok, text });
const check = (ok, text) => {
  report(!!ok, text);
  if (!ok) throw new Error(text);
};

async function main() {
  check(typeof FileSystemFileHandle !== 'undefined', 'OPFS types are available in this Worker');
  check(
    typeof FileSystemFileHandle.prototype.createSyncAccessHandle === 'function',
    'createSyncAccessHandle exists on this thread',
  );

  const adapter = await opfsAdapter('vdb-browser-check', { slots: 64 });
  const wasm = await fetch(new URL('../vdb.wasm', import.meta.url));
  const vdb = await load(wasm, adapter);

  check(vdb.abiVersion === 1, `ABI version is 1 (got ${vdb.abiVersion})`);
  report(true, `format version ${vdb.formatVersion}, library ${vdb.version}`);

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

  db.close();
  vdb.dispose();
  adapter.close();

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
  postMessage({ kind: 'done', firstRun: before === 0 });
}

main().catch((e) => {
  report(false, `${e.name}: ${e.message}`);
  postMessage({ kind: 'done', error: true });
});
