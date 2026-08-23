#!/usr/bin/env bash
# Build and run the React Native bridge's C++ tests.
#
# This covers the half of the native layer that can run on a development machine — and
# deliberately the half that holds the logic. `cpp/vdb_jsi.cpp` is what remains, and it needs a
# React Native app to compile at all; see sdk/react-native/README.md for what that means.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building the static library"
cargo build -p isha-vector-db-ffi --release

OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

echo "compiling the bridge and its tests"
clang++ -std=c++17 -Wall -Wextra -Werror -O1 \
  -I crates/isha-vector-db-ffi/include \
  -I sdk/react-native/cpp \
  sdk/react-native/cpp/vdb_bridge.cpp \
  sdk/react-native/cpp/test_bridge.cpp \
  target/release/libisha_vector_db_ffi.a \
  -framework CoreFoundation -framework Security \
  -o "$OUT/test_bridge" 2>/dev/null \
  || clang++ -std=c++17 -Wall -Wextra -Werror -O1 \
    -I crates/isha-vector-db-ffi/include \
    -I sdk/react-native/cpp \
    sdk/react-native/cpp/vdb_bridge.cpp \
    sdk/react-native/cpp/test_bridge.cpp \
    target/release/libisha_vector_db_ffi.a \
    -lpthread -ldl -lm \
    -o "$OUT/test_bridge"

echo "running"
"$OUT/test_bridge"

echo "running the JavaScript tests"
cd sdk/react-native && node --test test/api.test.js
