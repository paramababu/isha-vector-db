// The filter and metadata compilers.
//
// These turn the query object a caller writes into the postfix sequence `vdb.h` takes. Getting
// it wrong is quiet: a filter that loses a clause returns documents the caller asked to exclude
// and says nothing, which is why the sequences are asserted here step by step rather than only
// through the answers they produce.
//
// The other half of that check lives in `cpp/test_bridge.cpp`, which replays sequences of this
// shape onto a real `vdb_filter_t` and compares the hits against the real engine. This file
// proves the right sequence is built; that one proves the sequence means what it should.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { compileFilter, compileMetadata, constants } from '../src/filter.js';
import { VdbError } from '../src/error.js';

const { STEP, OP, UNARY, COMBINE, MAX_DEPTH } = constants;

/** A readable summary of a step sequence, so a failing assertion says what went wrong. */
function summarise(steps) {
  return steps.map((s) => {
    switch (s.kind) {
      case STEP.compareString:
        return `${s.field} ${opName(s.op)} ${JSON.stringify(s.text)}`;
      case STEP.compareI64:
        return `${s.field} ${opName(s.op)} ${s.number}i`;
      case STEP.compareF64:
        return `${s.field} ${opName(s.op)} ${s.number}f`;
      case STEP.compareBool:
        return `${s.field} ${opName(s.op)} ${s.flag}`;
      case STEP.unary:
        return `${s.field} ${s.op === UNARY.exists ? 'EXISTS' : 'IS_NULL'}`;
      case STEP.combine:
        return `${combineName(s.op)}(${s.count})`;
      default:
        return `unknown(${s.kind})`;
    }
  });
}

function opName(op) {
  return Object.keys(OP).find((k) => OP[k] === op) ?? `op${op}`;
}

function combineName(op) {
  return Object.keys(COMBINE).find((k) => COMBINE[k] === op)?.toUpperCase() ?? `c${op}`;
}

/**
 * Every step sequence must be balanced: exactly one expression left on the stack.
 *
 * The bridge checks this too, against the real builder. Checking it here as well means a
 * compiler bug is caught by the test that produced the sequence rather than three layers later.
 */
function depthOf(steps) {
  let depth = 0;
  for (const s of steps) {
    if (s.kind === STEP.combine) {
      assert.ok(s.count <= depth, `${combineName(s.op)}(${s.count}) with only ${depth} on the stack`);
      depth -= s.count;
      depth += 1;
    } else {
      depth += 1;
    }
  }
  return depth;
}

function compiled(filter) {
  const steps = compileFilter(filter);
  if (steps.length > 0) {
    assert.equal(depthOf(steps), 1, `unbalanced: ${summarise(steps).join(' | ')}`);
  }
  return summarise(steps);
}

test('nothing to filter on compiles to nothing', () => {
  // The bridge reads an empty sequence as "search everything", which is what {} visibly ought
  // to mean — it is the identity of `and`.
  assert.deepEqual(compileFilter(undefined), []);
  assert.deepEqual(compileFilter(null), []);
  assert.deepEqual(compileFilter({}), []);
});

test('a bare value means equality', () => {
  assert.deepEqual(compiled({ category: 'tools' }), ['category eq "tools"']);
  assert.deepEqual(compiled({ stocked: true }), ['stocked eq true']);
});

test('an integral number is stored as an integer, a fractional one as a float', () => {
  // Metadata stores integers as integers. Comparing `{ count: 3 }` as a float would work today
  // and be a trap waiting for the day it does not.
  assert.deepEqual(compiled({ count: 3 }), ['count eq 3i']);
  assert.deepEqual(compiled({ price: 4.5 }), ['price eq 4.5f']);
  assert.deepEqual(compiled({ price: -0.5 }), ['price eq -0.5f']);
});

test('several keys mean conjunction', () => {
  assert.deepEqual(compiled({ category: 'tools', price: 25 }), [
    'category eq "tools"',
    'price eq 25i',
    'AND(2)',
  ]);
});

