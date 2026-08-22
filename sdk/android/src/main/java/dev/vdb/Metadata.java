package dev.vdb;

import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Metadata to attach to a document.
 *
 * <p>Built in Java and handed over once at write time, rather than by a sequence of native calls
 * per field: a JNI crossing per field would make writing a document with six fields seven
 * crossings instead of two.
 */
public final class Metadata {
  private final Map<String, Value> fields = new LinkedHashMap<>();

  public static Metadata of() {
    return new Metadata();
  }

  public Metadata set(String key, String value) {
    fields.put(key, Value.of(value));
    return this;
  }

  public Metadata set(String key, long value) {
    fields.put(key, Value.of(value));
    return this;
  }

  public Metadata set(String key, double value) {
    fields.put(key, Value.of(value));
    return this;
  }

  public Metadata set(String key, boolean value) {
    fields.put(key, Value.of(value));
    return this;
  }

  public Metadata setNull(String key) {
    fields.put(key, Value.ofNull());
    return this;
  }

  public boolean isEmpty() {
    return fields.isEmpty();
  }

  /** Build a native metadata handle. The caller must free it. */
  long toNative() {
    long handle = Native.metadataNew();
    try {
      for (Map.Entry<String, Value> entry : fields.entrySet()) {
        entry.getValue().setOn(handle, entry.getKey());
      }
    } catch (RuntimeException e) {
      Native.metadataFree(handle);
      throw e;
    }
    return handle;
  }
}
