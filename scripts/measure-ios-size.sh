#!/usr/bin/env bash
# What linking the engine actually adds to an application.
#
# The static archive is around 20 MB, which looks alarming and is not the number that matters:
# an archive holds every object file with its symbols, and the linker keeps only what is
# reachable. This measures the difference between a binary that calls the engine and one that
# does not — which is the figure an app-size budget is about.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${1:-aarch64-apple-darwin}"
LIB="target/$TARGET/release/libvdb_ffi.a"
[[ -f "$LIB" ]] || { echo "FAIL: build $LIB first (scripts/build-xcframework.sh)"; exit 1; }

WORK="$(mktemp -d)"
cat > "$WORK/empty.c" <<'EOF'
int main(void) { return 0; }
EOF
cat > "$WORK/uses.c" <<'EOF'
#include "vdb.h"
#include <string.h>
/* Reference the whole surface, so nothing is discarded that a real application would keep. */
int main(void) {
  vdb_db_t *db = NULL; vdb_collection_t *c = NULL; vdb_results_t *r = NULL; vdb_error_t *e = NULL;
  const char *p = "/tmp/x"; float v[2] = {1, 0};
  vdb_open((const uint8_t *)p, strlen(p), true, false, VDB_DURABILITY_BATCH, &db, &e);
  vdb_collection_create(db, (const uint8_t *)"c", 1, 2, VDB_METRIC_COSINE, false, &c, &e);
  vdb_upsert(c, (const uint8_t *)"a", 1, v, 2, NULL, NULL, &e);
  vdb_search(c, v, 2, 1, &r, &e);
  vdb_results_len(r); vdb_results_free(r); vdb_collection_free(c); vdb_close(db, &e);
  return (int)vdb_abi_version();
}
EOF

cc -Os "$WORK/empty.c" -o "$WORK/empty" -Wl,-dead_strip

# Both configurations, because the difference is large enough to mislead either way. Xcode
# passes -dead_strip for release builds, so the stripped figure is the one an application
# actually pays; the other is what you get if you link the archive by hand and forget.
cc -Os -I crates/vdb-ffi/include "$WORK/uses.c" "$LIB" -o "$WORK/uses" -Wl,-dead_strip \
   -framework Security -framework CoreFoundation
cc -Os -I crates/vdb-ffi/include "$WORK/uses.c" "$LIB" -o "$WORK/whole" \
   -framework Security -framework CoreFoundation
strip -x "$WORK/empty" "$WORK/uses" "$WORK/whole" 2>/dev/null

EMPTY=$(wc -c < "$WORK/empty" | tr -d ' ')
USES=$(wc -c < "$WORK/uses" | tr -d ' ')
WHOLE=$(wc -c < "$WORK/whole" | tr -d ' ')
DELTA=$((USES - EMPTY))
printf 'target:                        %s\n' "$TARGET"
printf 'baseline binary:               %s bytes\n' "$EMPTY"
printf 'with the engine, dead-stripped: %s bytes\n' "$USES"
printf 'without dead-stripping:        %s bytes\n' "$WHOLE"
printf 'engine contributes:            %s bytes (%.2f MB)\n' "$DELTA" "$(echo "scale=4; $DELTA/1048576" | bc)"

LIMIT=$((1500 * 1024))
if [[ $DELTA -gt $LIMIT ]]; then
  echo "FAIL: over the 1.5 MB budget from docs/architecture/01-scope.md §1.3"
  exit 1
fi
echo "within the 1.5 MB budget"
