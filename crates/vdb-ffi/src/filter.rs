//! Building a filter across the boundary.
//!
//! # Why a postfix builder
//!
//! A filter is a tree, and C has no good way to receive one. The obvious options are both bad:
//! a function per operator per value type is thirty-odd functions and still cannot nest, and an
//! encoded payload means every binding reimplements our encoding and one of them gets it wrong.
//!
//! So the builder is a stack. Leaves are pushed; a combinator pops several and pushes one.
//!
//! ```c
//! vdb_filter_t *f = vdb_filter_new();
//! vdb_filter_compare_str(f, "category", 8, VDB_OP_EQ, "tools", 5, NULL);
//! vdb_filter_compare_f64(f, "price", 5, VDB_OP_LT, 50.0, NULL);
//! vdb_filter_combine(f, VDB_COMBINE_AND, 2, NULL);   /* category == "tools" && price < 50 */
//! ```
//!
//! Eight functions express every filter the engine has, at any depth. The cost is that a
//! malformed sequence — combining more nodes than were pushed — is a runtime error rather than
//! a compile-time one, so the stack depth is checked on every operation and again at the end.

use vdb_core::filter::Filter;
use vdb_core::metadata::Value;

use crate::error::{guard, VdbError};
use crate::strings::borrow_str;
use crate::Boundary;

/// Comparison operators, matching `vdb_op_t` in the header.
const OP_EQ: i32 = 1;
const OP_NE: i32 = 2;
const OP_GT: i32 = 3;
const OP_GTE: i32 = 4;
const OP_LT: i32 = 5;
const OP_LTE: i32 = 6;
const OP_STARTS_WITH: i32 = 7;
const OP_CONTAINS: i32 = 8;

/// Unary predicates, matching `vdb_unary_t`.
const UNARY_EXISTS: i32 = 1;
const UNARY_IS_NULL: i32 = 2;

/// Combinators, matching `vdb_combine_t`.
const COMBINE_AND: i32 = 1;
const COMBINE_OR: i32 = 2;
const COMBINE_NOT: i32 = 3;

/// A filter under construction: a stack of partially-built expressions.
#[derive(Debug, Default)]
pub struct VdbFilter {
    stack: Vec<Filter>,
}

impl VdbFilter {
    pub(crate) fn into_raw(self) -> *mut Self {
        Box::into_raw(Box::new(self))
    }

    /// # Safety
    /// The pointer must come from `into_raw` and must not be used again.
    pub(crate) unsafe fn destroy(ptr: *mut Self) {
        // SAFETY: the caller guarantees the pointer came from `into_raw`.
        drop(unsafe { Box::from_raw(ptr) });
    }

    /// # Safety
    /// A non-null pointer must be live and must have come from `into_raw`.
    pub(crate) unsafe fn borrow_mut<'a>(ptr: *mut Self) -> Result<&'a mut Self, Boundary> {
        if ptr.is_null() {
            return Err(Boundary::Null);
        }
        // SAFETY: checked non-null; the caller guarantees exclusivity.
        Ok(unsafe { &mut *ptr })
    }

    /// The finished filter, or `None` if the sequence did not build exactly one.
    pub(crate) fn finish(&self) -> Option<&Filter> {
        match self.stack.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    fn push_compare(&mut self, field: &str, op: i32, value: Value) -> Result<(), Boundary> {
        let filter = match op {
            OP_EQ => Filter::eq(field, value),
            OP_NE => Filter::ne(field, value),
            OP_GT => Filter::gt(field, value),
            OP_GTE => Filter::gte(field, value),
            OP_LT => Filter::lt(field, value),
            OP_LTE => Filter::lte(field, value),
            OP_CONTAINS => Filter::contains(field, value),
            OP_STARTS_WITH => match value {
                Value::Str(prefix) => Filter::starts_with(field, prefix),
                // A prefix test on a number is not a coercion question, it is a mistake, and
                // saying so is better than matching nothing forever.
                _ => return Err(Boundary::InvalidArgument),
            },
            _ => return Err(Boundary::InvalidArgument),
        };
        self.stack.push(filter);
        Ok(())
    }
}

/// Create a filter builder. Release with [`vdb_filter_free`].
#[no_mangle]
pub extern "C" fn vdb_filter_new() -> *mut VdbFilter {
    VdbFilter::default().into_raw()
}

/// Release a filter builder. Null is a no-op.
///
/// # Safety
/// `filter` must come from [`vdb_filter_new`] and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn vdb_filter_free(filter: *mut VdbFilter) {
    if filter.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the handle came from this library and is not reused.
    unsafe { VdbFilter::destroy(filter) };
}

