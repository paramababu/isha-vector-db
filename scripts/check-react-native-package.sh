#!/usr/bin/env bash
# Check that the React Native tarball contains every file its own build files reference.
#
# # Why this exists
#
# Version 0.1.0 was published to npm and could not be installed by anybody. The tarball held nine
# files: the JavaScript, the C++, a podspec and a CMakeLists. It did not hold
# `android/build.gradle`, so autolinking found no Android module and contributed nothing; it did
# not hold `android/src/main/cpp/vdb_module.cpp`, which CMakeLists named as a source; it did not
# hold anything under `ios/`, though the podspec globbed it; and it did not hold `vdb.h`, which
# every C++ file in it includes.
#
# Every one of those is mechanically checkable from the tarball, without an Android or iOS
# toolchain — which is the point. The platform builds cannot be run here, but "the package
# contains what it says it contains" can, and that is the failure that actually shipped.
#
# What this does NOT check: that Gradle, CocoaPods or Xcode are happy with the contents. Only a
# real app can say that. See sdk/react-native/README.md.
#
# Pass --release to additionally require the native libraries. They are gitignored and produced
# at release time, so demanding them on every pull request would fail every pull request; but
# publishing without them ships a package that links against nothing, which is the other half of
# what went wrong in 0.1.0.
set -euo pipefail
cd "$(dirname "$0")/.."

RELEASE=0
[[ "${1:-}" == "--release" ]] && RELEASE=1

PACKAGE=sdk/react-native
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

echo "packing $PACKAGE"
# --ignore-scripts is deliberately NOT passed: prepack copies the C ABI header in, and whether
# that works is one of the things being checked.
tarball=$(cd "$PACKAGE" && npm pack --pack-destination "$OUT" --silent | tail -1)
tar xzf "$OUT/$tarball" -C "$OUT"
ROOT="$OUT/package"

failures=0
present() {
  if [[ -e "$ROOT/$1" ]]; then
    echo "  ok   $1${2:+  ($2)}"
  else
    echo "  FAIL $1 is missing${2:+  ($2)}"
    failures=$((failures + 1))
  fi
}

echo
echo "files the build references"
present package.json
present README.md
present index.d.ts                              "the types package.json points at"
present src/native.js                           "the entry point package.json points at"
present src/index.js
present src/filter.js
present src/error.js
present include/vdb.h                           "included by every C++ file here; copied by prepack"
present cpp/vdb_bridge.h
present cpp/vdb_bridge.cpp
present cpp/vdb_jsi.cpp
present react-native.config.js                  "autolinking"
present isha-vector-db.podspec                  "autolinking, iOS"
present android/build.gradle                    "autolinking, Android — absent in 0.1.0"
present android/CMakeLists.txt
present android/src/main/cpp/vdb_module.cpp     "named by CMakeLists — absent in 0.1.0"
present ios/Vdb.h                               "globbed by the podspec — absent in 0.1.0"
present ios/Vdb.mm                              "globbed by the podspec — absent in 0.1.0"

echo
echo "every Java source the config registers"
package_import=$(grep -o "import [a-zA-Z0-9_.]*;" "$ROOT/react-native.config.js" | sed 's/import //; s/;//')
if [[ -z "$package_import" ]]; then
  echo "  FAIL react-native.config.js names no package class"
  failures=$((failures + 1))
else
  java_path="android/src/main/java/$(echo "$package_import" | tr . /).java"
  present "$java_path" "packageImportPath in react-native.config.js"
fi

echo
echo "every source CMakeLists names"
# The add_library block, minus the target name and the SHARED keyword.
sources=$(sed -n '/^add_library(vdb SHARED/,/^)/p' "$ROOT/android/CMakeLists.txt" \
          | grep -oE '[^ ]+\.cpp' || true)
if [[ -z "$sources" ]]; then
  echo "  FAIL CMakeLists.txt lists no sources"
  failures=$((failures + 1))
fi
for source in $sources; do
  # Paths are relative to android/.
  resolved=$(cd "$ROOT/android" 2>/dev/null && realpath -m --relative-to="$ROOT" "$source" 2>/dev/null || echo "")
  if [[ -z "$resolved" ]]; then
    # BSD realpath has no --relative-to; fall back to resolving by hand.
    resolved="android/$source"
    while [[ "$resolved" == *"/../"* ]]; do
      resolved=$(echo "$resolved" | sed -E 's#[^/]+/\.\./##')
    done
  fi
  present "$resolved" "named by CMakeLists"
done

echo
echo "no test or build scaffolding shipped to users"
for unwanted in cpp/test_bridge.cpp test scripts; do
  if [[ -e "$ROOT/$unwanted" ]]; then
    echo "  FAIL $unwanted was shipped; it is not something an app needs"
    failures=$((failures + 1))
  else
    echo "  ok   $unwanted is not shipped"
  fi
done

echo
if [[ $RELEASE -eq 1 ]]; then
  echo "the engine, for every architecture the package claims"
  # The ABIs android/build.gradle lists in abiFilters. A missing one is not a link error at
  # build time but an UnsatisfiedLinkError on a device nobody tested.
  for abi in arm64-v8a armeabi-v7a x86_64; do
    present "android/jniLibs/$abi/libisha_vector_db_ffi.a" "Android $abi"
  done
  present "ios/Vdb.xcframework/Info.plist" "the iOS xcframework"
else
  echo "native libraries (skipped; pass --release to require them)"
  echo "  ..   android/jniLibs and ios/Vdb.xcframework are built at release time"
fi

echo
echo "the shipped header is the canonical one"
if diff -q "$ROOT/include/vdb.h" crates/isha-vector-db-ffi/include/vdb.h >/dev/null 2>&1; then
  echo "  ok   include/vdb.h matches crates/isha-vector-db-ffi/include/vdb.h"
else
  echo "  FAIL include/vdb.h differs from the canonical header, or is missing"
  failures=$((failures + 1))
fi

echo
if [[ $failures -eq 0 ]]; then
  echo "React Native package: OK"
else
  echo "React Native package: $failures problem(s)"
  echo
  echo "A package missing a file its own build names cannot be installed by anyone. This is what"
  echo "happened to 0.1.0; see sdk/react-native/README.md."
  exit 1
fi
