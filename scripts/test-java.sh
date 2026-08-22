#!/usr/bin/env bash
# Compile and run the Java API against a desktop JVM.
#
# The JNI boundary is the part of the Android SDK most likely to be wrong, and it does not need
# a device to exercise. Running it here turns a five-minute emulator loop into a five-second one;
# the instrumented tests then cover what only a device can.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building the JNI library for the host…"
cargo build --release -p vdb-jni

case "$(uname -s)" in
  Darwin) LIB="target/release/libvdb_jni.dylib" ;;
  Linux)  LIB="target/release/libvdb_jni.so" ;;
  *)      echo "unsupported host"; exit 1 ;;
esac
[[ -f "$LIB" ]] || { echo "FAIL: $LIB was not produced"; exit 1; }

OUT="$(mktemp -d)"
echo "compiling the Java sources…"
javac -Xlint:all -d "$OUT" \
  sdk/android/src/main/java/dev/vdb/*.java \
  sdk/android/src/test/java/dev/vdb/*.java

echo "running…"
java -cp "$OUT" -Dvdb.library.path="$PWD/$LIB" dev.vdb.SmokeTest
