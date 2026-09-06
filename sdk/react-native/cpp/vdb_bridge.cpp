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

/// `ErrorCode::COLLECTION_ALREADY_EXISTS` from `docs/api/error-codes.md`.
///
/// The one code this layer branches on, so it is named rather than left as a literal in the
/// middle of a condition.
constexpr std::uint32_t COLLECTION_ALREADY_EXISTS = 4001;

const std::uint8_t* bytes(const std::string& s) {
  return reinterpret_cast<const std::uint8_t*>(s.data());
}

/// Build a `vdb_metadata_t` from the flattened fields that crossed from JavaScript.
///
/// Returns null on an empty list, which is what `vdb_upsert` wants for "no metadata" — so a
/// document with no fields costs no allocation. The caller owns anything non-null.
Error build_metadata(const std::vector<MetaField>& fields, vdb_metadata_t** out) {
  *out = nullptr;
  if (fields.empty()) {
    return Error::success();
  }
  vdb_metadata_t* m = vdb_metadata_new();
  if (m == nullptr) {
    return Error::from(VDB_INTERNAL, "could not allocate metadata");
  }
  for (const MetaField& field : fields) {
    vdb_error_t* err = nullptr;
    std::int32_t rc = VDB_OK;
    const std::uint8_t* key = bytes(field.key);
    const std::size_t key_len = field.key.size();
    switch (field.kind) {
      case MetaField::kString:
        rc = vdb_metadata_set_string(m, key, key_len, bytes(field.text), field.text.size(), &err);
        break;
      case MetaField::kI64:
        rc = vdb_metadata_set_i64(m, key, key_len, field.integer, &err);
        break;
      case MetaField::kF64:
        rc = vdb_metadata_set_f64(m, key, key_len, field.real, &err);
        break;
      case MetaField::kBool:
        rc = vdb_metadata_set_bool(m, key, key_len, field.flag, &err);
        break;
      case MetaField::kNull:
        rc = vdb_metadata_set_null(m, key, key_len, &err);
        break;
      default:
        vdb_metadata_free(m);
        return Error::from(VDB_INVALID_ARGUMENT,
                           "unknown metadata kind " + std::to_string(field.kind));
    }
    Error error = take(rc, err);
    if (!error.ok) {
      vdb_metadata_free(m);
      return error;
    }
  }
  *out = m;
  return Error::success();
}

/// Replay a postfix sequence onto a filter builder.
///
/// The completeness check is the point: the C ABI reports an unbalanced stack when the search
/// runs, by which time the error has travelled a long way from the filter that caused it. Doing
/// it here means the message can name the depth, and a filter that quietly lost a clause — which
/// returns documents the caller asked to exclude — cannot reach the engine at all.
Error build_filter(const std::vector<FilterOp>& ops, vdb_filter_t** out) {
  *out = nullptr;
  if (ops.empty()) {
    return Error::success();
  }
  vdb_filter_t* f = vdb_filter_new();
  if (f == nullptr) {
    return Error::from(VDB_INTERNAL, "could not allocate a filter");
  }
  for (const FilterOp& op : ops) {
    vdb_error_t* err = nullptr;
    std::int32_t rc = VDB_OK;
    const std::uint8_t* field = bytes(op.field);
    const std::size_t field_len = op.field.size();
    switch (op.kind) {
      case FilterOp::kCompareString:
        rc = vdb_filter_compare_str(f, field, field_len, op.op, bytes(op.text), op.text.size(),
                                    &err);
        break;
      case FilterOp::kCompareI64:
        rc = vdb_filter_compare_i64(f, field, field_len, op.op, op.integer, &err);
        break;
      case FilterOp::kCompareF64:
        rc = vdb_filter_compare_f64(f, field, field_len, op.op, op.real, &err);
        break;
      case FilterOp::kCompareBool:
        rc = vdb_filter_compare_bool(f, field, field_len, op.op, op.flag, &err);
        break;
      case FilterOp::kUnary:
        rc = vdb_filter_unary(f, field, field_len, op.op, &err);
        break;
      case FilterOp::kCombine:
        rc = vdb_filter_combine(f, op.op, op.count, &err);
        break;
      default:
        vdb_filter_free(f);
        return Error::from(VDB_INVALID_ARGUMENT,
                           "unknown filter step " + std::to_string(op.kind));
    }
    Error error = take(rc, err);
    if (!error.ok) {
      vdb_filter_free(f);
      return error;
    }
  }

  std::size_t depth = vdb_filter_depth(f);
  if (depth != 1) {
    vdb_filter_free(f);
    return Error::from(VDB_INVALID_ARGUMENT,
                       "this filter left " + std::to_string(depth) +
                           " expressions on the stack rather than 1; it is incomplete");
  }
  *out = f;
  return Error::success();
}

