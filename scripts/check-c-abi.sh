#!/usr/bin/env bash
# Compile and run a C program against the built library.
#
# The Rust ABI tests live inside the crate that defines the symbols, so they cannot catch a
# header that does not compile or a declaration C spells differently. This can.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building the static library…"
cargo build --release -p isha-vector-db-ffi

LIB="target/release/libisha_vector_db_ffi.a"
[[ -f "$LIB" ]] || { echo "FAIL: $LIB was not produced"; exit 1; }

# Rust's std needs a few system libraries when linked into a C binary.
case "$(uname -s)" in
  Darwin) EXTRA=(-framework Security -framework CoreFoundation) ;;
  Linux)  EXTRA=(-lpthread -ldl -lm) ;;
  *)      EXTRA=() ;;
esac

OUT="$(mktemp -d)/smoke"
echo "compiling examples/smoke.c against include/vdb.h…"
cc -Wall -Wextra -Werror -std=c11 \
   -I crates/isha-vector-db-ffi/include \
   crates/isha-vector-db-ffi/examples/smoke.c "$LIB" -o "$OUT" "${EXTRA[@]}"

rm -rf /tmp/vdb-c-smoke
echo "running…"
"$OUT"
echo "C ABI: OK"
