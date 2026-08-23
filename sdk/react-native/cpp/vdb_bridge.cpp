#include "vdb_bridge.h"

#include <map>
#include <mutex>

extern "C" {
#include "vdb.h"
}

namespace vdb {
namespace {

/// Turn a C ABI failure into a structured error, freeing the error object either way.
///
/// Every entry point in the C ABI reports through a `vdb_error_t**` out-parameter, and the
/// caller owns what lands there. Doing that in one place is what keeps the methods below free of
/// the same six lines, and means an early return cannot leak.
Error take(std::int32_t rc, vdb_error_t* err) {
  if (rc == 0 && err == nullptr) {
    return Error::success();
  }
  std::int32_t code = rc;
  std::string message;
  if (err != nullptr) {
    code = static_cast<std::int32_t>(vdb_error_code(err));
    const char* text = vdb_error_message(err);
    message = text != nullptr ? text : "";
    vdb_error_free(err);
  }
  if (message.empty()) {
    message = "vdb error " + std::to_string(rc);
  }
  return Error::from(code, std::move(message));
}

/// `ErrorCode::COLLECTION_ALREADY_EXISTS` from `docs/api/errors.md`.
///
/// The one code this layer branches on, so it is named rather than left as a literal in the
/// middle of a condition.
constexpr std::uint32_t COLLECTION_ALREADY_EXISTS = 4001;

const std::uint8_t* bytes(const std::string& s) {
  return reinterpret_cast<const std::uint8_t*>(s.data());
}

}  // namespace

/// Handles, and what they point at.
///
/// The JS side gets integers rather than pointers. A stale or forged handle then fails a map
/// lookup and returns an error, where a raw pointer would be dereferenced — and a use-after-free
/// reached from JavaScript is exactly the class of bug a database must not have.
struct Bridge::State {
  mutable std::mutex mutex;
  Handle next = 1;
  std::map<Handle, vdb_db_t*> databases;
  std::map<Handle, vdb_collection_t*> collections;

