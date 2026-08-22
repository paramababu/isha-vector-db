#!/usr/bin/env bash
# Build the static library for the host and run the Swift test suite against it.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building the static library for the host…"
cargo build --release -p vdb-ffi

cd sdk/ios
swift test 2>&1
