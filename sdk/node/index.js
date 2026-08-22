'use strict';

/**
 * @vdb/node — an embedded, offline-first vector database.
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

/** Open or create a database at a directory path. */
function open(path, options) {
  const db = native.openDatabase(path, options ?? {});
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
