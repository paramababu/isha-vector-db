// The only file here that touches React Native.
//
// # Read this before changing it
//
// **Nothing in this file is compiled or run by CI.** It needs React Native's JSI headers, which
// only exist inside an app. Everything it could get wrong that is worth testing — handle
// lifetimes, error translation, use-after-close, the create-or-open fallback — lives in
// `vdb_bridge.cpp`, which is compiled and exercised against the real engine by
// `scripts/test-react-native.sh`.
//
// So this file has one job and should keep it: convert JS values to C++ ones, call a single
// `Bridge` method, convert back. If you find yourself adding a branch here, it probably belongs
// in the bridge where it can be tested.
//
// # Zero copy
//
// A vector arrives as a `Float32Array`. `getArrayBuffer().data()` hands over the backing store
// directly, so a 768-float vector crosses as a pointer rather than the ~10 KB JSON array the
// legacy bridge would have produced. That is the entire reason for using JSI here.
//
// The pointer is valid only for the duration of the call: JavaScript may move or free the buffer
// afterwards, and the bridge copies anything it needs to keep.

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

Handle handleOf(jsi::Runtime& rt, const jsi::Value& value) {
  return static_cast<Handle>(value.asNumber());
}

std::string stringOf(jsi::Runtime& rt, const jsi::Value& value) {
  return value.asString(rt).utf8(rt);
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

    if (method == "open") return fn(rt, name, 3, &HostBridge::open);
    if (method == "close") return fn(rt, name, 1, &HostBridge::close);
    if (method == "collection") return fn(rt, name, 4, &HostBridge::collection);
    if (method == "releaseCollection") return fn(rt, name, 1, &HostBridge::releaseCollection);
    if (method == "upsert") return fn(rt, name, 3, &HostBridge::upsert);
    if (method == "remove") return fn(rt, name, 2, &HostBridge::remove);
    if (method == "contains") return fn(rt, name, 2, &HostBridge::contains);
    if (method == "count") return fn(rt, name, 1, &HostBridge::count);
    if (method == "flush") return fn(rt, name, 1, &HostBridge::flush);
    if (method == "search") return fn(rt, name, 3, &HostBridge::search);

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
    require(rt, bridge_->open(stringOf(rt, args[0]), args[1].getBool(), args[2].getBool(), &db));
    return jsi::Value(static_cast<double>(db));
  }

  jsi::Value close(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    require(rt, bridge_->close(handleOf(rt, args[0])));
    return jsi::Value::undefined();
  }

  jsi::Value collection(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    Handle out = 0;
    require(rt, bridge_->collection(handleOf(rt, args[0]), stringOf(rt, args[1]),
                                    static_cast<std::uint32_t>(args[2].asNumber()),
                                    static_cast<std::int32_t>(args[3].asNumber()), &out));
    return jsi::Value(static_cast<double>(out));
  }

  jsi::Value releaseCollection(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    require(rt, bridge_->release_collection(handleOf(rt, args[0])));
    return jsi::Value::undefined();
  }

  jsi::Value upsert(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    std::uint32_t dimension = 0;
    const float* vector = floats(rt, args[2], &dimension);
    bool inserted = false;
    require(rt, bridge_->upsert(handleOf(rt, args[0]), stringOf(rt, args[1]), vector, dimension,
                                &inserted));
    return jsi::Value(inserted);
  }

  jsi::Value remove(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    bool existed = false;
    require(rt, bridge_->remove(handleOf(rt, args[0]), stringOf(rt, args[1]), &existed));
    return jsi::Value(existed);
  }

  jsi::Value contains(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    bool found = false;
    require(rt, bridge_->contains(handleOf(rt, args[0]), stringOf(rt, args[1]), &found));
    return jsi::Value(found);
  }

  jsi::Value count(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    std::uint64_t value = 0;
    require(rt, bridge_->count(handleOf(rt, args[0]), &value));
    return jsi::Value(static_cast<double>(value));
  }

  jsi::Value flush(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    require(rt, bridge_->flush(handleOf(rt, args[0])));
    return jsi::Value::undefined();
  }

  jsi::Value search(jsi::Runtime& rt, const jsi::Value* args, std::size_t) {
    std::uint32_t dimension = 0;
    const float* query = floats(rt, args[1], &dimension);
    std::vector<Hit> hits;
    require(rt, bridge_->search(handleOf(rt, args[0]), query, dimension,
                                static_cast<std::size_t>(args[2].asNumber()), &hits));

    jsi::Array out(rt, hits.size());
    for (std::size_t i = 0; i < hits.size(); i++) {
      jsi::Object hit(rt);
      hit.setProperty(rt, "id", jsi::String::createFromUtf8(rt, hits[i].id));
      hit.setProperty(rt, "score", jsi::Value(static_cast<double>(hits[i].score)));
      out.setValueAtIndex(rt, i, hit);
    }
    return out;
  }

  std::shared_ptr<Bridge> bridge_;
};

}  // namespace

/// Install the bindings. Called once, by the TurboModule, at startup.
void install(jsi::Runtime& runtime) {
  auto host = std::make_shared<HostBridge>(std::make_shared<Bridge>());
  runtime.global().setProperty(
      runtime, "__vdb", jsi::Object::createFromHostObject(runtime, std::move(host)));
}

}  // namespace vdb
