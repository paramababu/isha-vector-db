// Turning a filter query object into the postfix sequence the C ABI takes.
//
// A filter is a tree, and C has no good way to receive one, so `vdb.h` takes a stack: push
// leaves, then combine the top N. Node binds the engine directly and can hand it a tree, but
// everything reaching the engine through the C ABI — this SDK, Swift, Java — has to flatten one.
//
// This is done in JavaScript rather than in C++ for the same reason the bridge holds the rest of
// the logic: it is the half that can be tested without a device. `test/filter.test.js` runs the
// whole of it in Node, and `cpp/vdb_bridge.cpp` replays whatever it produces onto a real
// `vdb_filter_t` and checks the answers against the real engine.
//
// The query-object convention is borrowed from `@isais-logic/isha-vector-db-node` deliberately, and the
// semantics are the engine's own — `docs/api/filters.md` is the reference. Divergence between
// SDKs is the fastest way to make a cross-platform library untrustworthy, so where this file has
// a choice it makes the same one `crates/isha-vector-db-node/src/filter.rs` makes.

import { VdbError } from './error.js';

/** Step kinds, matching `MetaField`/`FilterOp` in `cpp/vdb_bridge.h`. */
const STEP = {
  compareString: 1,
  compareI64: 2,
  compareF64: 3,
  compareBool: 4,
  unary: 5,
  combine: 6,
};

/** `vdb_op_t`. */
const OP = {
  eq: 1,
  ne: 2,
  gt: 3,
  gte: 4,
  lt: 5,
  lte: 6,
  startsWith: 7,
  contains: 8,
};

/** `vdb_unary_t`. */
const UNARY = { exists: 1, isNull: 2 };

/** `vdb_combine_t`. */
const COMBINE = { and: 1, or: 2, not: 3 };

/**
 * Deepest nesting accepted, matching the engine's own limit and Node's.
 *
 * Checked on the way down rather than after building: this walk recurses, and a deeply nested
 * object from an untrusted source would otherwise overflow the stack, which kills the app rather
 * than producing an error anyone can handle.
 */
const MAX_DEPTH = 32;

/** One step, with every property present so the JSI layer reads a fixed shape and never branches. */
function step(kind, { field = '', op = 0, text = '', number = 0, flag = false, count = 0 } = {}) {
  return { kind, field, op, text, number, flag, count };
}

function fail(message) {
  // `INVALID_ARGUMENT`. The engine would say the same thing about the same mistake; catching it
  // here only means it is said before a native call rather than after one.
  return new VdbError(1002, message);
}

/**
 * Compile a filter object into a postfix step list.
 *
 * `undefined` and `{}` both compile to an empty list, which the bridge reads as "no filter" —
 * `{}` is the identity of `and`, and searching everything is what it visibly ought to mean.
 */
export function compileFilter(filter) {
  if (filter === undefined || filter === null) return [];
  if (typeof filter !== 'object' || Array.isArray(filter)) {
    throw fail('a filter must be an object');
  }
  const steps = [];
  const clauses = walkObject(filter, steps, 1);
  // A filter of no clauses is not pushed at all, so an empty object stays an empty list rather
  // than becoming an `and` of nothing that the stack would then have to hold.
  if (clauses === 0) return [];
  conjoin(steps, clauses);
  return steps;
}

/** Combine `count` expressions already on the stack, unless there is only the one. */
function conjoin(steps, count) {
  if (count > 1) steps.push(step(STEP.combine, { op: COMBINE.and, count }));
}

/**
 * Walk one filter object, pushing its clauses. Returns how many it pushed.
 *
 * Several keys in one object mean conjunction and a bare value means equality — which is what
 * the shape looks like it means, and a filter language whose obvious reading is wrong is worse
 * than one with no shorthand at all.
 */
