// The JNI entry point CMakeLists.txt has always referenced and this repository never had.
//
// Its absence is one of the two reasons version 0.1.0 could not build: `add_library` named a
// source file that was not in the tarball, so the CMake configure step failed before it reached
// the compiler.
//
// All it does is turn the `long` the Java side holds back into a `jsi::Runtime&` and call
// `vdb::install`. There is nothing else here on purpose — every decision worth testing is in
// `vdb_bridge.cpp`, which runs on a development machine.
//
// NOT VERIFIED. There is no Android project here to build it against.

#include <jni.h>
#include <jsi/jsi.h>

namespace vdb {
/// Defined in `cpp/vdb_jsi.cpp`.
void install(facebook::jsi::Runtime& runtime);
}  // namespace vdb

extern "C" JNIEXPORT void JNICALL
Java_dev_isha_vectordb_reactnative_VdbModule_nativeInstall(JNIEnv* /*env*/, jclass /*clazz*/,
                                                           jlong runtimePointer) {
  if (runtimePointer == 0) {
    // The Java side already refuses to call with a zero pointer; this is the second guard,
    // because dereferencing it would take the whole app down rather than report a problem.
    return;
  }
  auto* runtime = reinterpret_cast<facebook::jsi::Runtime*>(runtimePointer);
  vdb::install(*runtime);
}
