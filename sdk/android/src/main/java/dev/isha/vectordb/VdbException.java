package dev.isha.vectordb;

/**
 * A failure from the database.
 *
 * <p>The message begins with a stable code, {@code VDB-nnnn}, documented in
 * {@code docs/api/error-codes.md}. Match on {@link #code()} rather than on the prose, which is
 * allowed to change.
 */
public final class VdbException extends RuntimeException {
  private static final long serialVersionUID = 1L;

  public VdbException(String message) {
    super(message);
  }

  /**
   * The stable numeric code, or 0 if this failure did not come from the engine — a bad argument
   * caught at the boundary, for instance.
   */
  public int code() {
    String message = getMessage();
    if (message == null || !message.startsWith("[VDB-")) {
      return 0;
    }
    int end = message.indexOf(']');
    if (end < 5) {
      return 0;
    }
    try {
      return Integer.parseInt(message.substring(5, end));
    } catch (NumberFormatException e) {
      return 0;
    }
  }
}
