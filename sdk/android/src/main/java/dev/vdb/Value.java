package dev.vdb;

/**
 * A scalar metadata value.
 *
 * <p>Flat scalars only for now. Nested objects and arrays need a recursive conversion whose type
 * rules deserve deciding once and sharing across every SDK, rather than being improvised here
 * and then diverging in the next one.
 */
public final class Value {
  final int kind;
  final String text;
  final long number;
  final double real;
  final boolean flag;

  private Value(int kind, String text, long number, double real, boolean flag) {
    this.kind = kind;
    this.text = text;
    this.number = number;
    this.real = real;
    this.flag = flag;
  }

  public static Value of(String value) {
    if (value == null) {
      return ofNull();
    }
    return new Value(Native.VALUE_STRING, value, 0, 0, false);
  }

  public static Value of(long value) {
    return new Value(Native.VALUE_I64, null, value, 0, false);
  }

  public static Value of(double value) {
    return new Value(Native.VALUE_F64, null, 0, value, false);
  }

  public static Value of(boolean value) {
    return new Value(Native.VALUE_BOOL, null, 0, 0, value);
  }

  public static Value ofNull() {
    return new Value(Native.VALUE_NULL, null, 0, 0, false);
  }

  /** Write this value into a native builder under `key`. */
  void setOn(long metadata, String key) {
    Native.metadataSet(metadata, key, kind, text, number, real, flag);
  }

  /** Push a comparison against this value onto a native filter builder. */
  void compareOn(long filter, String field, int op) {
    Native.filterCompare(filter, field, op, kind, text, number, real, flag);
  }
}
