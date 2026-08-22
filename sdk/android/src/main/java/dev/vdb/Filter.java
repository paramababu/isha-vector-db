package dev.vdb;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * A metadata predicate.
 *
 * <p>A tree, because that is what a filter is. The native layer receives it as a postfix
 * sequence — the only shape JNI can take a tree in without reflection on every node — and this
 * class is what keeps that out of sight: callers write an expression and it is flattened on the
 * way down, so an unbalanced sequence is not something they can construct.
 *
 * <pre>{@code
 * Filter cheapTools = Filter.eq("category", "tools").and(Filter.lt("price", 50.0));
 * List<Collection.Hit> hits = docs.search(query, 10, cheapTools);
 * }</pre>
 *
 * <p>Evaluation is <b>total</b>: comparing a string to a number is {@code false}, never an
 * error, and a field no document has is absent. Three consequences surprise people, and each is
 * deliberate — an absent field equals null; {@link #ne} is the exact negation of {@link #eq} so
 * it matches absent fields; and {@link #gt} and {@link #lte} are <em>both</em> false where no
 * ordering is defined, so they are not negations of one another. See {@code docs/api/filters.md}.
 *
 * <p>Plain classes rather than sealed interfaces or records: this ships to Android, where those
 * need a recent API level or desugaring, and a filter type is not worth that constraint.
 */
public abstract class Filter {
  // Operators, mirroring the constants on the Rust side.
  private static final int OP_EQ = 1;
  private static final int OP_NE = 2;
  private static final int OP_GT = 3;
  private static final int OP_GTE = 4;
  private static final int OP_LT = 5;
  private static final int OP_LTE = 6;
  private static final int OP_STARTS_WITH = 7;
  private static final int OP_CONTAINS = 8;

  private static final int UNARY_EXISTS = 1;
  private static final int UNARY_IS_NULL = 2;

  private static final int COMBINE_AND = 1;
  private static final int COMBINE_OR = 2;
  private static final int COMBINE_NOT = 3;

  Filter() {}

  /** Flatten this node onto a native builder, children first. */
  abstract void encode(long builder);

  // ---- leaves ----

  public static Filter eq(String field, String value) {
    return new Compare(field, OP_EQ, Value.of(value));
  }

  public static Filter eq(String field, long value) {
    return new Compare(field, OP_EQ, Value.of(value));
  }

  public static Filter eq(String field, double value) {
    return new Compare(field, OP_EQ, Value.of(value));
  }

  public static Filter eq(String field, boolean value) {
    return new Compare(field, OP_EQ, Value.of(value));
  }

  /** The exact negation of {@link #eq}, so it matches documents lacking the field. */
  public static Filter ne(String field, String value) {
    return new Compare(field, OP_NE, Value.of(value));
  }

  public static Filter ne(String field, long value) {
    return new Compare(field, OP_NE, Value.of(value));
  }

  public static Filter gt(String field, double value) {
    return new Compare(field, OP_GT, Value.of(value));
  }

  public static Filter gte(String field, double value) {
    return new Compare(field, OP_GTE, Value.of(value));
  }

  public static Filter lt(String field, double value) {
    return new Compare(field, OP_LT, Value.of(value));
  }

  public static Filter lte(String field, double value) {
    return new Compare(field, OP_LTE, Value.of(value));
  }

  /** The field is a string with this prefix. */
  public static Filter startsWith(String field, String prefix) {
    return new Compare(field, OP_STARTS_WITH, Value.of(prefix));
  }

  /** The field is an <b>array</b> containing this value. Not a substring test. */
  public static Filter contains(String field, String value) {
    return new Compare(field, OP_CONTAINS, Value.of(value));
  }

  /** The field is present, including an explicit null. */
  public static Filter exists(String field) {
    return new Unary(field, UNARY_EXISTS);
  }

  /** The field is absent, or present and null. */
  public static Filter isNull(String field) {
    return new Unary(field, UNARY_IS_NULL);
  }

  // ---- combinators ----

  /** Every child must match. */
  public static Filter all(Filter... children) {
    return new Combine(COMBINE_AND, Arrays.asList(children));
  }

  /** At least one child must match. */
  public static Filter any(Filter... children) {
    return new Combine(COMBINE_OR, Arrays.asList(children));
  }

  public static Filter not(Filter child) {
    return new Combine(COMBINE_NOT, java.util.Collections.singletonList(child));
  }

  public Filter and(Filter other) {
    return all(this, other);
  }

  public Filter or(Filter other) {
    return any(this, other);
  }

  // ---- flattening ----

  /** Build a native filter handle. The caller must free it. */
  long toNative() {
    long handle = Native.filterNew();
    try {
      encode(handle);
      int depth = Native.filterDepth(handle);
      if (depth != 1) {
        // Unreachable through this API — the traversal guarantees it — but a wrong answer here
        // would be a filter that silently drops a clause, so it is checked rather than trusted.
        throw new VdbException("filter flattened to depth " + depth + ", expected 1");
      }
    } catch (RuntimeException e) {
      Native.filterFree(handle);
      throw e;
    }
    return handle;
  }

  private static final class Compare extends Filter {
    private final String field;
    private final int op;
    private final Value value;

    Compare(String field, int op, Value value) {
      this.field = field;
      this.op = op;
      this.value = value;
    }

    @Override
    void encode(long builder) {
      value.compareOn(builder, field, op);
    }
  }

  private static final class Unary extends Filter {
    private final String field;
    private final int predicate;

    Unary(String field, int predicate) {
      this.field = field;
      this.predicate = predicate;
    }

    @Override
    void encode(long builder) {
      Native.filterUnary(builder, field, predicate);
    }
  }

  private static final class Combine extends Filter {
    private final int combinator;
    private final List<Filter> children;

    Combine(int combinator, List<Filter> children) {
      this.combinator = combinator;
      this.children = new ArrayList<>(children);
    }

    @Override
    void encode(long builder) {
      if (children.isEmpty()) {
        // The identities: an empty `all` matches everything, an empty `any` matches nothing.
        // The native builder refuses a zero-count combine, where it is far more likely a
        // mistake, so these are written as an explicit tautology and contradiction.
        Native.filterUnary(builder, "", UNARY_IS_NULL);
        if (combinator == COMBINE_AND) {
          Native.filterUnary(builder, "", UNARY_IS_NULL);
          Native.filterCombine(builder, COMBINE_OR, 2);
        } else {
          Native.filterUnary(builder, "", UNARY_EXISTS);
          Native.filterCombine(builder, COMBINE_AND, 2);
        }
        return;
      }
      for (Filter child : children) {
        child.encode(builder);
      }
      Native.filterCombine(builder, combinator, children.size());
    }
  }
}
