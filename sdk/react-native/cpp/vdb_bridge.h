// The React Native bridge, with no React Native in it.
//
// # Why this file has no JSI
//
// A JSI `HostObject` needs React Native's headers, which live inside an app rather than in this
// repository. `scripts/test-react-native.sh` now fetches them and compiles `vdb_jsi.cpp`
// against the real thing, so that file no longer goes unchecked — but compiling is not running,
// and there is still no JavaScript engine here to run it in.
//
// So the logic stays here, in plain C++ over `vdb.h`, where it is compiled *and executed*
// against the real engine on every push: handle lifetimes, error translation, the rules about
// what may be called after close, the create-or-open fallback, and the postfix filter and
// metadata builders. `vdb_jsi.cpp` converts values and calls one method.
//
// The same split is why the web SDK's marshalling could be tested in Node while only the OPFS
// adapter needed a browser.

#pragma once

#include <cstdint>
#include <memory>
#include <string>
#include <vector>

namespace vdb {

/// What went wrong, carrying the engine's own structured code.
///
/// The code is the stable one from `docs/api/error-codes.md`, not a string an SDK invented. A caller
/// that wants to branch on "collection not found" can, in every language binding, on the same
/// number.
struct Error {
  bool ok = true;
  std::int32_t code = 0;
  std::string message;

  static Error success() { return Error{}; }
  static Error from(std::int32_t code, std::string message) {
    return Error{false, code, std::move(message)};
  }
};

/// One search result.
struct Hit {
  std::string id;
  float score = 0.0f;
};

/// A handle the JS side holds. Zero is never valid, so a closed handle is distinguishable from
/// an uninitialised one rather than being a plausible pointer.
using Handle = std::uint64_t;

/// One metadata field, as a tagged union flattened into a struct.
///
/// Flattened rather than a `variant` because this is what crosses from JavaScript, and a fixed
/// set of properties is what keeps the JSI layer free of branches: it reads the same six names
/// from every element and lets this layer — the tested one — decide what they mean.
struct MetaField {
  enum Kind : std::int32_t {
    kString = 1,
    kI64 = 2,
    kF64 = 3,
    kBool = 4,
    /// Explicitly null, which is not the same as absent: only an existence test can tell them
    /// apart, and dropping the key would silently change what that test reports.
    kNull = 5,
  };

  std::int32_t kind = kString;
  std::string key;
  std::string text;
  std::int64_t integer = 0;
  double real = 0.0;
  bool flag = false;
};

/// One step of a filter, in postfix order.
///
/// A filter is a tree, and neither C nor a JSI value conversion takes a tree well. The C ABI
/// receives one as a sequence of pushes and combines (see the filter section of `vdb.h`); this
/// is that sequence, built in JavaScript from the query object a caller actually writes and
/// replayed onto a `vdb_filter_t` here.
struct FilterOp {
  enum Kind : std::int32_t {
    kCompareString = 1,
    kCompareI64 = 2,
    kCompareF64 = 3,
    kCompareBool = 4,
    /// `EXISTS` or `IS_NULL`, in `op`.
    kUnary = 5,
    /// `AND`, `OR` or `NOT` in `op`, over the top `count` expressions.
    kCombine = 6,
  };

  std::int32_t kind = kCompareString;
  std::string field;
  /// A `vdb_op_t`, `vdb_unary_t` or `vdb_combine_t`, according to `kind`.
  std::int32_t op = 0;
  std::string text;
  std::int64_t integer = 0;
  double real = 0.0;
  bool flag = false;
  std::size_t count = 0;
};

/// Counters for a collection. Mirrors `vdb_stats_t`.
struct Stats {
  std::uint64_t live_documents = 0;
  std::uint64_t total_rows = 0;
  std::uint64_t segments = 0;
  std::uint64_t buffered_documents = 0;
  float dead_ratio = 0.0f;
  std::uint32_t dimension = 0;
};

/// What `verify` found. A damaged database is a result, not a failure to produce one.
struct VerifyReport {
  std::uint64_t errors = 0;
  std::uint64_t warnings = 0;
};

/// Everything the JSI layer is allowed to do.
///
/// Deliberately a small, flat surface of value types: no pointers cross into JSI code, and
/// nothing in the header below requires the caller to know the C ABI's ownership rules. Getting
/// those wrong is the failure mode this class exists to prevent.
class Bridge {
 public:
  Bridge();
  ~Bridge();

