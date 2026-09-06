// The only file here that touches React Native.
//
// # Read this before changing it
//
// This file is **compiled** by CI, against React Native's real JSI headers, which
// `scripts/test-react-native.sh` fetches from the `react-native` npm package. It is not **run**:
// that needs a JavaScript engine and an app. So a signature that no longer exists or a
// conversion that does not type-check is caught here on every push, while a conversion that
// compiles and does the wrong thing is not.
//
// That is why the file still has one job and should keep it: convert JS values to the types in
// `vdb_bridge.h`, call a single `Bridge` method, convert back. Everything that could be wrong
// *rather than merely ill-typed* — handle lifetimes, error translation, use-after-close, the
// create-or-open fallback, the postfix filter and metadata builders — lives in the bridge, which
// is compiled and executed against the real engine. If you find yourself adding a branch here,
// it probably belongs there.
//
// # Zero copy
//
// A vector arrives as a `Float32Array`. `getArrayBuffer().data()` hands over the backing store
// directly, so a 768-float vector crosses as a pointer rather than the ~10 KB JSON array the
// legacy bridge would have produced. That is the entire reason for using JSI here.
//
// The pointer is valid only for the duration of the call: JavaScript may move or free the buffer
// afterwards, and the bridge copies anything it needs to keep.
//
// # Fixed shapes
//
// Filter steps and metadata fields arrive as arrays of objects with the *same* property names on
// every element, whatever the step means. That is deliberate: it lets the readers below be
// loops with no branches, and puts the decision about what a step means in the tested layer.

#include <jsi/jsi.h>

#include <memory>
#include <string>
#include <vector>

#include "vdb_bridge.h"

