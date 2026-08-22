package dev.vdb;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.List;

/**
 * The Java API, driven against a real JVM.
 *
 * <p>Deliberately a plain {@code main} rather than JUnit: it runs on a desktop JVM with nothing
 * installed but a JDK, which is what makes it possible to test the JNI boundary here at all
 * rather than only inside an Android emulator. The Android instrumented tests will cover what
 * only a device can — asset paths, the storage sandbox, 16 KB pages — and they will be slower and
 * rarer. This is the loop that catches JNI mistakes in seconds.
 */
public final class SmokeTest {
  private static int failures = 0;

  public static void main(String[] args) throws Exception {
    Path dir = Files.createTempDirectory("vdb-java-test");
    try {
      lifecycle(dir.resolve("lifecycle").toString());
      errorsBecomeExceptions(dir.resolve("errors").toString());
      closingIsIdempotent(dir.resolve("closing").toString());
      lockIsExclusive(dir.resolve("lock").toString());
      persistence(dir.resolve("persist").toString());
      metadataAndFilters(dir.resolve("filters").toString());
    } finally {
      deleteRecursively(dir);
    }
    if (failures > 0) {
      System.out.println(failures + " check(s) failed");
      System.exit(1);
    }
    System.out.println("all checks passed");
  }

  static void lifecycle(String path) {
    try (Database db = Vdb.open(path);
        Collection c = db.collection("docs", 3)) {
      check("name", c.name().equals("docs"));
      check("dimension", c.dimension() == 3);

      check("insert reports new", c.upsert("east", new float[] {1, 0, 0}));
      check("replace reports not new", !c.upsert("east", new float[] {1, 0, 0}));
      c.upsert("north", new float[] {0, 1, 0});
      check("count", c.count() == 2);
      check("contains", c.contains("east"));

      List<Collection.Hit> hits = c.search(new float[] {0.9f, 0.1f, 0}, 2);
      check("two hits", hits.size() == 2);
      check("nearest is east", hits.get(0).id().equals("east"));
      check("ordered by score", hits.get(0).score() > hits.get(1).score());

      check("delete reports existed", c.delete("east"));
      check("deleting twice is a no-op", !c.delete("east"));
      check("count after delete", c.count() == 1);

      check("collections listed", db.listCollections().equals(List.of("docs")));
    }
  }

  /** Every engine failure must arrive as an exception carrying its stable code. */
  static void errorsBecomeExceptions(String path) {
    try (Database db = Vdb.open(path);
        Collection c = db.collection("docs", 3)) {
      VdbException e = expectThrow("wrong dimension", () -> c.upsert("a", new float[] {1, 0}));
      check("code is parsed", e.code() == 4003);
      check("message names the collection", e.getMessage().contains("docs"));

      expectThrow("wrong dimension on search", () -> c.search(new float[] {1, 0}, 1));
      expectThrow("unknown collection", () -> db.openCollection("nope"));
      expectThrow("zero topK", () -> c.search(new float[] {1, 0, 0}, 0));

      // A failed write must leave nothing behind.
      check("nothing written by a rejected upsert", c.count() == 0);
    }
  }

  static void closingIsIdempotent(String path) {
    Database db = Vdb.open(path);
    db.close();
    db.close();
    check("closed database reports it", !db.isOpen());
    expectThrow("using a closed database", () -> db.collection("docs", 2));
  }

  /** The lock must be released by close, or a killed and restarted app cannot reopen. */
  static void lockIsExclusive(String path) {
    Database first = Vdb.open(path);
    expectThrow("second writer", () -> Vdb.open(path));
    first.close();
    Vdb.open(path).close();
  }

  static void persistence(String path) {
    try (Database db = Vdb.open(path);
        Collection c = db.collection("docs", 2)) {
      c.upsert("kept", new float[] {1, 0});
    }
    try (Database db = Vdb.open(path);
        Collection c = db.openCollection("docs")) {
      check("survived a reopen", c.contains("kept"));
      check("count survived", c.count() == 1);
    }
  }

