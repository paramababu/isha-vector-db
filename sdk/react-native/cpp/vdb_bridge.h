// The React Native bridge, with no React Native in it.
//
// # Why this file has no JSI
//
// A JSI `HostObject` cannot be compiled or run outside a React Native app, so anything written
// inside one is unverifiable until it is on a device. That is a bad place to put logic — handle
// lifetimes, error translation, buffer marshalling, the rules about what may be called after
// close.
//
// So all of that lives here, in plain C++ over `vdb.h`, and is compiled and tested by
// `scripts/test-react-native.sh` on the development machine. `vdb_jsi.cpp` is the only file that
// touches JSI, and it is deliberately thin enough to read in one sitting: it converts JS values
// to the types below, calls one method, and converts back.
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
/// The code is the stable one from `docs/api/errors.md`, not a string an SDK invented. A caller
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
  Error open(const std::string& path, bool create_if_missing, bool read_only, Handle* out);

  /// Close a database. Closing one twice is an error rather than a crash.
  Error close(Handle db);

  /// Create a collection, or open it if it already exists with a matching specification.
  Error collection(Handle db, const std::string& name, std::uint32_t dimension,
                   std::int32_t metric, Handle* out);

  /// Release a collection handle. The database stays open.
  Error release_collection(Handle collection);

  /// Insert or replace a document. `vector` must have the collection's dimension.
  Error upsert(Handle collection, const std::string& id, const float* vector,
               std::uint32_t dimension, bool* inserted);

  /// Remove a document, reporting whether it existed.
  Error remove(Handle collection, const std::string& id, bool* existed);

  /// Whether a document exists.
  Error contains(Handle collection, const std::string& id, bool* out);

  /// How many documents are live.
  Error count(Handle collection, std::uint64_t* out);

  /// Flush a collection's writes.
  Error flush(Handle collection);

  /// The `top_k` nearest documents, best first.
  Error search(Handle collection, const float* query, std::uint32_t dimension,
               std::size_t top_k, std::vector<Hit>* out);

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