namespace vdb {
namespace {

using namespace facebook;

/// Throw the engine's error into JavaScript, preserving its structured code.
///
/// A plain `jsi::JSError` would carry only the message. The `code` property is what lets a
/// caller branch on "collection not found" rather than matching on English.
[[noreturn]] void throwError(jsi::Runtime& rt, const Error& e) {
  jsi::Object error(rt);
  error.setProperty(rt, "message", jsi::String::createFromUtf8(rt, e.message));
  error.setProperty(rt, "code", jsi::Value(static_cast<double>(e.code)));
  error.setProperty(rt, "name", jsi::String::createFromUtf8(rt, "VdbError"));
  throw jsi::JSError(rt, jsi::Value(rt, error));
}

void require(jsi::Runtime& rt, const Error& e) {
  if (!e.ok) {
    throwError(rt, e);
  }
}

/// Borrow a `Float32Array`'s backing store. Valid only for this call.
const float* floats(jsi::Runtime& rt, const jsi::Value& value, std::uint32_t* length) {
  if (!value.isObject()) {
    throw jsi::JSError(rt, "expected a Float32Array");
  }
  jsi::Object object = value.getObject(rt);
  if (!object.isArrayBuffer(rt)) {
    // A TypedArray, not the buffer itself: take its buffer and honour the view's offset.
    jsi::Value buffer = object.getProperty(rt, "buffer");
    if (!buffer.isObject() || !buffer.getObject(rt).isArrayBuffer(rt)) {
      throw jsi::JSError(rt, "expected a Float32Array");
    }
    std::size_t offset =
        static_cast<std::size_t>(object.getProperty(rt, "byteOffset").asNumber());
    std::size_t bytes = static_cast<std::size_t>(object.getProperty(rt, "byteLength").asNumber());
    auto array = buffer.getObject(rt).getArrayBuffer(rt);
    *length = static_cast<std::uint32_t>(bytes / sizeof(float));
    return reinterpret_cast<const float*>(array.data(rt) + offset);
  }
  auto array = object.getArrayBuffer(rt);
  *length = static_cast<std::uint32_t>(array.size(rt) / sizeof(float));
  return reinterpret_cast<const float*>(array.data(rt));
}

Handle handleOf(const jsi::Value& value) { return static_cast<Handle>(value.asNumber()); }

std::string stringOf(jsi::Runtime& rt, const jsi::Value& value) {
  return value.asString(rt).utf8(rt);
}

std::int32_t intOf(const jsi::Value& value) { return static_cast<std::int32_t>(value.asNumber()); }

jsi::Array arrayOf(jsi::Runtime& rt, const jsi::Value& value, const char* what) {
  if (!value.isObject() || !value.getObject(rt).isArray(rt)) {
    throw jsi::JSError(rt, std::string("expected an array of ") + what);
  }
  return value.getObject(rt).getArray(rt);
}

/// Read the metadata fields the JavaScript layer compiled.
///
/// Every element carries `kind`, `key`, `text`, `number` and `flag`, whichever it uses, so this
/// reads five names and lets `Bridge::upsert` decide what they mean.
std::vector<MetaField> metaFieldsOf(jsi::Runtime& rt, const jsi::Value& value) {
  std::vector<MetaField> fields;
  if (value.isUndefined() || value.isNull()) {
    return fields;
  }
  jsi::Array array = arrayOf(rt, value, "metadata fields");
  std::size_t n = array.size(rt);
  fields.reserve(n);
  for (std::size_t i = 0; i < n; i++) {
    jsi::Object element = array.getValueAtIndex(rt, i).asObject(rt);
    MetaField field;
    field.kind = intOf(element.getProperty(rt, "kind"));
    field.key = stringOf(rt, element.getProperty(rt, "key"));
    field.text = stringOf(rt, element.getProperty(rt, "text"));
    double number = element.getProperty(rt, "number").asNumber();
    field.integer = static_cast<std::int64_t>(number);
    field.real = number;
    field.flag = element.getProperty(rt, "flag").getBool();
    fields.push_back(std::move(field));
  }
  return fields;
}

/// Read the postfix filter sequence the JavaScript layer compiled.
std::vector<FilterOp> filterOpsOf(jsi::Runtime& rt, const jsi::Value& value) {
  std::vector<FilterOp> ops;
  if (value.isUndefined() || value.isNull()) {
    return ops;
  }
  jsi::Array array = arrayOf(rt, value, "filter steps");
  std::size_t n = array.size(rt);
  ops.reserve(n);
  for (std::size_t i = 0; i < n; i++) {
    jsi::Object element = array.getValueAtIndex(rt, i).asObject(rt);
    FilterOp op;
    op.kind = intOf(element.getProperty(rt, "kind"));
    op.field = stringOf(rt, element.getProperty(rt, "field"));
    op.op = intOf(element.getProperty(rt, "op"));
    op.text = stringOf(rt, element.getProperty(rt, "text"));
    double number = element.getProperty(rt, "number").asNumber();
    op.integer = static_cast<std::int64_t>(number);
    op.real = number;
    op.flag = element.getProperty(rt, "flag").getBool();
    op.count = static_cast<std::size_t>(element.getProperty(rt, "count").asNumber());
    ops.push_back(std::move(op));
  }
  return ops;
}

jsi::Array hitsOf(jsi::Runtime& rt, const std::vector<Hit>& hits) {
  jsi::Array out(rt, hits.size());
  for (std::size_t i = 0; i < hits.size(); i++) {
    jsi::Object hit(rt);
    hit.setProperty(rt, "id", jsi::String::createFromUtf8(rt, hits[i].id));
    hit.setProperty(rt, "score", jsi::Value(static_cast<double>(hits[i].score)));
    out.setValueAtIndex(rt, i, hit);
  }
  return out;
}

/// The object installed as `globalThis.__vdb`.
class HostBridge : public jsi::HostObject {
 public:
  explicit HostBridge(std::shared_ptr<Bridge> bridge) : bridge_(std::move(bridge)) {}