function walkObject(object, steps, depth) {
  if (depth > MAX_DEPTH) throw fail(`filter nested deeper than ${MAX_DEPTH}`);
  let clauses = 0;

  for (const [key, value] of Object.entries(object)) {
    if (value === undefined) continue;
    switch (key) {
      case '$and':
      case '$or': {
        const list = requireFilterList(key, value);
        // An empty `$and` matches everything and an empty `$or` matches nothing. Neither can be
        // expressed as a combine of zero, so they are refused rather than silently becoming the
        // other one.
        if (list.length === 0) throw fail(`${key} needs at least one filter`);
        for (const each of list) {
          const pushed = walkObject(requireObject(key, each), steps, depth + 1);
          if (pushed === 0) throw fail(`${key} cannot contain an empty filter`);
          conjoin(steps, pushed);
        }
        if (list.length > 1) {
          steps.push(step(STEP.combine, {
            op: key === '$and' ? COMBINE.and : COMBINE.or,
            count: list.length,
          }));
        }
        clauses++;
        break;
      }
      case '$not': {
        const pushed = walkObject(requireObject('$not', value), steps, depth + 1);
        if (pushed === 0) throw fail('$not cannot negate an empty filter');
        conjoin(steps, pushed);
        steps.push(step(STEP.combine, { op: COMBINE.not, count: 1 }));
        clauses++;
        break;
      }
      default: {
        if (key.startsWith('$')) {
          throw fail(
            `unknown filter operator ${JSON.stringify(key)}; expected $and, $or or $not at the ` +
              'top level',
          );
        }
        clauses += walkField(key, value, steps, depth);
      }
    }
  }
  return clauses;
}

/** One field's clause: a bare value means equality, an object means operators. */
function walkField(field, value, steps, depth) {
  if (Array.isArray(value)) {
    // Equality with the array, or membership in it? Both readings are plausible, so neither is
    // guessed at.
    throw fail(
      `field ${JSON.stringify(field)}: an array is not a filter value; use { $contains: … } for ` +
        'array membership or { $in: [ … ] } for a set of alternatives',
    );
  }
  if (value !== null && typeof value === 'object') {
    return walkOperators(field, value, steps, depth);
  }
  pushCompare(steps, field, OP.eq, value);
  return 1;
}

function walkOperators(field, operators, steps, depth) {
  if (depth > MAX_DEPTH) throw fail(`filter nested deeper than ${MAX_DEPTH}`);
  let clauses = 0;

  for (const [op, raw] of Object.entries(operators)) {
    if (raw === undefined) continue;
    switch (op) {
      case '$eq':
        pushCompare(steps, field, OP.eq, raw);
        break;
      case '$ne':
        pushCompare(steps, field, OP.ne, raw);
        break;
      case '$gt':
        pushCompare(steps, field, OP.gt, raw);
        break;
      case '$gte':
        pushCompare(steps, field, OP.gte, raw);
        break;
      case '$lt':
        pushCompare(steps, field, OP.lt, raw);
        break;
      case '$lte':
        pushCompare(steps, field, OP.lte, raw);
        break;
      case '$contains':
        pushCompare(steps, field, OP.contains, raw);
        break;
      case '$startsWith':
        if (typeof raw !== 'string') {
          throw fail(`field ${JSON.stringify(field)}: $startsWith takes a string`);
        }
        steps.push(step(STEP.compareString, { field, op: OP.startsWith, text: raw }));
        break;
      case '$in':
      case '$nin': {
        // Not in the C ABI: `$in` is an OR of equalities, and `$nin` its negation. Desugared
        // here rather than added to a frozen ABI, and the equivalence is exact because `$ne` is
        // the exact negation of `$eq` — including for documents lacking the field.
        const values = requireArray(field, op, raw);
        if (values.length === 0) throw fail(`field ${JSON.stringify(field)}: ${op} needs a value`);
        for (const value of values) pushCompare(steps, field, OP.eq, value);
        if (values.length > 1) {
          steps.push(step(STEP.combine, { op: COMBINE.or, count: values.length }));
        }
        if (op === '$nin') steps.push(step(STEP.combine, { op: COMBINE.not, count: 1 }));
        break;
      }
      case '$exists':
        steps.push(
          step(STEP.unary, { field, op: raw ? UNARY.exists : UNARY.isNull }),
        );
        break;
      default:
        throw fail(`field ${JSON.stringify(field)}: unknown operator ${JSON.stringify(op)}`);
    }
    clauses++;
  }

  // `{ field: {} }` constrains nothing, and an operator object that constrains nothing is far
  // more likely to be a typo than an intention.
  if (clauses === 0) {
    throw fail(`field ${JSON.stringify(field)}: an empty operator object matches everything`);
  }
  conjoin(steps, clauses);
  return 1;
}

