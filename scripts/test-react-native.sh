#!/usr/bin/env bash
# Build and run everything in the React Native SDK that can be checked without a device.
#
# Three layers, and it is worth being precise about what each one proves:
#
#   1. cpp/vdb_bridge.cpp   compiled AND RUN against the real engine. This is where the logic
#                           lives, so this is the check that can catch a wrong answer.
#   2. cpp/vdb_jsi.cpp      COMPILED against React Native's real JSI headers, at both ends of
#                           the supported version range. Catches a signature that moved or a
#                           conversion that does not type-check; cannot catch a conversion that
#                           type-checks and is wrong, which is why layer 1 holds the logic.
#   3. src/*.js             run in Node against a mock host object: argument validation, the
#                           filter and metadata compilers, handle-state rules.
#
# What is still not covered: the JSI layer actually executing, and the iOS and Android packaging.
# Those need an app. sdk/react-native/README.md says so plainly.
set -euo pipefail
cd "$(dirname "$0")/.."

# The two ends of the range package.json claims to support. Testing the bracket rather than two
# adjacent minors is deliberate: JSI moves between major React Native versions, and the failure
# this guards against is the oldest supported version quietly stopping working.
#
# Bump these when the supported range in sdk/react-native/package.json changes.
RN_VERSIONS=("0.73.11" "0.87.1")

OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

echo "building the static library"
cargo build -p isha-vector-db-ffi --release

case "$(uname -s)" in
  Darwin) LINK=(-framework CoreFoundation -framework Security) ;;
  *)      LINK=(-lpthread -ldl -lm) ;;
esac

echo
echo "1/3  compiling and running the bridge against the real engine"
clang++ -std=c++17 -Wall -Wextra -Werror -O1 \
  -I crates/isha-vector-db-ffi/include \
  -I sdk/react-native/cpp \
  sdk/react-native/cpp/vdb_bridge.cpp \
  sdk/react-native/cpp/test_bridge.cpp \
  target/release/libisha_vector_db_ffi.a \
  "${LINK[@]}" \
  -o "$OUT/test_bridge"
"$OUT/test_bridge"

echo
echo "2/3  compiling the JSI layer against React Native's own headers"
for version in "${RN_VERSIONS[@]}"; do
  headers="$OUT/rn-$version"
  mkdir -p "$headers"
  # Only ReactCommon/jsi is extracted: the rest of the react-native tarball is 23 MB of things
  # this does not need, and installing the package would pull a dependency tree with it.
  url=$(npm view "react-native@$version" dist.tarball)
  curl -sSL "$url" | tar xz -C "$headers" --strip-components=3 package/ReactCommon/jsi

  printf '     react-native %s ... ' "$version"
  clang++ -std=c++17 -Wall -Wextra -Werror -fsyntax-only \
    -I "$headers" \
    -I crates/isha-vector-db-ffi/include \
    -I sdk/react-native/cpp \
    sdk/react-native/cpp/vdb_jsi.cpp
  echo "ok"
done

echo
echo "3/3  running the JavaScript tests"
cd sdk/react-native && node --test test/api.test.js test/filter.test.js