  jsi::Value get(jsi::Runtime& rt, const jsi::PropNameID& name) override {
    std::string method = name.utf8(rt);

    if (method == "version") {
      return jsi::String::createFromUtf8(rt, Bridge::version());
    }
    if (method == "abiVersion") {
      return jsi::Value(static_cast<double>(Bridge::abi_version()));
    }
    if (method == "formatVersion") {
      return jsi::Value(static_cast<double>(Bridge::format_version()));
    }
    if (method == "liveHandles") {
      return jsi::Value(static_cast<double>(bridge_->live_handles()));
    }

    if (method == "open") return fn(rt, name, 4, &HostBridge::open);
    if (method == "close") return fn(rt, name, 1, &HostBridge::close);
    if (method == "flushDatabase") return fn(rt, name, 1, &HostBridge::flushDatabase);
    if (method == "collection") return fn(rt, name, 4, &HostBridge::collection);
    if (method == "openCollection") return fn(rt, name, 2, &HostBridge::openCollection);
    if (method == "dropCollection") return fn(rt, name, 2, &HostBridge::dropCollection);
    if (method == "releaseCollection") return fn(rt, name, 1, &HostBridge::releaseCollection);
    if (method == "upsert") return fn(rt, name, 4, &HostBridge::upsert);
    if (method == "upsertMany") return fn(rt, name, 5, &HostBridge::upsertMany);
    if (method == "remove") return fn(rt, name, 2, &HostBridge::remove);
    if (method == "contains") return fn(rt, name, 2, &HostBridge::contains);
    if (method == "count") return fn(rt, name, 1, &HostBridge::count);
    if (method == "flush") return fn(rt, name, 1, &HostBridge::flush);
    if (method == "search") return fn(rt, name, 4, &HostBridge::search);
    if (method == "stats") return fn(rt, name, 1, &HostBridge::stats);
    if (method == "compact") return fn(rt, name, 2, &HostBridge::compact);
    if (method == "verify") return fn(rt, name, 2, &HostBridge::verify);

    return jsi::Value::undefined();
  }

 private:
  using Method = jsi::Value (HostBridge::*)(jsi::Runtime&, const jsi::Value*, std::size_t);

  jsi::Value fn(jsi::Runtime& rt, const jsi::PropNameID& name, unsigned count, Method method) {
    return jsi::Function::createFromHostFunction(
        rt, name, count,
        [this, method](jsi::Runtime& rt, const jsi::Value&, const jsi::Value* args,
                       std::size_t n) { return (this->*method)(rt, args, n); });
  }

  jsi::Value open(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    Handle db = 0;
    require(rt, bridge_->open(stringOf(rt, args[0]), args[1].getBool(), args[2].getBool(),
                              intOf(args[3]), &db));
    return jsi::Value(static_cast<double>(db));
  }

  jsi::Value close(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    require(rt, bridge_->close(handleOf(args[0])));
    return jsi::Value::undefined();
  }

  jsi::Value flushDatabase(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    require(rt, bridge_->flush_database(handleOf(args[0])));
    return jsi::Value::undefined();
  }

  jsi::Value collection(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    Handle out = 0;
    require(rt, bridge_->collection(handleOf(args[0]), stringOf(rt, args[1]),
                                    static_cast<std::uint32_t>(args[2].asNumber()),
                                    intOf(args[3]), &out));
    return jsi::Value(static_cast<double>(out));
  }

  jsi::Value openCollection(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    Handle out = 0;
    require(rt, bridge_->open_collection(handleOf(args[0]), stringOf(rt, args[1]), &out));
    return jsi::Value(static_cast<double>(out));
  }

  jsi::Value dropCollection(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    require(rt, bridge_->drop_collection(handleOf(args[0]), stringOf(rt, args[1])));
    return jsi::Value::undefined();
  }

  jsi::Value releaseCollection(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    require(rt, bridge_->release_collection(handleOf(args[0])));
    return jsi::Value::undefined();
  }

  jsi::Value upsert(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    std::uint32_t dimension = 0;
    const float* vector = floats(rt, args[2], &dimension);
    bool inserted = false;
    require(rt, bridge_->upsert(handleOf(args[0]), stringOf(rt, args[1]), vector, dimension,
                                metaFieldsOf(rt, args[3]), &inserted));
    return jsi::Value(inserted);
  }

