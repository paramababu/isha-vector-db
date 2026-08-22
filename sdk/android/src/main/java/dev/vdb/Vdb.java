package dev.vdb;

/** Constants and entry points. */
public final class Vdb {
  private Vdb() {}

  /** How aggressively writes are made durable. */
  public enum Durability {
    /** Sync every write. Safe against power loss; slow on flash. */
    FULL(1),
    /**
     * Sync on batch commit, flush and close. The default.
     *
     * <p>In every mode a process crash loses nothing — the bytes are already in the page cache.
     * Only power loss can lose an unsynced write. On Android the system kills applications
     * routinely and power loss is rare, so this is the sensible default rather than FULL.
     */
    BATCH(2),
    /** Sync on flush and close only. For bulk import. */
    RELAXED(3);

    final int value;

    Durability(int value) {
      this.value = value;
    }
  }

  /** Similarity metric. */
  public enum Metric {
    /** Cosine similarity. Ignores magnitude, which is usually what embeddings want. */
    COSINE(1),
    /** Euclidean distance. */
    L2(2),
    /**
     * Inner product. Rewards magnitude as well as direction, so a longer vector can outrank an
     * exact match. That is what the inner product means, not a defect.
     */
    DOT(3);

    final int value;

    Metric(int value) {
      this.value = value;
    }
  }

  /** How thoroughly to check a database. */
  public enum VerifyLevel {
    /** Headers and the manifest. Milliseconds, whatever the size. */
    QUICK(1),
    /** Every block's checksum. Reads every byte. */
    CHECKSUMS(2),
    /** Checksums plus cross-file consistency. */
    FULL(3);

    final int value;

    VerifyLevel(int value) {
      this.value = value;
    }
  }

  /** Open or create a database at a directory path. */
  public static Database open(String path) {
    return open(path, true, false, Durability.BATCH);
  }

  /**
   * Open or create a database.
   *
   * <p>A read-only handle takes no lock, so it can inspect a database another process has open.
   * {@code createIfMissing} is ignored when {@code readOnly} is set, since creating requires
   * writing.
   */
  public static Database open(
      String path, boolean createIfMissing, boolean readOnly, Durability durability) {
    long handle = Native.openDatabase(path, createIfMissing, readOnly, durability.value);
    return new Database(handle);
  }
}
