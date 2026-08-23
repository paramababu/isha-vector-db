#!/usr/bin/env bash
# Build the Node addon and place it beside the JavaScript that loads it.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release -p isha-vector-db-node

case "$(uname -s)" in
  Darwin) EXT="dylib" ;;
  Linux)  EXT="so" ;;
  *)      EXT="dll" ;;
esac
SRC="target/release/libisha_vector_db_node.${EXT}"
[[ "$EXT" == "dll" ]] && SRC="target/release/isha_vector_db_node.dll"
[[ -f "$SRC" ]] || { echo "FAIL: $SRC was not produced"; exit 1; }

cp "$SRC" sdk/node/vdb.node
echo "built sdk/node/vdb.node ($(wc -c < sdk/node/vdb.node) bytes)"