/// Push a comparison against a string value.
///
/// # Safety
/// `filter` must be live; `field` and `value` must point to readable UTF-8.
#[no_mangle]
pub unsafe extern "C" fn vdb_filter_compare_str(
    filter: *mut VdbFilter,
    field: *const u8,
    field_len: usize,
    op: i32,
    value: *const u8,
    value_len: usize,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and both strings are readable.
        let f = unsafe { VdbFilter::borrow_mut(filter) }?;
        let field = unsafe { borrow_str(field, field_len) }?.to_owned();
        let value = unsafe { borrow_str(value, value_len) }?.to_owned();
        f.push_compare(&field, op, Value::Str(value))
    })
}

/// Push a comparison against an integer value.
///
/// # Safety
/// `filter` must be live and `field` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_filter_compare_i64(
    filter: *mut VdbFilter,
    field: *const u8,
    field_len: usize,
    op: i32,
    value: i64,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and `field` is readable.
        let f = unsafe { VdbFilter::borrow_mut(filter) }?;
        let field = unsafe { borrow_str(field, field_len) }?.to_owned();
        f.push_compare(&field, op, Value::I64(value))
    })
}

/// Push a comparison against a floating-point value.
///
/// # Safety
/// `filter` must be live and `field` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_filter_compare_f64(
    filter: *mut VdbFilter,
    field: *const u8,
    field_len: usize,
    op: i32,
    value: f64,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and `field` is readable.
        let f = unsafe { VdbFilter::borrow_mut(filter) }?;
        let field = unsafe { borrow_str(field, field_len) }?.to_owned();
        f.push_compare(&field, op, Value::F64(value))
    })
}

/// Push a comparison against a boolean value.
///
/// # Safety
/// `filter` must be live and `field` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_filter_compare_bool(
    filter: *mut VdbFilter,
    field: *const u8,
    field_len: usize,
    op: i32,
    value: bool,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and `field` is readable.
        let f = unsafe { VdbFilter::borrow_mut(filter) }?;
        let field = unsafe { borrow_str(field, field_len) }?.to_owned();
        f.push_compare(&field, op, Value::Bool(value))
    })
}

/// Push a unary predicate: `VDB_UNARY_EXISTS` or `VDB_UNARY_IS_NULL`.
///
/// # Safety
/// `filter` must be live and `field` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_filter_unary(
    filter: *mut VdbFilter,
    field: *const u8,
    field_len: usize,
    predicate: i32,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and `field` is readable.
        let f = unsafe { VdbFilter::borrow_mut(filter) }?;
        let field = unsafe { borrow_str(field, field_len) }?.to_owned();
        let built = match predicate {
            UNARY_EXISTS => Filter::exists(field),
            UNARY_IS_NULL => Filter::is_null(field),
            _ => return Err(Boundary::InvalidArgument),
        };
        f.stack.push(built);
        Ok(())
    })
}

/// Pop `count` expressions and push their combination.
///
/// `VDB_COMBINE_NOT` takes exactly one; `AND` and `OR` take any number. Combining more than were
/// pushed is refused rather than silently producing a smaller filter — a filter that quietly
/// drops a clause returns documents the caller asked to exclude.
///
/// # Safety
/// `filter` must be live.
#[no_mangle]
pub unsafe extern "C" fn vdb_filter_combine(
    filter: *mut VdbFilter,
    combinator: i32,
    count: usize,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live.
        let f = unsafe { VdbFilter::borrow_mut(filter) }?;
        if count == 0 || count > f.stack.len() {
            return Err(Boundary::InvalidArgument);
        }
        if combinator == COMBINE_NOT && count != 1 {
            return Err(Boundary::InvalidArgument);
        }
        let operands: Vec<Filter> = f.stack.split_off(f.stack.len() - count);
        let combined = match combinator {
            COMBINE_AND => Filter::all(operands),
            COMBINE_OR => Filter::any(operands),
            COMBINE_NOT => match operands.into_iter().next() {
                Some(only) => Filter::negate(only),
                None => return Err(Boundary::InvalidArgument),
            },
            _ => return Err(Boundary::InvalidArgument),
        };
        f.stack.push(combined);
        Ok(())
    })
}

/// How many expressions are on the builder's stack.
///
/// A finished filter has exactly one. Anything else means the sequence of pushes and combines
/// did not balance, and [`crate::vdb_search_filtered`] will refuse it.
///
/// # Safety
/// `filter` must be live, or null — in which case this returns 0.
#[no_mangle]
pub unsafe extern "C" fn vdb_filter_depth(filter: *const VdbFilter) -> usize {
    if filter.is_null() {
        return 0;
    }
    // SAFETY: the caller guarantees a non-null handle is live.
    unsafe { (*filter).stack.len() }
}
