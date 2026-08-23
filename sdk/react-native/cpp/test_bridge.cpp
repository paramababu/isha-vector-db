// The React Native bridge, exercised against the real engine.
//
// This is the half of the native layer that can be run on a development machine, and it is
// deliberately the half that holds the logic: handle lifetimes, error translation, use-after-
// close, the create-or-open fallback. `vdb_jsi.cpp` is what is left over, and it only converts
// values.
//
// Built and run by `scripts/test-react-native.sh`.

#include "vdb_bridge.h"

#include <unistd.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

extern "C" {
// For the metric and error-code constants. The bridge header deliberately does not re-export
// them: the C ABI is the contract, and a second copy of these values would be a second thing to
// keep in step.
#include "vdb.h"
}

namespace {

int failures = 0;
int checks = 0;

void check(bool ok, const std::string& what) {
  checks++;
  if (ok) {
    std::printf("  ok   %s\n", what.c_str());
  } else {
    failures++;
    std::printf("  FAIL %s\n", what.c_str());
  }
}

/// A temporary directory that removes itself.
class Scratch {
 public:
  Scratch() {
    char pattern[] = "/tmp/vdb-rn-XXXXXX";
    const char* made = mkdtemp(pattern);
    path_ = made != nullptr ? made : "/tmp/vdb-rn-fallback";
  }
  ~Scratch() {
    std::string command = "rm -rf '" + path_ + "'";
    if (std::system(command.c_str()) != 0) {
      std::printf("  note: could not remove %s\n", path_.c_str());
    }
  }
  const std::string& path() const { return path_; }

 private:
  std::string path_;
};

std::vector<float> vector_for(int i, std::uint32_t dimension) {
  std::vector<float> v(dimension, 1.0f);
  v[0] = static_cast<float>(i);
  return v;
}

void versions() {
  std::printf("versions\n");
  check(!vdb::Bridge::version().empty(), "the library reports a version");
  // The ABI is frozen at 1; the on-disk format moves independently of it.
  check(vdb::Bridge::abi_version() == 1, "the C ABI version is 1");
  check(vdb::Bridge::format_version() >= 1, "the format version is at least 1");
}

void lifecycle() {
  std::printf("lifecycle\n");
  Scratch dir;
  vdb::Bridge bridge;

  vdb::Handle db = 0;
  vdb::Error e = bridge.open(dir.path(), true, false, &db);
  check(e.ok, "opened a database: " + e.message);
  check(db != 0, "a handle is never zero");

  vdb::Handle docs = 0;
  e = bridge.collection(db, "docs", 4, VDB_METRIC_COSINE, &docs);
  check(e.ok, "created a collection: " + e.message);

  // Asking again must open the existing one rather than failing.
  vdb::Handle again = 0;
  e = bridge.collection(db, "docs", 4, VDB_METRIC_COSINE, &again);
  check(e.ok, "opened the same collection again: " + e.message);
  check(again != docs, "the second handle is distinct");
  check(bridge.release_collection(again).ok, "released the duplicate handle");

  bool inserted = false;
  for (int i = 0; i < 8; i++) {
    std::vector<float> v = vector_for(i, 4);
    e = bridge.upsert(docs, "doc-" + std::to_string(i), v.data(), 4, &inserted);
    if (!e.ok) break;
  }
  check(e.ok, "inserted eight documents: " + e.message);
  check(inserted, "the last insert reported a new document");

  std::uint64_t count = 0;
  check(bridge.count(docs, &count).ok && count == 8, "count is eight");

  bool exists = false;
  check(bridge.contains(docs, "doc-3", &exists).ok && exists, "doc-3 exists");
  check(bridge.contains(docs, "nope", &exists).ok && !exists, "an absent id reports false");

  std::vector<vdb::Hit> hits;
  std::vector<float> query = vector_for(7, 4);
  e = bridge.search(docs, query.data(), 4, 3, &hits);
  check(e.ok, "searched: " + e.message);
  check(hits.size() == 3, "three hits");
  check(!hits.empty() && hits[0].id == "doc-7", "the nearest is doc-7");
  check(hits.size() >= 2 && hits[0].score >= hits[1].score, "scores descend");

  bool existed = false;
  check(bridge.remove(docs, "doc-0", &existed).ok && existed, "deleted doc-0");
  check(bridge.remove(docs, "doc-0", &existed).ok && !existed,
        "deleting an absent document succeeds and reports false");

  check(bridge.flush(docs).ok, "flushed");
  check(bridge.release_collection(docs).ok, "released the collection");
  check(bridge.close(db).ok, "closed the database");
  check(bridge.live_handles() == 0, "no handles are outstanding");
}

/// Everything a JavaScript caller can get wrong must produce an error, not a crash.
///
/// This is the whole reason handles are integers rather than pointers: a stale one fails a map
/// lookup, where a raw pointer would be dereferenced. A use-after-free reachable from JS is the
/// class of bug a database must not have.
void misuse() {
  std::printf("misuse\n");
  Scratch dir;
  vdb::Bridge bridge;

  vdb::Handle db = 0;
  check(bridge.open(dir.path(), true, false, &db).ok, "opened");
  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 3, VDB_METRIC_COSINE, &docs).ok, "collection");

  check(bridge.close(db).ok, "closed once");
  vdb::Error twice = bridge.close(db);
  check(!twice.ok, "closing twice is an error, not a crash");
  check(twice.message.find("already closed") != std::string::npos,
        "the message says what happened: " + twice.message);

  // The collection handle outlived its database. It must fail rather than reach freed memory.
  std::uint64_t count = 0;
  vdb::Error stale = bridge.count(docs, &count);
  check(!stale.ok || count == 0, "a collection of a closed database does not crash");

  vdb::Error bogus = bridge.count(999999, &count);
  check(!bogus.ok, "a handle that was never issued is rejected");

  vdb::Error released = bridge.release_collection(999999);
  check(!released.ok, "releasing an unknown collection is rejected");
}

