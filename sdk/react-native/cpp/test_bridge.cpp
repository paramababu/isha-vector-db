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
  vdb::Error e = bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db);
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
    e = bridge.upsert(docs, "doc-" + std::to_string(i), v.data(), 4, {}, &inserted);
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
  e = bridge.search(docs, query.data(), 4, 3, {}, &hits);
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
  check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened");
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
  check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened");
  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 3, VDB_METRIC_COSINE, &docs).ok, "collection");

  std::vector<float> wrong(2, 1.0f);
  vdb::Error e = bridge.upsert(docs, "bad", wrong.data(), 2, {}, nullptr);
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
    check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened");
    vdb::Handle docs = 0;
    check(bridge.collection(db, "docs", 4, VDB_METRIC_COSINE, &docs).ok, "collection");
    for (int i = 0; i < 5; i++) {
      std::vector<float> v = vector_for(i, 4);
      bridge.upsert(docs, "doc-" + std::to_string(i), v.data(), 4, {}, nullptr);
    }
    check(bridge.flush(docs).ok, "flushed");
    check(bridge.release_collection(docs).ok, "released");
    check(bridge.close(db).ok, "closed");
  }

  vdb::Bridge bridge;
  vdb::Handle db = 0;
  check(bridge.open(dir.path(), false, false, VDB_DURABILITY_BATCH, &db).ok, "reopened without creating");
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
    check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened and deliberately not closed");
    vdb::Handle docs = 0;
    check(bridge.collection(db, "docs", 3, VDB_METRIC_COSINE, &docs).ok, "collection");
    check(bridge.live_handles() == 2, "two handles outstanding");
  }

  // If the destructor did not release the lock, this open fails.
  vdb::Bridge second;
  vdb::Handle db = 0;
  vdb::Error e = second.open(dir.path(), false, false, VDB_DURABILITY_BATCH, &db);
  check(e.ok, "the abandoned database could be reopened: " + e.message);
  check(second.close(db).ok, "closed");
}


/// Metadata written through the bridge must be visible to a filter.
///
/// The two halves are tested together deliberately: a metadata builder that writes the wrong
/// type and a filter that reads the wrong type agree with each other and disagree with the
/// engine, and testing either alone would not notice.
void metadata_and_filters() {
  std::printf("metadata and filters\n");
  Scratch dir;
  vdb::Bridge bridge;

  vdb::Handle db = 0;
  check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened");
  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 2, VDB_METRIC_COSINE, &docs).ok, "collection");

  auto field = [](std::int32_t kind, const std::string& key) {
    vdb::MetaField f;
    f.kind = kind;
    f.key = key;
    return f;
  };

  // hammer: tools, 25, in stock.  saw: tools, 75.  ball: toys, no price.
  std::vector<vdb::MetaField> hammer{field(vdb::MetaField::kString, "category"),
                                     field(vdb::MetaField::kI64, "price"),
                                     field(vdb::MetaField::kBool, "stocked")};
  hammer[0].text = "tools";
  hammer[1].integer = 25;
  hammer[2].flag = true;

  std::vector<vdb::MetaField> saw{field(vdb::MetaField::kString, "category"),
                                  field(vdb::MetaField::kI64, "price")};
  saw[0].text = "tools";
  saw[1].integer = 75;

  std::vector<vdb::MetaField> ball{field(vdb::MetaField::kString, "category")};
  ball[0].text = "toys";

  const float hammer_v[2] = {1.0f, 0.0f};
  const float saw_v[2] = {0.95f, 0.31f};
  const float ball_v[2] = {0.7f, 0.7f};
  check(bridge.upsert(docs, "hammer", hammer_v, 2, hammer, nullptr).ok, "wrote hammer");
  check(bridge.upsert(docs, "saw", saw_v, 2, saw, nullptr).ok, "wrote saw");
  check(bridge.upsert(docs, "ball", ball_v, 2, ball, nullptr).ok, "wrote ball");

  const float query[2] = {1.0f, 0.0f};
  std::vector<vdb::Hit> hits;

  // No filter searches everything.
  check(bridge.search(docs, query, 2, 10, {}, &hits).ok && hits.size() == 3,
        "an empty filter searches everything");

  // category == "tools"
  vdb::FilterOp tools;
  tools.kind = vdb::FilterOp::kCompareString;
  tools.field = "category";
  tools.op = VDB_OP_EQ;
  tools.text = "tools";
  check(bridge.search(docs, query, 2, 10, {tools}, &hits).ok && hits.size() == 2,
        "a string comparison narrows the search");

  // category == "tools" AND price < 50
  vdb::FilterOp cheap;
  cheap.kind = vdb::FilterOp::kCompareF64;
  cheap.field = "price";
  cheap.op = VDB_OP_LT;
  cheap.real = 50.0;
  vdb::FilterOp both;
  both.kind = vdb::FilterOp::kCombine;
  both.op = VDB_COMBINE_AND;
  both.count = 2;
  vdb::Error e = bridge.search(docs, query, 2, 10, {tools, cheap, both}, &hits);
  check(e.ok, "a combined filter runs: " + e.message);
  check(hits.size() == 1 && !hits.empty() && hits[0].id == "hammer",
        "AND of two clauses leaves only the hammer");

  // A boolean, and an existence test over a field only some documents carry.
  vdb::FilterOp stocked;
  stocked.kind = vdb::FilterOp::kCompareBool;
  stocked.field = "stocked";
  stocked.op = VDB_OP_EQ;
  stocked.flag = true;
  check(bridge.search(docs, query, 2, 10, {stocked}, &hits).ok && hits.size() == 1,
        "a boolean comparison works");

  vdb::FilterOp priced;
  priced.kind = vdb::FilterOp::kUnary;
  priced.field = "price";
  priced.op = VDB_UNARY_EXISTS;
  check(bridge.search(docs, query, 2, 10, {priced}, &hits).ok && hits.size() == 2,
        "an existence test excludes the document without the field");

  // An explicit null is present where an absent field is not — the reason
  // vdb_metadata_set_null exists rather than the binding dropping the key.
  std::vector<vdb::MetaField> noted{field(vdb::MetaField::kNull, "note")};
  check(bridge.upsert(docs, "noted", ball_v, 2, noted, nullptr).ok, "wrote an explicit null");
  vdb::FilterOp has_note;
  has_note.kind = vdb::FilterOp::kUnary;
  has_note.field = "note";
  has_note.op = VDB_UNARY_EXISTS;
  check(bridge.search(docs, query, 2, 10, {has_note}, &hits).ok && hits.size() == 1,
        "an explicitly null field exists; an absent one does not");

  check(bridge.release_collection(docs).ok, "released");
  check(bridge.close(db).ok, "closed");
}

