// A stand-in for the JSI host object, recording what it was asked to do.
//
// Shared by the API and filter tests. It records rather than simulates: what these tests check
// is that the JavaScript layer asks for the right thing in the right shape, and the answers it
// would get back are the bridge's business, checked against the real engine by
// `cpp/test_bridge.cpp`.

export function mockHost(overrides = {}) {
  const calls = [];
  const host = {
    version: '0.0.1',
    abiVersion: 1,
    formatVersion: 2,
    liveHandles: 0,
    open: (path, create, readOnly, durability) => {
      calls.push(['open', path, create, readOnly, durability]);
      return 1;
    },
    close: (h) => calls.push(['close', h]),
    flushDatabase: (h) => calls.push(['flushDatabase', h]),
    collection: (db, name, dimension, metric) => {
      calls.push(['collection', db, name, dimension, metric]);
      return 2;
    },
    openCollection: (db, name) => {
      calls.push(['openCollection', db, name]);
      return 3;
    },
    dropCollection: (db, name) => calls.push(['dropCollection', db, name]),
    releaseCollection: (h) => calls.push(['releaseCollection', h]),
    upsert: (h, id, vector, metadata) => {
      calls.push(['upsert', h, id, vector, metadata]);
      return true;
    },
    upsertMany: (h, ids, vectors, dimension, metadata) => {
      calls.push(['upsertMany', h, ids, vectors, dimension, metadata]);
      return ids.length;
    },
    remove: (h, id) => {
      calls.push(['remove', h, id]);
      return true;
    },
    contains: (h, id) => {
      calls.push(['contains', h, id]);
      return id === 'present';
    },
    count: (h) => {
      calls.push(['count', h]);
      return 7;
    },
    flush: (h) => calls.push(['flush', h]),
    search: (h, query, k, filter) => {
      calls.push(['search', h, query, k, filter]);
      return [{ id: 'a', score: 0.9 }];
    },
    stats: (h) => {
      calls.push(['stats', h]);
      return {
        liveDocuments: 7,
        totalRows: 9,
        segments: 1,
        bufferedDocuments: 0,
        deadRatio: 0.22,
        dimension: 4,
      };
    },
    compact: (h, ratio) => {
      calls.push(['compact', h, ratio]);
      return 2;
    },
    verify: (h, level) => {
      calls.push(['verify', h, level]);
      return { errors: 0, warnings: 1 };
    },
    ...overrides,
  };
  host.calls = calls;
  return host;
}

/** The last call the host recorded. */
export function lastCall(host, method) {
  const found = [...host.calls].reverse().find((c) => c[0] === method);
  if (!found) throw new Error(`the host was never asked to ${method}`);
  return found;
}
