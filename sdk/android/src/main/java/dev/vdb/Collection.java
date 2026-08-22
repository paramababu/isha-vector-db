package dev.vdb;

import java.util.ArrayList;
import java.util.List;

/** A handle to one collection. */
public final class Collection implements AutoCloseable {
  private long handle;
  private final String name;
  private final int dimension;

  Collection(long handle, String name, int dimension) {
    this.handle = handle;
    this.name = name;
    this.dimension = dimension;
  }

  /** The collection's name. */
  public String name() {
    return name;
  }

  /** Its vector dimension, or -1 when the collection was opened rather than created. */
  public int dimension() {
    return dimension;
  }

  /** Insert or replace. Returns true when the document was new. */
  public boolean upsert(String id, float[] vector) {
    return Native.upsert(alive(), id, vector);
  }

  /** Remove a document. Returns whether it existed; removing an absent one is not an error. */
  public boolean delete(String id) {
    return Native.delete(alive(), id);
  }

  /** Whether a document exists. */
  public boolean contains(String id) {
    return Native.contains(alive(), id);
  }

  /** Live documents. */
  public long count() {
    return Native.count(alive());
  }

  /** Fold buffered writes into a segment. */
  public void flush() {
    Native.flushCollection(alive());
  }

  /**
   * Find the nearest documents.
   *
   * <p>Ordered by score descending, ties broken by ascending id. Scores are always
   * higher-is-better, whatever the metric.
   */
  public List<Hit> search(float[] query, int topK) {
    long results = Native.search(alive(), query, topK);
    try {
      int n = Native.resultCount(results);
      List<Hit> hits = new ArrayList<>(n);
      for (int i = 0; i < n; i++) {
        hits.add(new Hit(Native.resultId(results, i), Native.resultScore(results, i)));
      }
      return hits;
    } finally {
      // Freed here rather than left to the collector: a search result holds engine memory, and
      // a loop of searches would otherwise accumulate it until a GC nobody scheduled.
      Native.freeResult(results);
    }
  }

  /** Release the handle. The collection itself is unaffected. Idempotent. */
  @Override
  public void close() {
    if (handle != 0) {
      long h = handle;
      handle = 0;
      Native.freeCollection(h);
    }
  }

  private long alive() {
    if (handle == 0) {
      throw new VdbException("the collection is closed");
    }
    return handle;
  }

  /** One search result. */
  public static final class Hit {
    private final String id;
    private final float score;

    Hit(String id, float score) {
      this.id = id;
      this.score = score;
    }

    /** The document's id. */
    public String id() {
      return id;
    }

    /** Its score. Always higher-is-better. */
    public float score() {
      return score;
    }

    @Override
    public String toString() {
      return id + "@" + score;
    }
  }
}