/// An incomplete filter must be refused before it reaches the engine.
///
/// A filter that lost a clause returns documents the caller asked to exclude, and says nothing
/// about it. That is the failure this check exists to make impossible.
void an_incomplete_filter_is_refused() {
  std::printf("incomplete filters\n");
  Scratch dir;
  vdb::Bridge bridge;

  vdb::Handle db = 0;
  check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened");
  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 2, VDB_METRIC_COSINE, &docs).ok, "collection");

  vdb::FilterOp a;
  a.kind = vdb::FilterOp::kCompareString;
  a.field = "category";
  a.op = VDB_OP_EQ;
  a.text = "tools";
  vdb::FilterOp b = a;
  b.text = "toys";

  const float query[2] = {1.0f, 0.0f};
  std::vector<vdb::Hit> hits;

  // Two clauses pushed and never combined: two expressions left on the stack.
  vdb::Error e = bridge.search(docs, query, 2, 10, {a, b}, &hits);
  check(!e.ok, "two uncombined clauses are refused");
  check(e.message.find("incomplete") != std::string::npos,
        "the message says the filter is incomplete: " + e.message);

  // Combining more than were pushed.
  vdb::FilterOp greedy;
  greedy.kind = vdb::FilterOp::kCombine;
  greedy.op = VDB_COMBINE_AND;
  greedy.count = 5;
  check(!bridge.search(docs, query, 2, 10, {a, greedy}, &hits).ok,
        "combining more clauses than exist is refused");

  check(bridge.release_collection(docs).ok, "released");
  check(bridge.close(db).ok, "closed");
}

/// A batch writes every document, and says which one failed when one does.
void batches() {
  std::printf("batches\n");
  Scratch dir;
  vdb::Bridge bridge;

  vdb::Handle db = 0;
  check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened");
  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 4, VDB_METRIC_COSINE, &docs).ok, "collection");

  // One contiguous block of count * dimension floats, which is how it crosses from JavaScript.
  const std::size_t n = 6;
  std::vector<std::string> ids;
  std::vector<float> vectors;
  for (std::size_t i = 0; i < n; i++) {
    ids.push_back("doc-" + std::to_string(i));
    std::vector<float> v = vector_for(static_cast<int>(i), 4);
    vectors.insert(vectors.end(), v.begin(), v.end());
  }

  std::uint64_t inserted = 0;
  vdb::Error e = bridge.upsert_many(docs, ids, vectors.data(), 4, n, {}, &inserted);
  check(e.ok, "wrote a batch of six: " + e.message);
  check(inserted == n, "all six were new");

  std::uint64_t count = 0;
  check(bridge.count(docs, &count).ok && count == n, "six documents are live");

  // Rewriting the same ids replaces rather than inserts.
  check(bridge.upsert_many(docs, ids, vectors.data(), 4, n, {}, &inserted).ok, "rewrote");
  check(inserted == 0, "a rewrite inserts nothing new");
  check(bridge.count(docs, &count).ok && count == n, "still six documents");

  // Per-document metadata, one entry per document.
  std::vector<std::vector<vdb::MetaField>> metadata;
  for (std::size_t i = 0; i < n; i++) {
    vdb::MetaField f;
    f.kind = vdb::MetaField::kI64;
    f.key = "index";
    f.integer = static_cast<std::int64_t>(i);
    metadata.push_back({f});
  }
  check(bridge.upsert_many(docs, ids, vectors.data(), 4, n, metadata, &inserted).ok,
        "a batch carrying metadata");

  // Mismatched lengths are refused rather than silently truncated.
  std::vector<std::string> two{"a", "b"};
  check(!bridge.upsert_many(docs, two, vectors.data(), 4, n, {}, &inserted).ok,
        "fewer ids than documents is refused");
  metadata.pop_back();
  check(!bridge.upsert_many(docs, ids, vectors.data(), 4, n, metadata, &inserted).ok,
        "metadata for only some documents is refused");

  // A failure part-way names the document, because the ones before it were written.
  std::vector<std::string> three{"x", "y", "z"};
  std::vector<float> wrong(3 * 2, 1.0f);
  vdb::Error partial = bridge.upsert_many(docs, three, wrong.data(), 2, 3, {}, &inserted);
  check(!partial.ok, "a wrong dimension fails the batch");
  check(partial.message.find("document 0") != std::string::npos,
        "the message names the document that failed: " + partial.message);

  check(bridge.release_collection(docs).ok, "released");
  check(bridge.close(db).ok, "closed");
}