/// The engine's own message must survive the trip, not be flattened into "upsert failed".
void errors_keep_their_message() {
  std::printf("errors\n");
  Scratch dir;
  vdb::Bridge bridge;

  vdb::Handle db = 0;
  check(bridge.open(dir.path(), true, false, &db).ok, "opened");
  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 3, VDB_METRIC_COSINE, &docs).ok, "collection");

  std::vector<float> wrong(2, 1.0f);
  vdb::Error e = bridge.upsert(docs, "bad", wrong.data(), 2, nullptr);
  check(!e.ok, "a wrong dimension is rejected");
  check(e.message.find("3-dimensional") != std::string::npos,
        "the engine's own message reaches the caller: " + e.message);
  check(e.code > 0, "the structured code survives too");

  // A collection that does not exist, asked for with a bad specification: the create failure
  // must be reported, not replaced by "not found" from a fallback open.
  vdb::Handle bad = 0;
  vdb::Error spec = bridge.collection(db, "zero", 0, VDB_METRIC_COSINE, &bad);
  check(!spec.ok, "a zero dimension is rejected");
  check(spec.message.find("not found") == std::string::npos,
        "the real reason is reported, not a misleading fallback: " + spec.message);

  check(bridge.release_collection(docs).ok, "released");
  check(bridge.close(db).ok, "closed");
}

/// Data written through the bridge must still be there after a reopen.
void persistence() {
  std::printf("persistence\n");
  Scratch dir;

  {
    vdb::Bridge bridge;
    vdb::Handle db = 0;
    check(bridge.open(dir.path(), true, false, &db).ok, "opened");
    vdb::Handle docs = 0;
    check(bridge.collection(db, "docs", 4, VDB_METRIC_COSINE, &docs).ok, "collection");
    for (int i = 0; i < 5; i++) {
      std::vector<float> v = vector_for(i, 4);
      bridge.upsert(docs, "doc-" + std::to_string(i), v.data(), 4, nullptr);
    }
    check(bridge.flush(docs).ok, "flushed");
    check(bridge.release_collection(docs).ok, "released");
    check(bridge.close(db).ok, "closed");
  }

  vdb::Bridge bridge;
  vdb::Handle db = 0;
  check(bridge.open(dir.path(), false, false, &db).ok, "reopened without creating");
  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 4, VDB_METRIC_COSINE, &docs).ok, "reopened the collection");
  std::uint64_t count = 0;
  check(bridge.count(docs, &count).ok && count == 5, "all five documents survived");
  check(bridge.release_collection(docs).ok, "released");
  check(bridge.close(db).ok, "closed");
}

/// A destroyed bridge must release whatever the application forgot.
///
/// The JS garbage collector destroys a HostObject at a time of its choosing, and may never run
/// before the app exits. A database left open holds its lock file, and on a phone the next
/// launch finds it held by a process that no longer exists.
void the_destructor_cleans_up() {
  std::printf("cleanup\n");
  Scratch dir;
  {
    vdb::Bridge bridge;
    vdb::Handle db = 0;
    check(bridge.open(dir.path(), true, false, &db).ok, "opened and deliberately not closed");
    vdb::Handle docs = 0;
    check(bridge.collection(db, "docs", 3, VDB_METRIC_COSINE, &docs).ok, "collection");
    check(bridge.live_handles() == 2, "two handles outstanding");
  }

  // If the destructor did not release the lock, this open fails.
  vdb::Bridge second;
  vdb::Handle db = 0;
  vdb::Error e = second.open(dir.path(), false, false, &db);
  check(e.ok, "the abandoned database could be reopened: " + e.message);
  check(second.close(db).ok, "closed");
}

}  // namespace

int main() {
  versions();
  lifecycle();
  misuse();
  errors_keep_their_message();
  persistence();
  the_destructor_cleans_up();

  std::printf("\n%d checks, %d failures\n", checks, failures);
  if (failures == 0) {
    std::printf("all checks passed\n");
  }
  return failures == 0 ? 0 : 1;
}
