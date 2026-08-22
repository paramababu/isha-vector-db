/*
 * A real C program against the real library.
 *
 * The Rust tests in tests/abi.rs exercise the same symbols, but from inside the crate that
 * defines them — so they cannot catch a header that does not compile, a type that C spells
 * differently, or a missing include. This can, and it is what scripts/check-c-abi.sh runs.
 *
 *   cc -I crates/vdb-ffi/include examples/smoke.c target/release/libvdb_ffi.a -o smoke
 */

#include "vdb.h"
#include <stdio.h>
#include <string.h>

int main(void) {
  printf("abi=%u format=%u version=%s\n", vdb_abi_version(), vdb_format_version(), vdb_version());

  const char *path = "/tmp/vdb-c-smoke";
  vdb_db_t *db = NULL; vdb_error_t *err = NULL;
  if (vdb_open((const uint8_t *)path, strlen(path), true, false, VDB_DURABILITY_BATCH, &db, &err)) {
    printf("open failed: %s\n", vdb_error_message(err)); return 1;
  }
  vdb_collection_t *c = NULL;
  if (vdb_collection_create(db, (const uint8_t *)"docs", 4, 3, VDB_METRIC_COSINE, false, &c, &err)) {
    printf("create failed: %s\n", vdb_error_message(err)); return 1;
  }
  float east[3] = {1, 0, 0}, north[3] = {0, 1, 0};
  vdb_upsert(c, (const uint8_t *)"east", 4, east, 3, NULL, NULL, &err);
  vdb_upsert(c, (const uint8_t *)"north", 5, north, 3, NULL, NULL, &err);

  float q[3] = {0.9f, 0.1f, 0};
  vdb_results_t *r = NULL;
  if (vdb_search(c, q, 3, 1, &r, &err)) { printf("search failed: %s\n", vdb_error_message(err)); return 1; }
  size_t len = 0;
  const uint8_t *id = vdb_results_id(r, 0, &len);
  printf("nearest=%.*s score=%.4f\n", (int)len, id, vdb_results_score(r, 0));
  vdb_results_free(r);

  /* an engine error, reported through the boundary */
  float wrong[2] = {1, 0};
  int32_t rc = vdb_upsert(c, (const uint8_t *)"bad", 3, wrong, 2, NULL, NULL, &err);
  printf("wrong dimension: rc=%d code=%u msg=%s\n", rc, vdb_error_code(err), vdb_error_message(err));
  vdb_error_free(err); err = NULL;

  vdb_collection_free(c);
  vdb_close(db, &err);
  return 0;
}
