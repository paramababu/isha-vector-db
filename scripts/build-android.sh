#!/usr/bin/env bash
# Build the JNI library for every Android ABI.
#
# Uses the NDK's clang wrappers directly rather than cargo-ndk: the linker path is the only thing
# cargo-ndk was providing, and one fewer tool to install is one fewer thing to go wrong in a
# contributor's setup.
set -euo pipefail
cd "$(dirname "$0")/.."

NDK="${ANDROID_NDK_HOME:-}"
if [[ -z "$NDK" ]]; then
  # Pick the newest NDK the SDK has, so a contributor who installed one through Android Studio
  # does not also have to export a variable.
  SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
  NDK=$(find "$SDK/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort -V | tail -1 || true)
fi
[[ -n "$NDK" && -d "$NDK" ]] || {
  echo "FAIL: no NDK found. Set ANDROID_NDK_HOME."
  exit 1
}
echo "using NDK: $NDK"

case "$(uname -s)" in
  Darwin) HOST_TAG="darwin-x86_64" ;;
  Linux)  HOST_TAG="linux-x86_64" ;;
  *)      echo "unsupported host"; exit 1 ;;
esac
export PATH="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin:$PATH"

# Android 15 runs on devices with 16 KB memory pages, and a library linked for 4 KB will not
# load there at all. The flag is harmless on older devices, and forgetting it is the kind of
# thing discovered by a one-star review rather than by a test.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384"

# "target:abi" pairs rather than an associative array: macOS still ships bash 3.2, which has
# none, and a build script that only runs on a contributor's Linux box is half a build script.
ABIS="aarch64-linux-android:arm64-v8a armv7-linux-androideabi:armeabi-v7a x86_64-linux-android:x86_64"

OUT="sdk/android/src/main/jniLibs"
for pair in $ABIS; do
  target="${pair%%:*}"
  abi="${pair##*:}"
  echo "--- $abi ($target)"
  rustup target add "$target" >/dev/null 2>&1 || true
  cargo build --release -p vdb-jni --target "$target"
  mkdir -p "$OUT/$abi"
  cp "target/$target/release/libvdb_jni.so" "$OUT/$abi/"
  printf '    %s: %s bytes\n' "$abi" "$(wc -c < "$OUT/$abi/libvdb_jni.so" | tr -d ' ')"
done
echo "wrote $OUT"