  vdb_db_t* database(Handle h) const {
    auto it = databases.find(h);
    return it == databases.end() ? nullptr : it->second;
  }
  vdb_collection_t* collection(Handle h) const {
    auto it = collections.find(h);
    return it == collections.end() ? nullptr : it->second;
  }
};

Bridge::Bridge() : state_(std::make_unique<State>()) {}

Bridge::~Bridge() {
  // Whatever the application forgot. A database left open holds its lock file, and on a mobile
  // platform the next launch would find it held by a process that no longer exists.
  std::lock_guard<std::mutex> lock(state_->mutex);
  for (auto& [handle, collection] : state_->collections) {
    (void)handle;
    vdb_collection_free(collection);
  }
  for (auto& [handle, db] : state_->databases) {
    (void)handle;
    vdb_error_t* err = nullptr;
    vdb_close(db, &err);
    if (err != nullptr) {
      vdb_error_free(err);
    }
  }
}

Error Bridge::open(const std::string& path, bool create_if_missing, bool read_only, Handle* out) {
  if (out == nullptr) {
    return Error::from(0, "no output handle");
  }
  vdb_db_t* db = nullptr;
  vdb_error_t* err = nullptr;
  std::int32_t rc = vdb_open(bytes(path), path.size(), create_if_missing, read_only,
                             VDB_DURABILITY_BATCH, &db, &err);
  Error error = take(rc, err);
  if (!error.ok) {
    return error;
  }
  std::lock_guard<std::mutex> lock(state_->mutex);
  *out = state_->next++;
  state_->databases[*out] = db;
  return Error::success();
}

Error Bridge::close(Handle db) {
  vdb_db_t* raw = nullptr;
  {
    std::lock_guard<std::mutex> lock(state_->mutex);
    auto it = state_->databases.find(db);
    if (it == state_->databases.end()) {
      // Closing twice is a bug in the caller, and saying so is more useful than succeeding
      // quietly and leaving them to wonder why the next call fails.
      return Error::from(VDB_INVALID_ARGUMENT, "this database is already closed");
    }
    raw = it->second;
    state_->databases.erase(it);
  }
  vdb_error_t* err = nullptr;
  return take(vdb_close(raw, &err), err);
}

Error Bridge::collection(Handle db, const std::string& name, std::uint32_t dimension,
                         std::int32_t metric, Handle* out) {
  if (out == nullptr) {
    return Error::from(0, "no output handle");
  }
  vdb_db_t* raw = nullptr;
  {
    std::lock_guard<std::mutex> lock(state_->mutex);
    raw = state_->database(db);
  }
  if (raw == nullptr) {
    return Error::from(VDB_INVALID_ARGUMENT, "this database is closed");
  }

  vdb_collection_t* collection = nullptr;
  vdb_error_t* err = nullptr;
  std::int32_t rc = vdb_collection_create(raw, bytes(name), name.size(), dimension, metric,
                                          false, &collection, &err);
  if (rc != 0) {
    // Falling back only on "already exists". Retrying on any failure would replace a precise
    // diagnosis — a zero dimension, an unknown metric — with the misleading "collection not
    // found" that an open would then produce.
    bool exists = err != nullptr && vdb_error_code(err) == COLLECTION_ALREADY_EXISTS;
    if (!exists) {
      // Consumed here rather than freed and re-reported, because `take(rc, nullptr)` has nothing
      // left to read and produces a bare "vdb error 4013" — the code without the sentence that
      // says what to do about it. The engine's message is the thing worth keeping.
      return take(rc, err);
    }
    vdb_error_free(err);
    err = nullptr;
    rc = vdb_collection_open(raw, bytes(name), name.size(), &collection, &err);
    Error error = take(rc, err);
    if (!error.ok) {
      return error;
    }
  }

  std::lock_guard<std::mutex> lock(state_->mutex);
  *out = state_->next++;
  state_->collections[*out] = collection;
  return Error::success();
}

Error Bridge::release_collection(Handle collection) {
  std::lock_guard<std::mutex> lock(state_->mutex);
  auto it = state_->collections.find(collection);
  if (it == state_->collections.end()) {
    return Error::from(VDB_INVALID_ARGUMENT, "this collection is already released");
  }
  vdb_collection_free(it->second);
  state_->collections.erase(it);
  return Error::success();
}

Error Bridge::upsert(Handle collection, const std::string& id, const float* vector,
                     std::uint32_t dimension, bool* inserted) {
  vdb_collection_t* raw = nullptr;
  {
    std::lock_guard<std::mutex> lock(state_->mutex);
    raw = state_->collection(collection);
  }
  if (raw == nullptr) {
    return Error::from(VDB_INVALID_ARGUMENT, "this collection is released");
  }
  bool did = false;
  vdb_error_t* err = nullptr;
  std::int32_t rc = vdb_upsert(raw, bytes(id), id.size(), vector, dimension, nullptr, &did, &err);
  Error error = take(rc, err);
  if (error.ok && inserted != nullptr) {
    *inserted = did;
  }
  return error;
}

Error Bridge::remove(Handle collection, const std::string& id, bool* existed) {
  vdb_collection_t* raw = nullptr;
  {
    std::lock_guard<std::mutex> lock(state_->mutex);
    raw = state_->collection(collection);
  }
  if (raw == nullptr) {
    return Error::from(VDB_INVALID_ARGUMENT, "this collection is released");
  }
  bool did = false;
  vdb_error_t* err = nullptr;
  Error error = take(vdb_delete(raw, bytes(id), id.size(), &did, &err), err);
  if (error.ok && existed != nullptr) {
    *existed = did;
  }
  return error;
}

Error Bridge::contains(Handle collection, const std::string& id, bool* out) {
  vdb_collection_t* raw = nullptr;
  {
    std::lock_guard<std::mutex> lock(state_->mutex);
    raw = state_->collection(collection);
  }
  if (raw == nullptr) {
    return Error::from(VDB_INVALID_ARGUMENT, "this collection is released");
  }
  bool found = false;
  vdb_error_t* err = nullptr;
  Error error = take(vdb_contains(raw, bytes(id), id.size(), &found, &err), err);
  if (error.ok && out != nullptr) {
    *out = found;
  }
  return error;
}

Error Bridge::count(Handle collection, std::uint64_t* out) {
  vdb_collection_t* raw = nullptr;
  {
    std::lock_guard<std::mutex> lock(state_->mutex);
    raw = state_->collection(collection);
  }
  if (raw == nullptr) {
    return Error::from(VDB_INVALID_ARGUMENT, "this collection is released");
  }
  std::uint64_t value = 0;
  vdb_error_t* err = nullptr;
  Error error = take(vdb_collection_count(raw, &value, &err), err);
  if (error.ok && out != nullptr) {
    *out = value;
  }
  return error;
}

Error Bridge::flush(Handle collection) {
  vdb_collection_t* raw = nullptr;
  {
    std::lock_guard<std::mutex> lock(state_->mutex);
    raw = state_->collection(collection);
  }
  if (raw == nullptr) {
    return Error::from(VDB_INVALID_ARGUMENT, "this collection is released");
  }
  vdb_error_t* err = nullptr;
  return take(vdb_collection_flush(raw, &err), err);
}

Error Bridge::search(Handle collection, const float* query, std::uint32_t dimension,
                     std::size_t top_k, std::vector<Hit>* out) {
  if (out == nullptr) {
    return Error::from(0, "no output vector");
  }
  vdb_collection_t* raw = nullptr;
  {
    std::lock_guard<std::mutex> lock(state_->mutex);
    raw = state_->collection(collection);
  }
  if (raw == nullptr) {
    return Error::from(VDB_INVALID_ARGUMENT, "this collection is released");
  }

  vdb_results_t* results = nullptr;
  vdb_error_t* err = nullptr;
  Error error = take(vdb_search(raw, query, dimension, top_k, &results, &err), err);
  if (!error.ok) {
    return error;
  }

  out->clear();
  std::size_t n = vdb_results_len(results);
  out->reserve(n);
  for (std::size_t i = 0; i < n; i++) {
    std::size_t len = 0;
    const std::uint8_t* id = vdb_results_id(results, i, &len);
    // Copied before the results are freed. The ids point into the engine's own memory and are
    // invalid the moment `vdb_results_free` runs.
    out->push_back(Hit{std::string(reinterpret_cast<const char*>(id), len),
                       vdb_results_score(results, i)});
  }
  vdb_results_free(results);
  return Error::success();
}

std::string Bridge::version() {
  const char* v = vdb_version();
  return v != nullptr ? v : "";
}

std::int32_t Bridge::abi_version() { return vdb_abi_version(); }

std::uint32_t Bridge::format_version() { return vdb_format_version(); }

std::size_t Bridge::live_handles() const {
  std::lock_guard<std::mutex> lock(state_->mutex);
  return state_->databases.size() + state_->collections.size();
}

}  // namespace vdb