/// Copy the ids out of a result before it is freed.
///
/// The ids point into the engine's own memory and are invalid the moment `vdb_results_free`
/// runs, so this is a copy and not an optimisation waiting to be made.
void collect(vdb_results_t* results, std::vector<Hit>* out) {
  out->clear();
  std::size_t n = vdb_results_len(results);
  out->reserve(n);
  for (std::size_t i = 0; i < n; i++) {
    std::size_t len = 0;
    const std::uint8_t* id = vdb_results_id(results, i, &len);
    out->push_back(
        Hit{std::string(reinterpret_cast<const char*>(id), len), vdb_results_score(results, i)});
  }
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

  /// Look a handle up under the lock, so callers do not each write the same three lines and
  /// none of them can be the one that forgets to take it.
  vdb_db_t* locked_database(Handle h) const {
    std::lock_guard<std::mutex> lock(mutex);
    return database(h);
  }
  vdb_collection_t* locked_collection(Handle h) const {
    std::lock_guard<std::mutex> lock(mutex);
    return collection(h);
  }

  Handle record_collection(vdb_collection_t* collection) {
    std::lock_guard<std::mutex> lock(mutex);
    Handle handle = next++;
    collections[handle] = collection;
    return handle;
  }
};

namespace {

Error no_database() { return Error::from(VDB_INVALID_ARGUMENT, "this database is closed"); }
Error no_collection() {
  return Error::from(VDB_INVALID_ARGUMENT, "this collection is released");
}

}  // namespace

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

