import CVdb
import Foundation

/// A metadata predicate.
///
/// A tree, because that is what a filter is. The C ABI receives it as a postfix sequence — the
/// only shape C can take a tree in without thirty functions — and this type is what keeps that
/// out of sight: callers write an expression and it is flattened on the way down.
///
/// Evaluation is **total**. Comparing a string to a number is `false`, never an error; a field
/// no document has is absent. Three consequences surprise people, and each is deliberate:
///
/// - An absent field equals `.null`, so `.equals("x", .null)` matches a document without `x`.
///   Use ``exists(_:)`` when the distinction matters.
/// - ``notEquals(_:_:)`` is the exact negation of ``equals(_:_:)``, so it matches absent fields.
/// - ``greaterThan(_:_:)`` and ``lessThanOrEqual(_:_:)`` are *both* false where no ordering is
///   defined, so they are not negations of one another.
///
/// The full rules are in `docs/api/filters.md`.
public indirect enum Filter: Sendable {
    /// A value a filter can compare against.
    public enum Value: Sendable, Equatable {
        case string(String)
        case int(Int64)
        case double(Double)
        case bool(Bool)
    }

    case equals(String, Value)
    case notEquals(String, Value)
    case greaterThan(String, Value)
    case greaterThanOrEqual(String, Value)
    case lessThan(String, Value)
    case lessThanOrEqual(String, Value)
    /// The field is a string with this prefix.
    case startsWith(String, String)
    /// The field is an **array** containing this value. Not a substring test.
    case contains(String, Value)
    /// The field is present, including an explicit null.
    case exists(String)
    /// The field is absent, or present and null.
    case isNull(String)

    /// Every child must match. An empty `all` matches everything.
    case all([Filter])
    /// At least one child must match. An empty `any` matches nothing.
    case any([Filter])
    case not(Filter)

    /// Combine two filters, requiring both.
    public static func && (lhs: Filter, rhs: Filter) -> Filter { .all([lhs, rhs]) }
    /// Combine two filters, requiring either.
    public static func || (lhs: Filter, rhs: Filter) -> Filter { .any([lhs, rhs]) }
    /// Negate a filter.
    public static prefix func ! (operand: Filter) -> Filter { .not(operand) }
}

extension Filter {
    /// Flatten into the ABI's builder, in postfix order.
    ///
    /// A post-order walk: children first, then the combinator that consumes them. The builder's
    /// depth invariant — one expression left at the end — falls out of the traversal, so an
    /// unbalanced sequence is not something a caller of this API can construct.
    func encode(into builder: OpaquePointer) throws {
        switch self {
        case let .equals(field, value): try push(builder, field, VDB_OP_EQ, value)
        case let .notEquals(field, value): try push(builder, field, VDB_OP_NE, value)
        case let .greaterThan(field, value): try push(builder, field, VDB_OP_GT, value)
        case let .greaterThanOrEqual(field, value): try push(builder, field, VDB_OP_GTE, value)
        case let .lessThan(field, value): try push(builder, field, VDB_OP_LT, value)
        case let .lessThanOrEqual(field, value): try push(builder, field, VDB_OP_LTE, value)
        case let .startsWith(field, prefix):
            try push(builder, field, VDB_OP_STARTS_WITH, .string(prefix))
        case let .contains(field, value): try push(builder, field, VDB_OP_CONTAINS, value)

        case let .exists(field): try pushUnary(builder, field, VDB_UNARY_EXISTS)
        case let .isNull(field): try pushUnary(builder, field, VDB_UNARY_IS_NULL)

        case let .all(children):
            // An empty `all` matches everything, which the engine spells as an empty
            // conjunction — but the ABI refuses a zero-count combine, since at that level it is
            // far more likely to be a mistake. Expressed instead as a tautology.
            if children.isEmpty {
                try pushUnary(builder, "", VDB_UNARY_IS_NULL)
                try pushUnary(builder, "", VDB_UNARY_IS_NULL)
                try combine(builder, VDB_COMBINE_OR, 2)
                return
            }
            for child in children { try child.encode(into: builder) }
            try combine(builder, VDB_COMBINE_AND, children.count)

        case let .any(children):
            if children.isEmpty {
                // An empty `any` matches nothing: a field is both null and present.
                try pushUnary(builder, "", VDB_UNARY_IS_NULL)
                try pushUnary(builder, "", VDB_UNARY_EXISTS)
                try combine(builder, VDB_COMBINE_AND, 2)
                return
            }
            for child in children { try child.encode(into: builder) }
            try combine(builder, VDB_COMBINE_OR, children.count)

        case let .not(child):
            try child.encode(into: builder)
            try combine(builder, VDB_COMBINE_NOT, 1)
        }
    }

    private func push(
        _ builder: OpaquePointer, _ field: String, _ op: vdb_op_t, _ value: Value
    ) throws {
        var fieldBytes = Array(field.utf8)
        let rawOp = Int32(op.rawValue)
        try check { error in
            fieldBytes.withUnsafeMutableBufferPointer { f in
                switch value {
                case let .string(s):
                    var bytes = Array(s.utf8)
                    return bytes.withUnsafeMutableBufferPointer { v in
                        vdb_filter_compare_str(
                            builder, f.baseAddress, f.count, rawOp, v.baseAddress, v.count, error
                        )
                    }
                case let .int(i):
                    return vdb_filter_compare_i64(builder, f.baseAddress, f.count, rawOp, i, error)
                case let .double(d):
                    return vdb_filter_compare_f64(builder, f.baseAddress, f.count, rawOp, d, error)
                case let .bool(b):
                    return vdb_filter_compare_bool(builder, f.baseAddress, f.count, rawOp, b, error)
                }
            }
        }
    }

    private func pushUnary(
        _ builder: OpaquePointer, _ field: String, _ predicate: vdb_unary_t
    ) throws {
        var bytes = Array(field.utf8)
        let raw = Int32(predicate.rawValue)
        try check { error in
            bytes.withUnsafeMutableBufferPointer { f in
                vdb_filter_unary(builder, f.baseAddress, f.count, raw, error)
            }
        }
    }

    private func combine(
        _ builder: OpaquePointer, _ combinator: vdb_combine_t, _ count: Int
    ) throws {
        let raw = Int32(combinator.rawValue)
        try check { vdb_filter_combine(builder, raw, count, $0) }
    }
}