/**
 * Push a comparison, choosing the step kind from the value's JavaScript type.
 *
 * `null` becomes an existence test rather than a comparison. The C ABI has no null comparison
 * because it needs none: an absent field already equals null, so `{ x: null }` is exactly
 * "x is absent or explicitly null", which is what `IS_NULL` tests.
 */
function pushCompare(steps, field, op, value) {
  if (value === null) {
    if (op === OP.eq) {
      steps.push(step(STEP.unary, { field, op: UNARY.isNull }));
      return;
    }
    if (op === OP.ne) {
      steps.push(step(STEP.unary, { field, op: UNARY.isNull }));
      steps.push(step(STEP.combine, { op: COMBINE.not, count: 1 }));
      return;
    }
    // An ordering against null is false for every document, and the engine says so — but there
    // is no way to spell it through the C ABI, and silently matching nothing would look like a
    // working query returning no results.
    throw fail(
      `field ${JSON.stringify(field)}: null can only be compared with $eq or $ne; an ordering ` +
        'against null is never true',
    );
  }

  switch (typeof value) {
    case 'string':
      steps.push(step(STEP.compareString, { field, op, text: value }));
      return;
    case 'boolean':
      steps.push(step(STEP.compareBool, { field, op, flag: value }));
      return;
    case 'number': {
      if (!Number.isFinite(value)) {
        throw fail(`field ${JSON.stringify(field)}: ${value} is not a comparable number`);
      }
      // Integral numbers become integers, matching how metadata is stored — otherwise
      // `{ count: 3 }` would compare a float against a stored integer and, although the engine
      // handles that correctly, the asymmetry would be a trap waiting for the day it does not.
      const integral = Number.isInteger(value) && Math.abs(value) < 9e15;
      steps.push(
        step(integral ? STEP.compareI64 : STEP.compareF64, { field, op, number: value }),
      );
      return;
    }
    default:
      throw fail(
        `field ${JSON.stringify(field)}: filter values must be strings, numbers, booleans or ` +
          `null; got ${typeof value}`,
      );
  }
}

function requireFilterList(key, value) {
  if (!Array.isArray(value)) throw fail(`${key} takes an array of filters`);
  return value;
}

function requireObject(key, value) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw fail(`${key} takes a filter object`);
  }
  return value;
}

function requireArray(field, op, value) {
  if (!Array.isArray(value)) throw fail(`field ${JSON.stringify(field)}: ${op} takes an array`);
  return value;
}

/**
 * Compile a metadata object into the flat field list the bridge writes.
 *
 * Same fixed shape as a filter step, for the same reason: the JSI layer reads the same property
 * names from every element and never has to decide what one means.
 *
 * `null` is written explicitly rather than dropped. No comparison can tell an explicit null from
 * an absent field, but `{ $exists: true }` can, and a binding that dropped the key would quietly
 * change what that reports.
 */
export function compileMetadata(metadata) {
  if (metadata === undefined || metadata === null) return [];
  if (typeof metadata !== 'object' || Array.isArray(metadata)) {
    throw fail('metadata must be an object');
  }

  const fields = [];
  for (const [key, value] of Object.entries(metadata)) {
    if (value === undefined) continue;
    if (value === null) {
      fields.push({ kind: 5, key, text: '', number: 0, flag: false });
      continue;
    }
    switch (typeof value) {
      case 'string':
        fields.push({ kind: 1, key, text: value, number: 0, flag: false });
        break;
      case 'boolean':
        fields.push({ kind: 4, key, text: '', number: 0, flag: value });
        break;
      case 'number': {
        if (!Number.isFinite(value)) {
          throw fail(`metadata field ${JSON.stringify(key)}: ${value} cannot be stored`);
        }
        const integral = Number.isInteger(value) && Math.abs(value) < 9e15;
        fields.push({ kind: integral ? 2 : 3, key, text: '', number: value, flag: false });
        break;
      }
      default:
        throw fail(
          `metadata field ${JSON.stringify(key)}: values must be strings, numbers, booleans or ` +
            `null; got ${Array.isArray(value) ? 'an array' : typeof value}. Nested objects and ` +
            'arrays are not supported yet.',
        );
    }
  }
  return fields;
}

export const constants = { STEP, OP, UNARY, COMBINE, MAX_DEPTH };