test('operators compile to their C ABI counterparts', () => {
  assert.deepEqual(compiled({ price: { $lt: 50 } }), ['price lt 50i']);
  assert.deepEqual(compiled({ price: { $gte: 50 } }), ['price gte 50i']);
  assert.deepEqual(compiled({ name: { $ne: 'saw' } }), ['name ne "saw"']);
  assert.deepEqual(compiled({ name: { $startsWith: 'ha' } }), ['name startsWith "ha"']);
  assert.deepEqual(compiled({ tags: { $contains: 'sharp' } }), ['tags contains "sharp"']);
});

test('several operators on one field mean conjunction', () => {
  assert.deepEqual(compiled({ price: { $gte: 10, $lt: 50 } }), [
    'price gte 10i',
    'price lt 50i',
    'AND(2)',
  ]);
});

test('$and, $or and $not nest', () => {
  assert.deepEqual(compiled({ $or: [{ category: 'toys' }, { price: { $gt: 50 } }] }), [
    'category eq "toys"',
    'price gt 50i',
    'OR(2)',
  ]);
  assert.deepEqual(compiled({ $not: { archived: true } }), ['archived eq true', 'NOT(1)']);
  assert.deepEqual(
    compiled({ $and: [{ a: 1, b: 2 }, { c: 3 }] }),
    ['a eq 1i', 'b eq 2i', 'AND(2)', 'c eq 3i', 'AND(2)'],
    'each branch is combined into one expression before the outer AND takes them',
  );
});

test('a single-element $and or $or is not wrapped in a needless combine', () => {
  assert.deepEqual(compiled({ $and: [{ a: 1 }] }), ['a eq 1i']);
  assert.deepEqual(compiled({ $or: [{ a: 1 }] }), ['a eq 1i']);
});

test('deep nesting stays balanced', () => {
  const filter = {
    $and: [
      { category: 'tools' },
      { $or: [{ price: { $lt: 20 } }, { $not: { stocked: false } }, { tags: { $contains: 'x' } }] },
      { $not: { $and: [{ a: 1 }, { b: 2 }] } },
    ],
  };
  const steps = compileFilter(filter);
  assert.equal(depthOf(steps), 1, summarise(steps).join(' | '));
  assert.deepEqual(summarise(steps), [
    'category eq "tools"',
    'price lt 20i',
    'stocked eq false',
    'NOT(1)',
    'tags contains "x"',
    'OR(3)',
    'a eq 1i',
    'b eq 2i',
    'AND(2)',
    'NOT(1)',
    'AND(3)',
  ]);
});

test('$in is an OR of equalities and $nin its negation', () => {
  // Neither is in the C ABI. Desugaring is exact because $ne is the exact negation of $eq,
  // including for documents that lack the field.
  assert.deepEqual(compiled({ category: { $in: ['tools', 'toys'] } }), [
    'category eq "tools"',
    'category eq "toys"',
    'OR(2)',
  ]);
  assert.deepEqual(compiled({ category: { $nin: ['tools', 'toys'] } }), [
    'category eq "tools"',
    'category eq "toys"',
    'OR(2)',
    'NOT(1)',
  ]);
  assert.deepEqual(compiled({ category: { $in: ['tools'] } }), ['category eq "tools"']);
  assert.deepEqual(compiled({ category: { $nin: ['tools'] } }), [
    'category eq "tools"',
    'NOT(1)',
  ]);
});

test('$exists maps to the unary predicates', () => {
  assert.deepEqual(compiled({ price: { $exists: true } }), ['price EXISTS']);
  // False means "absent, or present and null" — which is IS_NULL, not NOT EXISTS.
  assert.deepEqual(compiled({ price: { $exists: false } }), ['price IS_NULL']);
});

test('null compares as an existence test, because an absent field equals null', () => {
  assert.deepEqual(compiled({ note: null }), ['note IS_NULL']);
  assert.deepEqual(compiled({ note: { $eq: null } }), ['note IS_NULL']);
  assert.deepEqual(compiled({ note: { $ne: null } }), ['note IS_NULL', 'NOT(1)']);
});

test('an ordering against null is refused rather than silently matching nothing', () => {
  // It is never true, and the C ABI cannot express it. Compiling it to something that matches
  // nothing would look like a working query with no results.
  assert.throws(() => compileFilter({ price: { $gt: null } }), (e) => {
    assert.ok(e instanceof VdbError);
    assert.match(e.message, /null can only be compared with \$eq or \$ne/);
    return true;
  });
});

