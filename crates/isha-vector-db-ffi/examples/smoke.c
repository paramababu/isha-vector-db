/*
 * A real C program against the real library.
 *
 * The Rust tests in tests/abi.rs exercise the same symbols, but from inside the crate that
 * defines them — so they cannot catch a header that does not compile, a type that C spells
 * differently, or a missing include. This can, and it is what scripts/check-c-abi.sh runs.
 *
 *   cc -I crates/isha-vector-db-ffi/include examples/smoke.c target/release/libisha_vector_db_ffi.a -o smoke
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

  /* a filter, built on the stack in postfix order */
  vdb_metadata_t *m = vdb_metadata_new();
  vdb_metadata_set_string(m, (const uint8_t *)"kind", 4, (const uint8_t *)"axis", 4, &err);
  vdb_metadata_set_i64(m, (const uint8_t *)"order", 5, 1, &err);
  float up[3] = {0, 0, 1};
  vdb_upsert(c, (const uint8_t *)"up", 2, up, 3, m, NULL, &err);
  vdb_metadata_free(m);

  vdb_filter_t *f = vdb_filter_new();
  vdb_filter_compare_str(f, (const uint8_t *)"kind", 4, VDB_OP_EQ,
                         (const uint8_t *)"axis", 4, &err);
  vdb_filter_compare_i64(f, (const uint8_t *)"order", 5, VDB_OP_LTE, 5, &err);
  vdb_filter_combine(f, VDB_COMBINE_AND, 2, &err);
  printf("filter depth: %zu (1 means complete)\n", vdb_filter_depth(f));

  vdb_results_t *fr = NULL;
  if (vdb_search_filtered(c, q, 3, 10, f, &fr, &err)) {
    printf("filtered search failed: %s\n", vdb_error_message(err)); return 1;
  }
  printf("filtered hits: %zu", vdb_results_len(fr));
  for (size_t i = 0; i < vdb_results_len(fr); i++) {
    size_t n = 0;
    const uint8_t *hid = vdb_results_id(fr, i, &n);
    printf(" %.*s", (int)n, hid);
  }
  printf("\n");
  vdb_results_free(fr);
  vdb_filter_free(f);

  /* an engine error, reported through the boundary */
  float wrong[2] = {1, 0};
  int32_t rc = vdb_upsert(c, (const uint8_t *)"bad", 3, wrong, 2, NULL, NULL, &err);
  printf("wrong dimension: rc=%d code=%u msg=%s\n", rc, vdb_error_code(err), vdb_error_message(err));
  vdb_error_free(err); err = NULL;

  vdb_collection_free(c);
  vdb_close(db, &err);
  return 0;
}
