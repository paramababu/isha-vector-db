package dev.isha.vectordb;

import java.util.Arrays;
import java.util.List;

/**
 * An open database.
 *
 * <p>{@link AutoCloseable}, so a try-with-resources block releases the lock even when an
 * exception unwinds past the code that would have closed it. On Android that matters more than
 * usual: a held lock survives until the process dies, and a process that is killed and restarted
 * would otherwise find its own database unopenable.
 */
public final class Database implements AutoCloseable {
  private long handle;

  Database(long handle) {
    this.handle = handle;
  }

  /** Create a collection, or open it if one exists with a matching shape. */
  public Collection collection(String name, int dimension, Vdb.Metric metric) {
    return new Collection(
        Native.openCollection(alive(), name, dimension, metric.value, true), name, dimension);
  }

  /** Create a cosine collection, or open a matching one. */
  public Collection collection(String name, int dimension) {
    return collection(name, dimension, Vdb.Metric.COSINE);
  }

  /** Open an existing collection. */
  public Collection openCollection(String name) {
    return new Collection(Native.openCollection(alive(), name, 0, 0, false), name, -1);
  }

  /** Delete a collection and everything in it. Irreversible. */
  public void dropCollection(String name) {
    Native.dropCollection(alive(), name);
  }

  /** Every collection's name, sorted. */
  public List<String> listCollections() {
    return Arrays.asList(Native.listCollections(alive()));
  }

  /** Fold every collection's buffered writes into segments. */
  public void flush() {
    Native.flushDatabase(alive());
  }

  /** Whether the handle is still usable. */
  public boolean isOpen() {
    return handle != 0;
  }

  /** Flush and close, releasing the lock. Idempotent, so a finally block cannot double-throw. */
  @Override
  public void close() {
    if (handle != 0) {
      long h = handle;
      handle = 0;
      Native.closeDatabase(h);
    }
  }

  /**
   * Reclaim the space held by tombstoned rows, returning how many were removed.
   *
   * <p>Explicit rather than automatic: rewriting hundreds of megabytes is a decision about when
   * to spend I/O and battery, and an application knows more about that than the engine does. On
   * Android the obvious moment is a {@code WorkManager} job constrained to charging and idle.
   * Use {@link Collection#stats()}'s {@code deadRatio} to decide whether it is worth it.
   *
   * @param minDeadRatio how dead a segment must be before it is rewritten; 0 rewrites everything
   */
  public long compact(double minDeadRatio) {
    return Native.compact(alive(), minDeadRatio);
  }

  /** Reclaim space from segments that are at least 30% tombstones. */
  public long compact() {
    return compact(0.3);
  }

  /** Check the database's integrity. Reports rather than repairs. */
  public VerifyReport verify(Vdb.VerifyLevel level) {
    return new VerifyReport(Native.verify(alive(), level.value));
  }

  /** Check checksums. */
  public VerifyReport verify() {
    return verify(Vdb.VerifyLevel.CHECKSUMS);
  }

  private long alive() {
    if (handle == 0) {
      throw new VdbException("the database is closed");
    }
    return handle;
  }
}