test('an array as a field value is refused rather than guessed at', () => {
  assert.throws(() => compileFilter({ tags: ['a', 'b'] }), (e) => {
    assert.match(e.message, /\$contains/);
    assert.match(e.message, /\$in/);
    return true;
  });
});

test('the mistakes that would otherwise reach the engine are caught here', () => {
  assert.throws(() => compileFilter({ $nope: 1 }), /unknown filter operator/);
  assert.throws(() => compileFilter({ price: { $nope: 1 } }), /unknown operator/);
  assert.throws(() => compileFilter({ $and: {} }), /array of filters/);
  assert.throws(() => compileFilter({ $not: [] }), /filter object/);
  assert.throws(() => compileFilter([]), /must be an object/);
  assert.throws(() => compileFilter({ name: { $startsWith: 7 } }), /takes a string/);
  assert.throws(() => compileFilter({ price: { $in: 'tools' } }), /takes an array/);
  assert.throws(() => compileFilter({ price: { $in: [] } }), /needs a value/);
  assert.throws(() => compileFilter({ price: Number.NaN }), /not a comparable number/);
  assert.throws(() => compileFilter({ price: () => 1 }), /must be strings, numbers/);
});

test('an empty clause is refused rather than quietly matching everything', () => {
  // `{ price: {} }` constrains nothing, and an operator object that constrains nothing is far
  // more likely to be a typo than an intention.
  assert.throws(() => compileFilter({ price: {} }), /matches everything/);
  assert.throws(() => compileFilter({ $and: [] }), /at least one filter/);
  assert.throws(() => compileFilter({ $or: [{}] }), /cannot contain an empty filter/);
  assert.throws(() => compileFilter({ $not: {} }), /cannot negate an empty filter/);
});

test('undefined is skipped, so an optional query parameter does not have to be deleted', () => {
  assert.deepEqual(compiled({ category: 'tools', price: undefined }), ['category eq "tools"']);
  assert.deepEqual(compileFilter({ price: undefined }), []);
  assert.deepEqual(compiled({ price: { $lt: 50, $gt: undefined } }), ['price lt 50i']);
});

test('nesting deeper than the engine allows is refused before it overflows the stack', () => {
  let filter = { a: 1 };
  for (let i = 0; i < MAX_DEPTH + 2; i++) filter = { $not: filter };
  assert.throws(() => compileFilter(filter), new RegExp(`deeper than ${MAX_DEPTH}`));
});

// ---- metadata ----

test('metadata compiles to tagged fields', () => {
  assert.deepEqual(compileMetadata({ s: 'x', i: 3, f: 1.5, b: true, n: null }), [
    { kind: 1, key: 's', text: 'x', number: 0, flag: false },
    { kind: 2, key: 'i', text: '', number: 3, flag: false },
    { kind: 3, key: 'f', text: '', number: 1.5, flag: false },
    { kind: 4, key: 'b', text: '', number: 0, flag: true },
    { kind: 5, key: 'n', text: '', number: 0, flag: false },
  ]);
  assert.deepEqual(compileMetadata(undefined), []);
  assert.deepEqual(compileMetadata({}), []);
});

test('an explicit null is written, not dropped', () => {
  // No comparison can tell an explicit null from an absent field, but { $exists: true } can, so
  // dropping the key would quietly change what that reports. vdb_metadata_set_null exists for
  // exactly this.
  const [field] = compileMetadata({ note: null });
  assert.equal(field.kind, 5);
  assert.equal(field.key, 'note');
});

test('undefined is skipped, so an optional field does not have to be deleted', () => {
  assert.deepEqual(compileMetadata({ a: 1, b: undefined }).map((f) => f.key), ['a']);
});

test('what metadata cannot hold yet is refused with the reason', () => {
  assert.throws(() => compileMetadata({ tags: ['a'] }), /Nested objects and arrays/);
  assert.throws(() => compileMetadata({ nested: { a: 1 } }), /Nested objects and arrays/);
  assert.throws(() => compileMetadata({ n: Number.POSITIVE_INFINITY }), /cannot be stored/);
  assert.throws(() => compileMetadata('not an object'), /must be an object/);
});
