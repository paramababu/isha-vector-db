package dev.vdb;

/**
 * The raw JNI surface. Not public API — use {@link Vdb}, {@link Database} and
 * {@link Collection}.
 *
 * <p>Handles are {@code long}s. The alternative, a Java object owning native state, hands the
 * decision of when a database closes to the garbage collector, which is to say to nobody.
 */
final class Native {
  private Native() {}

  static {
    // On Android the library is unpacked from the APK and found by name. Elsewhere — a desktop
    // JVM running these tests — the path is given explicitly, because there is no APK.
    String explicit = System.getProperty("vdb.library.path");
    if (explicit != null) {
      System.load(explicit);
    } else {
      System.loadLibrary("vdb_jni");
    }
  }

  static native long openDatabase(String path, boolean createIfMissing, boolean readOnly, int durability);

  static native void closeDatabase(long db);

  static native void flushDatabase(long db);

  static native String[] listCollections(long db);

  static native long openCollection(long db, String name, int dimension, int metric, boolean create);

  static native void freeCollection(long collection);

  static native void dropCollection(long db, String name);

  static native boolean upsert(long collection, String id, float[] vector);

  static native boolean delete(long collection, String id);

  static native boolean contains(long collection, String id);

  static native long count(long collection);

  static native void flushCollection(long collection);

  static native long search(long collection, float[] query, int topK);

  static native int resultCount(long results);

  static native String resultId(long results, int index);

  static native float resultScore(long results, int index);

  static native void freeResult(long results);

  // Value kinds, shared with the Rust side. One `metadataSet` and one `filterCompare` taking a
  // tag rather than four near-identical natives each: JNI declarations are verbose on both
  // sides, and a tag is cheaper to read than four signatures are to keep in step.
  static final int VALUE_STRING = 1;
  static final int VALUE_I64 = 2;
  static final int VALUE_F64 = 3;
  static final int VALUE_BOOL = 4;
  static final int VALUE_NULL = 5;

  static native long metadataNew();

  static native void metadataFree(long metadata);

  static native void metadataSet(
      long metadata, String key, int kind, String text, long number, double real, boolean flag);

  static native boolean upsertWithMetadata(
      long collection, String id, float[] vector, long metadata);

  static native long filterNew();

  static native void filterFree(long filter);

  static native void filterCompare(
      long filter, String field, int op, int kind, String text, long number, double real,
      boolean flag);

  static native void filterUnary(long filter, String field, int predicate);

  static native void filterCombine(long filter, int combinator, int count);

  static native int filterDepth(long filter);

  static native long searchFiltered(long collection, float[] query, int topK, long filter);
}
