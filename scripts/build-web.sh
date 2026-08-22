#!/usr/bin/env bash
# Build the WebAssembly module and place it in the web SDK.
#
# No wasm-bindgen and no bundler: the SDK drives the same hand-written C ABI as every other
# binding, so a plain cargo build is the whole toolchain.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=wasm32-unknown-unknown

if ! rustup target list --installed | grep -q "$TARGET"; then
  echo "installing $TARGET"
  rustup target add "$TARGET"
fi

echo "building vdb-ffi for $TARGET"
cargo build -p vdb-ffi --target "$TARGET" --release

cp "target/$TARGET/release/vdb_ffi.wasm" sdk/web/vdb.wasm
SIZE=$(wc -c < sdk/web/vdb.wasm | tr -d ' ')
echo "sdk/web/vdb.wasm: $SIZE bytes"

# wasm-opt would shrink this considerably, but it is not required to build or ship, and adding a
# mandatory external tool to the build is the kind of dependency this project avoids.
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz sdk/web/vdb.wasm -o sdk/web/vdb.wasm
  echo "after wasm-opt -Oz: $(wc -c < sdk/web/vdb.wasm | tr -d ' ') bytes"
else
  echo "note: wasm-opt not installed; shipping the unoptimised module"
fi

echo "running the web SDK tests"
cd sdk/web && node --test test/wasm.test.js test/opfs.test.js