/// Stats, compaction and verification, which every other SDK already exposes.
void maintenance() {
  std::printf("maintenance\n");
  Scratch dir;
  vdb::Bridge bridge;

  vdb::Handle db = 0;
  check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened");
  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 4, VDB_METRIC_COSINE, &docs).ok, "collection");

  for (int i = 0; i < 10; i++) {
    std::vector<float> v = vector_for(i, 4);
    bridge.upsert(docs, "doc-" + std::to_string(i), v.data(), 4, {}, nullptr);
  }
  check(bridge.flush(docs).ok, "flushed");

  vdb::Stats stats;
  vdb::Error e = bridge.stats(docs, &stats);
  check(e.ok, "read stats: " + e.message);
  check(stats.live_documents == 10, "ten live documents");
  check(stats.dimension == 4, "the dimension is reported");
  check(stats.dead_ratio == 0.0f, "nothing is dead yet");

  for (int i = 0; i < 5; i++) {
    bool existed = false;
    bridge.remove(docs, "doc-" + std::to_string(i), &existed);
  }
  check(bridge.flush(docs).ok, "flushed the deletions");
  check(bridge.stats(docs, &stats).ok, "stats again");
  check(stats.live_documents == 5, "five live documents");
  check(stats.dead_ratio > 0.0f, "the dead ratio rose after deleting half");

  std::uint64_t reclaimed = 0;
  e = bridge.compact(db, 0.0f, &reclaimed);
  check(e.ok, "compacted: " + e.message);
  check(bridge.stats(docs, &stats).ok, "stats after compaction");
  check(stats.live_documents == 5, "compaction kept the live documents");

  vdb::VerifyReport report;
  e = bridge.verify(db, VDB_VERIFY_FULL, &report);
  check(e.ok, "verified: " + e.message);
  check(report.errors == 0, "a healthy database reports no errors");

  check(bridge.release_collection(docs).ok, "released");
  check(bridge.close(db).ok, "closed");
}

/// Opening a collection is not the same as creating one, and dropping is irreversible.
void opening_and_dropping() {
  std::printf("opening and dropping\n");
  Scratch dir;
  vdb::Bridge bridge;

  vdb::Handle db = 0;
  check(bridge.open(dir.path(), true, false, VDB_DURABILITY_BATCH, &db).ok, "opened");

  // open_collection does not create: a missing collection is an error, which is what you want
  // when its absence is a bug rather than a first run.
  vdb::Handle missing = 0;
  vdb::Error e = bridge.open_collection(db, "absent", &missing);
  check(!e.ok, "opening a collection that does not exist is an error");

  vdb::Handle docs = 0;
  check(bridge.collection(db, "docs", 3, VDB_METRIC_COSINE, &docs).ok, "created it");
  vdb::Handle reopened = 0;
  check(bridge.open_collection(db, "docs", &reopened).ok, "opened the existing one");
  check(bridge.release_collection(reopened).ok, "released");

  const float v[3] = {1.0f, 0.0f, 0.0f};
  check(bridge.upsert(docs, "a", v, 3, {}, nullptr).ok, "wrote a document");
  check(bridge.flush(docs).ok, "flushed");
  check(bridge.release_collection(docs).ok, "released before dropping");

  check(bridge.drop_collection(db, "docs").ok, "dropped");
  check(!bridge.open_collection(db, "docs", &missing).ok, "it is gone");

  check(bridge.flush_database(db).ok, "flushing a database with no collections is fine");
  check(bridge.close(db).ok, "closed");
}

}  // namespace

int main() {
  versions();
  lifecycle();
  misuse();
  errors_keep_their_message();
  persistence();
  the_destructor_cleans_up();
  metadata_and_filters();
  an_incomplete_filter_is_refused();
  batches();
  maintenance();
  opening_and_dropping();

  std::printf("\n%d checks, %d failures\n", checks, failures);
  if (failures == 0) {
    std::printf("all checks passed\n");
  }
  return failures == 0 ? 0 : 1;
}