  /** Metadata written from Java, and filters read back through it. */
  static void metadataAndFilters(String path) {
    try (Database db = Vdb.open(path);
        Collection c = db.collection("docs", 2)) {
      // Decreasing similarity to {1, 0}, so filtering is visible separately from ranking.
      c.upsert("hammer", new float[] {1, 0},
          Metadata.of().set("category", "tools").set("price", 25.0).set("sale", true));
      c.upsert("saw", new float[] {0.95f, 0.31f},
          Metadata.of().set("category", "tools").set("price", 75.0));
      c.upsert("ball", new float[] {0.7f, 0.7f}, Metadata.of().set("category", "toys"));

      check("unfiltered order", ids(c, null).equals(List.of("hammer", "saw", "ball")));
      check("simple filter", ids(c, Filter.eq("category", "tools")).equals(List.of("hammer", "saw")));

      Filter cheapTools = Filter.eq("category", "tools").and(Filter.lt("price", 50.0));
      check("conjunction", ids(c, cheapTools).equals(List.of("hammer")));

      Filter either = Filter.eq("category", "toys").or(Filter.gt("price", 50.0));
      check("disjunction", ids(c, either).equals(List.of("saw", "ball")));

      check("negation", ids(c, Filter.not(Filter.eq("category", "tools"))).equals(List.of("ball")));

      // Three levels, in one expression.
      Filter nested = Filter.any(
          Filter.all(Filter.eq("category", "tools"),
              Filter.any(Filter.lt("price", 50.0), Filter.eq("sale", true))),
          Filter.eq("category", "toys"));
      check("deep nesting", ids(c, nested).equals(List.of("hammer", "ball")));

      // "ball" has no price.
      check("exists", ids(c, Filter.exists("price")).equals(List.of("hammer", "saw")));
      check("isNull", ids(c, Filter.isNull("price")).equals(List.of("ball")));
      check("ne matches absent", ids(c, Filter.ne("price", 25L)).equals(List.of("saw", "ball")));

      check("startsWith", ids(c, Filter.startsWith("category", "too")).equals(List.of("hammer", "saw")));
      // A type mismatch is false, never an error.
      check("type mismatch is empty", ids(c, Filter.eq("category", 1L)).isEmpty());

      check("empty all matches everything", ids(c, Filter.all()).size() == 3);
      check("empty any matches nothing", ids(c, Filter.any()).isEmpty());

      // topK counts matches, not candidates.
      check("topK counts matches",
          c.search(new float[] {1, 0}, 2, Filter.eq("category", "toys")).size() == 1);

      // And after a flush, out of a segment rather than the buffer.
      c.flush();
      check("filters after flush", ids(c, Filter.eq("category", "tools")).equals(List.of("hammer", "saw")));
    }
  }

  static List<String> ids(Collection c, Filter filter) {
    List<Collection.Hit> hits =
        filter == null ? c.search(new float[] {1, 0}, 10) : c.search(new float[] {1, 0}, 10, filter);
    List<String> out = new java.util.ArrayList<>(hits.size());
    for (Collection.Hit hit : hits) {
      out.add(hit.id());
    }
    return out;
  }

  // ---- tiny harness -------------------------------------------------------

  interface Body {
    void run();
  }

  static VdbException expectThrow(String label, Body body) {
    try {
      body.run();
    } catch (VdbException e) {
      System.out.println("  ok   " + label + " threw: " + shorten(e.getMessage()));
      return e;
    }
    failures++;
    System.out.println("  FAIL " + label + " did not throw");
    return new VdbException("");
  }

  static void check(String label, boolean ok) {
    if (ok) {
      System.out.println("  ok   " + label);
    } else {
      failures++;
      System.out.println("  FAIL " + label);
    }
  }

  static String shorten(String s) {
    return s == null ? "" : (s.length() > 60 ? s.substring(0, 60) + "…" : s);
  }

  static void deleteRecursively(Path root) throws IOException {
    if (!Files.exists(root)) {
      return;
    }
    try (var walk = Files.walk(root)) {
      walk.sorted(Comparator.reverseOrder()).forEach(p -> p.toFile().delete());
    }
  }
}