  jsi::Value upsertMany(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    jsi::Array idArray = arrayOf(rt, args[1], "ids");
    std::size_t count = idArray.size(rt);
    std::vector<std::string> ids;
    ids.reserve(count);
    for (std::size_t i = 0; i < count; i++) {
      ids.push_back(stringOf(rt, idArray.getValueAtIndex(rt, i)));
    }

    // One contiguous block of count * dimension floats, packed on the JavaScript side. The
    // dimension is passed rather than divided out so a short buffer is the bridge's error to
    // report rather than a silent reinterpretation here.
    std::uint32_t total = 0;
    const float* vectors = floats(rt, args[2], &total);
    auto dimension = static_cast<std::uint32_t>(args[3].asNumber());
    if (dimension == 0 || total != count * dimension) {
      throw jsi::JSError(rt, "the batch's vector buffer does not match its ids and dimension");
    }

    std::vector<std::vector<MetaField>> metadata;
    if (!args[4].isUndefined() && !args[4].isNull()) {
      jsi::Array outer = arrayOf(rt, args[4], "per-document metadata");
      std::size_t entries = outer.size(rt);
      metadata.reserve(entries);
      for (std::size_t i = 0; i < entries; i++) {
        metadata.push_back(metaFieldsOf(rt, outer.getValueAtIndex(rt, i)));
      }
    }

    std::uint64_t inserted = 0;
    require(rt, bridge_->upsert_many(handleOf(args[0]), ids, vectors, dimension, count, metadata,
                                     &inserted));
    return jsi::Value(static_cast<double>(inserted));
  }

  jsi::Value remove(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    bool existed = false;
    require(rt, bridge_->remove(handleOf(args[0]), stringOf(rt, args[1]), &existed));
    return jsi::Value(existed);
  }

  jsi::Value contains(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    bool found = false;
    require(rt, bridge_->contains(handleOf(args[0]), stringOf(rt, args[1]), &found));
    return jsi::Value(found);
  }

  jsi::Value count(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    std::uint64_t value = 0;
    require(rt, bridge_->count(handleOf(args[0]), &value));
    return jsi::Value(static_cast<double>(value));
  }

  jsi::Value flush(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    require(rt, bridge_->flush(handleOf(args[0])));
    return jsi::Value::undefined();
  }

  jsi::Value search(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    std::uint32_t dimension = 0;
    const float* query = floats(rt, args[1], &dimension);
    std::vector<Hit> hits;
    require(rt, bridge_->search(handleOf(args[0]), query, dimension,
                                static_cast<std::size_t>(args[2].asNumber()),
                                filterOpsOf(rt, args[3]), &hits));
    return hitsOf(rt, hits);
  }

  jsi::Value stats(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    Stats s;
    require(rt, bridge_->stats(handleOf(args[0]), &s));
    jsi::Object out(rt);
    out.setProperty(rt, "liveDocuments", jsi::Value(static_cast<double>(s.live_documents)));
    out.setProperty(rt, "totalRows", jsi::Value(static_cast<double>(s.total_rows)));
    out.setProperty(rt, "segments", jsi::Value(static_cast<double>(s.segments)));
    out.setProperty(rt, "bufferedDocuments",
                    jsi::Value(static_cast<double>(s.buffered_documents)));
    out.setProperty(rt, "deadRatio", jsi::Value(static_cast<double>(s.dead_ratio)));
    out.setProperty(rt, "dimension", jsi::Value(static_cast<double>(s.dimension)));
    return out;
  }

  jsi::Value compact(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    std::uint64_t reclaimed = 0;
    require(rt, bridge_->compact(handleOf(args[0]), static_cast<float>(args[1].asNumber()),
                                 &reclaimed));
    return jsi::Value(static_cast<double>(reclaimed));
  }

  jsi::Value verify(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    VerifyReport report;
    require(rt, bridge_->verify(handleOf(args[0]), intOf(args[1]), &report));
    jsi::Object out(rt);
    out.setProperty(rt, "errors", jsi::Value(static_cast<double>(report.errors)));
    out.setProperty(rt, "warnings", jsi::Value(static_cast<double>(report.warnings)));
    return out;
  }

  std::shared_ptr<Bridge> bridge_;
};

}  // namespace

/// Install the bindings. Called once, by the native module, at startup.
///
/// Idempotent by construction: installing twice replaces the host object, and the `Bridge` the
/// old one held releases its handles when the last reference to it goes. That matters because a
/// React Native reload re-runs the JavaScript without restarting the process.
void install(jsi::Runtime& runtime) {
  auto host = std::make_shared<HostBridge>(std::make_shared<Bridge>());
  runtime.global().setProperty(
      runtime, "__vdb", jsi::Object::createFromHostObject(runtime, std::move(host)));
}

}  // namespace vdb
