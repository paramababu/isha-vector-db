#!/usr/bin/env bash
# Assemble the native libraries the React Native package ships.
#
# The package's JavaScript and C++ are in the repository; the engine is not. This puts it where
# `android/CMakeLists.txt` and the podspec expect to find it:
#
#   android/jniLibs/<abi>/libisha_vector_db_ffi.a   the Rust static library, per Android ABI
#   ios/Vdb.xcframework                             the same, per Apple architecture
#
# Both are gitignored. They are produced at release time and are architecture-specific, so a
# committed copy would be one machine's binary in everyone's checkout.
#
# Run with no arguments to build whatever this machine can. An Android build needs the NDK; an
# iOS build needs macOS and Xcode. Neither is a hard failure here, because the common case is a
# contributor who has one of the two and wants the package assembled for that one.
set -euo pipefail
cd "$(dirname "$0")/.."

PACKAGE=sdk/react-native
did_something=0

# Remove debug information from a static archive, in place.
#
# Worth doing rather than left to the app's build: this package ships the engine for six
# architectures in one tarball, because an app's build chooses architectures and npm's os/cpu
# resolution cannot. Unstripped that is 158 MB unpacked, every byte of which every React Native
# developer downloads. Stripping takes ~35% off, and R10 in the risk register — binary size
# rejection on mobile — says to care about that.
#
# Debug info only. The exported symbols are global and are what the linker needs; a strip that
# removed those would produce an archive that links against nothing, so this never uses one.
strip_archive() {
  local archive="$1" tool="$2"
  shift 2
  if [[ -z "$tool" || ! -x "$tool" ]]; then
    echo "    note: no strip tool; shipping $archive unstripped"
    return
  fi
  local before
  before=$(wc -c < "$archive" | tr -d ' ')
  "$tool" "$@" "$archive" 2>/dev/null || {
    echo "    note: could not strip $archive; shipping it unstripped"
    return
  }
  local after
  after=$(wc -c < "$archive" | tr -d ' ')
  printf '    stripped %s: %s -> %s bytes\n' "$(basename "$(dirname "$archive")")" "$before" "$after"
}

# The C ABI header. `npm pack` would do this through prepack, but a developer pointing an app at
# a local checkout never runs pack, and would otherwise get a missing-include error.
mkdir -p "$PACKAGE/include"
cp crates/isha-vector-db-ffi/include/vdb.h "$PACKAGE/include/vdb.h"
echo "vdb.h -> $PACKAGE/include/"

# ---- Android ---------------------------------------------------------------

# The same "target:abi" pairs build-android.sh uses, and for the same reason it uses a string:
# macOS still ships bash 3.2, which has no associative arrays.
ANDROID_ABIS="aarch64-linux-android:arm64-v8a armv7-linux-androideabi:armeabi-v7a x86_64-linux-android:x86_64"

android() {
  local ndk="${ANDROID_NDK_HOME:-}"
  if [[ -z "$ndk" ]]; then
    local sdk="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
    ndk=$(find "$sdk/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort -V | tail -1 || true)
  fi
  if [[ -z "$ndk" || ! -d "$ndk" ]]; then
    echo "skipping Android: no NDK (set ANDROID_NDK_HOME)"
    return
  fi

  local host_tag
  case "$(uname -s)" in
    Darwin) host_tag="darwin-x86_64" ;;
    Linux)  host_tag="linux-x86_64" ;;
    *)      echo "skipping Android: unsupported host"; return ;;
  esac
  export PATH="$ndk/toolchains/llvm/prebuilt/$host_tag/bin:$PATH"

  # No 16 KB page-size link flag here, deliberately. What this builds is a static archive, and
  # an archive is not linked — libvdb.so is, by the app's own CMake, which is therefore where
  # `-Wl,-z,max-page-size=16384` lives (see android/CMakeLists.txt). Setting it on this build
  # would affect only the cdylib nobody ships, while reading as though the problem were handled.

  echo "building the Android static libraries"
  for pair in $ANDROID_ABIS; do
    local target="${pair%%:*}"
    local abi="${pair##*:}"
    echo "--- $abi ($target)"
    rustup target add "$target" >/dev/null 2>&1 || true
    cargo build --release -p isha-vector-db-ffi --target "$target"
    mkdir -p "$PACKAGE/android/jniLibs/$abi"
    cp "target/$target/release/libisha_vector_db_ffi.a" "$PACKAGE/android/jniLibs/$abi/"
    strip_archive "$PACKAGE/android/jniLibs/$abi/libisha_vector_db_ffi.a" \
      "$(command -v llvm-strip || true)" --strip-debug
    printf '    %s: %s bytes\n' "$abi" \
      "$(wc -c < "$PACKAGE/android/jniLibs/$abi/libisha_vector_db_ffi.a" | tr -d ' ')"
  done
  did_something=1
}

# ---- iOS -------------------------------------------------------------------

ios() {
  if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "skipping iOS: not macOS"
    return
  fi
  if ! command -v xcodebuild >/dev/null 2>&1; then
    echo "skipping iOS: no xcodebuild"
    return
  fi

  echo "building Vdb.xcframework"
  ./scripts/build-xcframework.sh
  rm -rf "$PACKAGE/ios/Vdb.xcframework"
  cp -R target/xcframework/Vdb.xcframework "$PACKAGE/ios/Vdb.xcframework"
  # -S drops debug symbols, -x drops local ones. The 43 exported vdb_* symbols are global and
  # survive both, which is what the linker needs; nothing else in the archive is anyone's.
  while IFS= read -r archive; do
    strip_archive "$archive" "$(xcrun -f strip 2>/dev/null || true)" -S -x
  done < <(find "$PACKAGE/ios/Vdb.xcframework" -name '*.a')
  echo "Vdb.xcframework -> $PACKAGE/ios/"
  did_something=1
}

android
ios

echo
if [[ $did_something -eq 0 ]]; then
  echo "No native libraries were built. The package's JavaScript is usable for tests, but an app"
  echo "linking against it will fail at the link step with a missing libisha_vector_db_ffi.a."
else
  echo "React Native package assembled in $PACKAGE"
fi