  Bridge(const Bridge&) = delete;
  Bridge& operator=(const Bridge&) = delete;

  /// Open or create a database at `path`, returning a handle in `out`.
  ///
  /// `durability` is a `vdb_durability_t`. It is passed through rather than fixed here so the
  /// JavaScript default and the engine's can be seen to be the same one.
  Error open(const std::string& path, bool create_if_missing, bool read_only,
             std::int32_t durability, Handle* out);

  /// Close a database. Closing one twice is an error rather than a crash.
  Error close(Handle db);

  /// Flush every collection in a database.
  Error flush_database(Handle db);

  /// Create a collection, or open it if it already exists with a matching specification.
  Error collection(Handle db, const std::string& name, std::uint32_t dimension,
                   std::int32_t metric, Handle* out);

  /// Open a collection that already exists. Unlike `collection`, this does not create one —
  /// which is what you want when a missing collection is a bug rather than a first run.
  Error open_collection(Handle db, const std::string& name, Handle* out);

  /// Delete a collection and everything in it. Irreversible.
  Error drop_collection(Handle db, const std::string& name);

  /// Release a collection handle. The database stays open.
  Error release_collection(Handle collection);

  /// Insert or replace a document. `vector` must have the collection's dimension.
  Error upsert(Handle collection, const std::string& id, const float* vector,
               std::uint32_t dimension, const std::vector<MetaField>& metadata, bool* inserted);

  /// Insert or replace `count` documents in one call.
  ///
  /// `vectors` is one contiguous block of `count * dimension` floats, which is the shape that
  /// makes a batch worth having: the whole batch crosses from JavaScript as a single buffer
  /// rather than as one JSI call and one `ArrayBuffer` lookup per document.
  ///
  /// `metadata` is either empty, meaning none of the documents have any, or exactly `count`
  /// entries. `inserted` receives how many documents were new rather than replaced.
  Error upsert_many(Handle collection, const std::vector<std::string>& ids, const float* vectors,
                    std::uint32_t dimension, std::size_t count,
                    const std::vector<std::vector<MetaField>>& metadata,
                    std::uint64_t* inserted);

  /// Remove a document, reporting whether it existed.
  Error remove(Handle collection, const std::string& id, bool* existed);

  /// Whether a document exists.
  Error contains(Handle collection, const std::string& id, bool* out);

  /// How many documents are live.
  Error count(Handle collection, std::uint64_t* out);

  /// Flush a collection's writes.
  Error flush(Handle collection);

  /// The `top_k` nearest documents, best first.
  ///
  /// An empty `filter` searches everything. A non-empty one must be a complete postfix
  /// sequence — one that leaves exactly one expression on the stack — and is refused rather
  /// than interpreted if it is not, because a filter quietly missing a clause returns documents
  /// the caller asked to exclude.
  ///
  /// `top_k` counts matches, not candidates: a filter excluding most of the collection still
  /// returns up to `top_k` results.
  Error search(Handle collection, const float* query, std::uint32_t dimension,
               std::size_t top_k, const std::vector<FilterOp>& filter, std::vector<Hit>* out);

  /// Counters for a collection, including the dead ratio that says whether to compact.
  Error stats(Handle collection, Stats* out);

  /// Reclaim the space held by tombstoned rows across every collection, reporting how many rows
  /// went. Explicit rather than automatic; see `vdb.h` for why.
  Error compact(Handle db, float min_dead_ratio, std::uint64_t* rows_reclaimed);

  /// Check integrity at a `vdb_verify_t` level. Reports rather than repairs, so a damaged
  /// database succeeds with a non-zero error count instead of throwing.
  Error verify(Handle db, std::int32_t level, VerifyReport* out);

  /// The library version string.
  static std::string version();

  /// The frozen C ABI version.
  static std::int32_t abi_version();

  /// The on-disk format this build writes.
  static std::uint32_t format_version();

  /// How many handles are outstanding.
  ///
  /// A JSI `HostObject` is destroyed by the JavaScript garbage collector, which is not
  /// deterministic and may never run before the app exits. Leaks therefore do not announce
  /// themselves; this is what lets a development build say "you opened three databases and
  /// closed one" instead of leaving it to be discovered in production.
  std::size_t live_handles() const;

 private:
  struct State;
  std::unique_ptr<State> state_;
};

}  // namespace vdb
