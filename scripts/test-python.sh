#!/usr/bin/env bash
# Build the engine and run the Python binding's tests against it.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building the shared library"
cargo build -p isha-vector-db-ffi --release

echo "running the Python tests"
python3 -m unittest discover -s sdk/python/tests "$@"
