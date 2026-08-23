package dev.isha.vectordb;

/**
 * What verification found.
 *
 * <p>A report rather than an exception: a damaged database is a result an application has to
 * decide about, and deciding what to discard is not a choice a library should make on its
 * behalf — nor one it can offer from inside a throw.
 */
public final class VerifyReport {
  /** Problems meaning data is damaged or unreadable. */
  public final long errors;
  /** Things that are odd but not damage — orphan files, an unusually high dead ratio. */
  public final long warnings;

  VerifyReport(long[] packed) {
    if (packed.length < 2) {
      throw new VdbException("malformed verify report from the native layer");
    }
    this.errors = packed[0];
    this.warnings = packed[1];
  }

  /** Whether nothing was found wrong. */
  public boolean isClean() {
    return errors == 0;
  }

  @Override
  public String toString() {
    return "VerifyReport{errors=" + errors + ", warnings=" + warnings + "}";
  }
}