Error Bridge::open(const std::string& path, bool create_if_missing, bool read_only,
                   std::int32_t durability, Handle* out) {
  if (out == nullptr) {
    return Error::from(0, "no output handle");
  }
  vdb_db_t* db = nullptr;
  vdb_error_t* err = nullptr;
  std::int32_t rc =
      vdb_open(bytes(path), path.size(), create_if_missing, read_only, durability, &db, &err);
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

Error Bridge::flush_database(Handle db) {
  vdb_db_t* raw = state_->locked_database(db);
  if (raw == nullptr) {
    return no_database();
  }
  vdb_error_t* err = nullptr;
  return take(vdb_flush(raw, &err), err);
}

Error Bridge::collection(Handle db, const std::string& name, std::uint32_t dimension,
                         std::int32_t metric, Handle* out) {
  if (out == nullptr) {
    return Error::from(0, "no output handle");
  }
  vdb_db_t* raw = state_->locked_database(db);
  if (raw == nullptr) {
    return no_database();
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

  *out = state_->record_collection(collection);
  return Error::success();
}

Error Bridge::open_collection(Handle db, const std::string& name, Handle* out) {
  if (out == nullptr) {
    return Error::from(0, "no output handle");
  }
  vdb_db_t* raw = state_->locked_database(db);
  if (raw == nullptr) {
    return no_database();
  }
  vdb_collection_t* collection = nullptr;
  vdb_error_t* err = nullptr;
  Error error = take(vdb_collection_open(raw, bytes(name), name.size(), &collection, &err), err);
  if (!error.ok) {
    return error;
  }
  *out = state_->record_collection(collection);
  return Error::success();
}

Error Bridge::drop_collection(Handle db, const std::string& name) {
  vdb_db_t* raw = state_->locked_database(db);
  if (raw == nullptr) {
    return no_database();
  }
  vdb_error_t* err = nullptr;
  return take(vdb_collection_drop(raw, bytes(name), name.size(), &err), err);
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
                     std::uint32_t dimension, const std::vector<MetaField>& metadata,
                     bool* inserted) {
  vdb_collection_t* raw = state_->locked_collection(collection);
  if (raw == nullptr) {
    return no_collection();
  }
  vdb_metadata_t* m = nullptr;
  Error built = build_metadata(metadata, &m);
  if (!built.ok) {
    return built;
  }
  bool did = false;
  vdb_error_t* err = nullptr;
  std::int32_t rc = vdb_upsert(raw, bytes(id), id.size(), vector, dimension, m, &did, &err);
  if (m != nullptr) {
    // The document took a copy; the builder was only ever ours.
    vdb_metadata_free(m);
  }
  Error error = take(rc, err);
  if (error.ok && inserted != nullptr) {
    *inserted = did;
  }
  return error;
}

Error Bridge::upsert_many(Handle collection, const std::vector<std::string>& ids,
                          const float* vectors, std::uint32_t dimension, std::size_t count,
                          const std::vector<std::vector<MetaField>>& metadata,
                          std::uint64_t* inserted) {
  if (ids.size() != count) {
    return Error::from(VDB_INVALID_ARGUMENT, "the batch has " + std::to_string(ids.size()) +
                                                 " ids for " + std::to_string(count) +
                                                 " documents");
  }
  if (!metadata.empty() && metadata.size() != count) {
    return Error::from(VDB_INVALID_ARGUMENT,
                       "the batch has metadata for " + std::to_string(metadata.size()) +
                           " of " + std::to_string(count) +
                           " documents; give every document metadata or none of them");
  }
  if (count > 0 && vectors == nullptr) {
    return Error::from(VDB_INVALID_ARGUMENT, "the batch has no vectors");
  }
  vdb_collection_t* raw = state_->locked_collection(collection);
  if (raw == nullptr) {
    return no_collection();
  }

  // Not a transaction. The C ABI has no batch call, so this is a loop, and a failure part-way
  // through leaves the documents before it written — which is why the error says where it
  // stopped rather than implying nothing happened.
  std::uint64_t new_documents = 0;
  for (std::size_t i = 0; i < count; i++) {
    vdb_metadata_t* m = nullptr;
    if (!metadata.empty()) {
      Error built = build_metadata(metadata[i], &m);
      if (!built.ok) {
        return Error::from(built.code, "document " + std::to_string(i) + " (" + ids[i] +
                                           "): " + built.message);
      }
    }
    bool did = false;
    vdb_error_t* err = nullptr;
    std::int32_t rc = vdb_upsert(raw, bytes(ids[i]), ids[i].size(), vectors + i * dimension,
                                 dimension, m, &did, &err);
    if (m != nullptr) {
      vdb_metadata_free(m);
    }
    Error error = take(rc, err);
    if (!error.ok) {
      return Error::from(error.code, "document " + std::to_string(i) + " (" + ids[i] + ") of " +
                                         std::to_string(count) + ": " + error.message +
                                         " — the documents before it were written");
    }
    if (did) {
      new_documents++;
    }
  }
  if (inserted != nullptr) {
    *inserted = new_documents;
  }
  return Error::success();
}

Error Bridge::remove(Handle collection, const std::string& id, bool* existed) {
  vdb_collection_t* raw = state_->locked_collection(collection);
  if (raw == nullptr) {
    return no_collection();
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
  vdb_collection_t* raw = state_->locked_collection(collection);
  if (raw == nullptr) {
    return no_collection();
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
  vdb_collection_t* raw = state_->locked_collection(collection);
  if (raw == nullptr) {
    return no_collection();
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
  vdb_collection_t* raw = state_->locked_collection(collection);
  if (raw == nullptr) {
    return no_collection();
  }
  vdb_error_t* err = nullptr;
  return take(vdb_collection_flush(raw, &err), err);
}

Error Bridge::search(Handle collection, const float* query, std::uint32_t dimension,
                     std::size_t top_k, const std::vector<FilterOp>& filter,
                     std::vector<Hit>* out) {
  if (out == nullptr) {
    return Error::from(0, "no output vector");
  }
  vdb_collection_t* raw = state_->locked_collection(collection);
  if (raw == nullptr) {
    return no_collection();
  }

  vdb_filter_t* f = nullptr;
  Error built = build_filter(filter, &f);
  if (!built.ok) {
    return built;
  }

  vdb_results_t* results = nullptr;
  vdb_error_t* err = nullptr;
  std::int32_t rc =
      f == nullptr
          ? vdb_search(raw, query, dimension, top_k, &results, &err)
          : vdb_search_filtered(raw, query, dimension, top_k, f, &results, &err);
  if (f != nullptr) {
    vdb_filter_free(f);
  }

  Error error = take(rc, err);
  if (!error.ok) {
    return error;
  }
  collect(results, out);
  vdb_results_free(results);
  return Error::success();
}

Error Bridge::stats(Handle collection, Stats* out) {
  if (out == nullptr) {
    return Error::from(0, "no output struct");
  }
  vdb_collection_t* raw = state_->locked_collection(collection);
  if (raw == nullptr) {
    return no_collection();
  }
  vdb_stats_t s = {};
  vdb_error_t* err = nullptr;
  Error error = take(vdb_collection_stats(raw, &s, &err), err);
  if (!error.ok) {
    return error;
  }
  out->live_documents = s.live_documents;
  out->total_rows = s.total_rows;
  out->segments = s.segments;
  out->buffered_documents = s.buffered_documents;
  out->dead_ratio = s.dead_ratio;
  out->dimension = s.dimension;
  return Error::success();
}

Error Bridge::compact(Handle db, float min_dead_ratio, std::uint64_t* rows_reclaimed) {
  vdb_db_t* raw = state_->locked_database(db);
  if (raw == nullptr) {
    return no_database();
  }
  std::uint64_t reclaimed = 0;
  vdb_error_t* err = nullptr;
  Error error = take(vdb_compact(raw, min_dead_ratio, &reclaimed, &err), err);
  if (error.ok && rows_reclaimed != nullptr) {
    *rows_reclaimed = reclaimed;
  }
  return error;
}

Error Bridge::verify(Handle db, std::int32_t level, VerifyReport* out) {
  if (out == nullptr) {
    return Error::from(0, "no output struct");
  }
  vdb_db_t* raw = state_->locked_database(db);
  if (raw == nullptr) {
    return no_database();
  }
  std::uint64_t errors = 0;
  std::uint64_t warnings = 0;
  vdb_error_t* err = nullptr;
  // A damaged database is a result, not a throw: `rc` is non-zero only when verification could
  // not run at all, and the counts are what the caller acts on.
  Error error = take(vdb_verify(raw, level, &errors, &warnings, &err), err);
  if (!error.ok) {
    return error;
  }
  out->errors = errors;
  out->warnings = warnings;
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
