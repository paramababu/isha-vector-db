'use strict';

/**
 * @isha-vector-db/node — an embedded, offline-first vector database.
 *
 * A thin layer over the native addon. It exists for two reasons, both of which would otherwise
 * have to be solved four times over in four bindings:
 *
 *   1. `Database.open(...)` reads better than `new Database(...)` for something that touches a
 *      disk and can fail. The addon exposes a free function because napi-rs classes cannot have
 *      a fallible constructor; this is where that becomes idiomatic JavaScript again.
 *
 *   2. `using` / `Symbol.dispose` support, so a database is closed even when an exception
 *      unwinds past the code that would have closed it. Forgetting to close leaves the lock
 *      held until the process exits, and every application forgets eventually.
 *
 * Everything here is synchronous, because the engine is. Wrap calls in a worker if a large
 * search would otherwise block your event loop — the decision belongs to the application, which
 * knows how big its collections are.
 */

const native = require('./vdb.node');

/**
 * The engine's stable numeric code, recovered from the message.
 *
 * napi-rs sets `error.code` to its own status string — `GenericFailure` for everything — so
 * without this Node is the only binding where a caller cannot branch on the engine's code, and
 * would have to match on English instead. Every other binding exposes the number.
 *
 * Parsing the message is not something a caller should ever do, and is acceptable here for one
 * reason: both ends are ours. The `[VDB-nnnn]` prefix is produced by `ErrorCode`'s `Display`,
 * whose format is part of the contract and covered by tests in `isha-vector-db-core`.
 */
function attachCode(error) {
  if (!(error instanceof Error)) return error;
  const match = /^\[VDB-(\d{4})\]/.exec(error.message ?? '');
  if (match) {
    error.code = Number(match[1]);
  }
  return error;
}

/** Run a native call, giving anything it throws the engine's numeric code. */
function withCode(fn) {
  try {
    return fn();
  } catch (e) {
    throw attachCode(e);
  }
}

/**
 * Wrap every method of a native object so its errors carry the code.
 *
 * The addon's objects are native classes, so the methods are on the prototype and there is
 * nothing to iterate at instance level; this walks the prototype chain once per object.
 */
function withCodes(target) {
  const prototype = Object.getPrototypeOf(target);
  for (const name of Object.getOwnPropertyNames(prototype)) {
    if (name === 'constructor') continue;
    const value = target[name];
    if (typeof value !== 'function') continue;
    Object.defineProperty(target, name, {
      configurable: true,
      writable: true,
      value: (...args) => {
        const result = withCode(() => value.apply(target, args));
        // A collection comes back from `db.collection(...)`; it needs the same treatment.
        return result !== null && typeof result === 'object' && 'upsert' in result
          ? withCodes(result)
          : result;
      },
    });
  }
  return target;
}

/** Open or create a database at a directory path. */
function open(path, options) {
  const db = withCodes(withCode(() => native.openDatabase(path, options ?? {})));
  // Closing an already-closed database is a no-op in the addon, so a `finally` that closes a
  // database an earlier `close()` already handled does not throw.
  db[Symbol.dispose] = () => db.close();
  return db;
}

module.exports = {
  open,
  Database: native.Database,
  Collection: native.Collection,
};
