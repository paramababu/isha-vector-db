#!/usr/bin/env bash
# Build Vdb.xcframework: device, simulator and macOS in one artifact.
#
# A static library rather than a dynamic one. Dynamic frameworks cost dyld time at launch, which
# an embedded database has no business spending, and static linking lets the linker discard the
# parts of the engine an application never calls.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="target/xcframework"
rm -rf "$OUT"
mkdir -p "$OUT"

# Size over speed for device builds: a mobile app-size budget is real, and the scan is
# memory-bandwidth-bound rather than instruction-bound, so `opt-level="z"` costs little.
export CARGO_PROFILE_RELEASE_OPT_LEVEL="${CARGO_PROFILE_RELEASE_OPT_LEVEL:-z}"

build() {
  local target="$1"
  echo "--- $target"
  rustup target add "$target" >/dev/null 2>&1 || true
  cargo build --release -p isha-vector-db-ffi --target "$target"
}

build aarch64-apple-ios
build aarch64-apple-ios-sim
build x86_64-apple-ios
build aarch64-apple-darwin

# The simulator slices must be one fat library: an xcframework cannot hold two slices for the
# same platform-and-variant, and Apple Silicon and Intel simulators are both "ios-simulator".
mkdir -p "$OUT/sim"
lipo -create \
  target/aarch64-apple-ios-sim/release/libisha_vector_db_ffi.a \
  target/x86_64-apple-ios/release/libisha_vector_db_ffi.a \
  -output "$OUT/sim/libisha_vector_db_ffi.a"

mkdir -p "$OUT/headers"
cp crates/isha-vector-db-ffi/include/vdb.h "$OUT/headers/"
cat > "$OUT/headers/module.modulemap" <<'MODMAP'
module CVdb {
    header "vdb.h"
    export *
}
MODMAP

xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libisha_vector_db_ffi.a -headers "$OUT/headers" \
  -library "$OUT/sim/libisha_vector_db_ffi.a" -headers "$OUT/headers" \
  -library target/aarch64-apple-darwin/release/libisha_vector_db_ffi.a -headers "$OUT/headers" \
  -output "$OUT/Vdb.xcframework"

echo
echo "built $OUT/Vdb.xcframework"
find "$OUT/Vdb.xcframework" -name '*.a' -exec sh -c 'printf "  %-52s %s bytes\n" "${1#*Vdb.xcframework/}" "$(wc -c < "$1" | tr -d " ")"' _ {} \;
echo
echo "Those are archive sizes, not what an application pays. A static archive holds every"
echo "object file with its symbols and debug info; the linker discards what is unreachable."
echo "Run scripts/measure-ios-size.sh for the figure that belongs in an app-size budget."
