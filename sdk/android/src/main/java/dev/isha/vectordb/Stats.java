package dev.isha.vectordb;

/** Counters for a collection. */
public final class Stats {
  /** Live documents. */
  public final long liveDocuments;
  /** Rows on disk, tombstones included. */
  public final long totalRows;
  /** Segments on disk. */
  public final long segments;
  /** Documents written but not yet folded into a segment. */
  public final long bufferedDocuments;
  /** Fraction of rows that are tombstones, 0 to 1. The number that says whether to compact. */
  public final double deadRatio;
  /** Vector dimension. */
  public final int dimension;

  Stats(long[] packed) {
    if (packed.length < 6) {
      throw new VdbException("malformed stats from the native layer");
    }
    this.liveDocuments = packed[0];
    this.totalRows = packed[1];
    this.segments = packed[2];
    this.bufferedDocuments = packed[3];
    // Carried across JNI as thousandths, since a long[] has no room for a float.
    this.deadRatio = packed[4] / 1000.0;
    this.dimension = (int) packed[5];
  }

  @Override
  public String toString() {
    return "Stats{live=" + liveDocuments + ", rows=" + totalRows + ", segments=" + segments
        + ", buffered=" + bufferedDocuments + ", dead=" + deadRatio + "}";
  }
}
