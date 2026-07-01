//! Value-level semantics for the arithmetic, comparison, and logical operations.
//!
//! These functions are the meaning of the instructions, factored out of the
//! dispatch loop so they can be reasoned about and tested on their own — a pure
//! `add(a, b)` is far easier to exercise across every operand pairing than the
//! same logic buried inside the interpreter. The VM's hot loop is then a thin
//! shell that fetches operands, calls one of these, and stores the result.
//!
//! # Numeric coercion
//!
//! Arithmetic and ordering treat integers and floats as one numeric tower. Two
//! integers stay integers, and integer results are overflow-checked so a fault is
//! reported rather than silently wrapped. If either operand is a float the result
//! is a float, following ordinary widening. Any non-numeric operand is a
//! [`VmError::TypeMismatch`].

use crate::VmError;
use value_lang::{Unpacked, Value};

/// A numeric operand, already narrowed to the two kinds arithmetic accepts.
#[derive(Clone, Copy)]
enum Num {
    Int(i32),
    Float(f64),
}

/// Extract a numeric view of `v`, or `None` if it is not a number.
#[inline]
fn num(v: Value) -> Option<Num> {
    if let Some(i) = v.as_int() {
        Some(Num::Int(i))
    } else {
        v.as_float().map(Num::Float)
    }
}

/// Widen a numeric operand to `f64` for mixed-kind arithmetic.
#[inline]
fn to_f64(n: Num) -> f64 {
    match n {
        Num::Int(i) => f64::from(i),
        Num::Float(f) => f,
    }
}

/// Pull both operands as numbers, or raise a type mismatch naming `op`.
#[inline]
fn nums(a: Value, b: Value, op: &'static str) -> Result<(Num, Num), VmError> {
    match (num(a), num(b)) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(VmError::TypeMismatch { op }),
    }
}

/// `a + b`. Integer addition is overflow-checked; a float operand promotes.
#[inline]
pub(crate) fn add(a: Value, b: Value) -> Result<Value, VmError> {
    match nums(a, b, "add")? {
        (Num::Int(x), Num::Int(y)) => x
            .checked_add(y)
            .map(Value::int)
            .ok_or(VmError::IntegerOverflow),
        (x, y) => Ok(Value::float(to_f64(x) + to_f64(y))),
    }
}

/// `a - b`. Integer subtraction is overflow-checked; a float operand promotes.
#[inline]
pub(crate) fn sub(a: Value, b: Value) -> Result<Value, VmError> {
    match nums(a, b, "sub")? {
        (Num::Int(x), Num::Int(y)) => x
            .checked_sub(y)
            .map(Value::int)
            .ok_or(VmError::IntegerOverflow),
        (x, y) => Ok(Value::float(to_f64(x) - to_f64(y))),
    }
}

/// `a * b`. Integer multiplication is overflow-checked; a float operand promotes.
#[inline]
pub(crate) fn mul(a: Value, b: Value) -> Result<Value, VmError> {
    match nums(a, b, "mul")? {
        (Num::Int(x), Num::Int(y)) => x
            .checked_mul(y)
            .map(Value::int)
            .ok_or(VmError::IntegerOverflow),
        (x, y) => Ok(Value::float(to_f64(x) * to_f64(y))),
    }
}

/// `a / b`. Integer division by zero errors; `i32::MIN / -1` overflows. A float
/// operand promotes and then follows IEEE-754 (division by zero yields ±∞ / NaN).
#[inline]
pub(crate) fn div(a: Value, b: Value) -> Result<Value, VmError> {
    match nums(a, b, "div")? {
        (Num::Int(x), Num::Int(y)) => {
            if y == 0 {
                Err(VmError::DivideByZero)
            } else {
                x.checked_div(y)
                    .map(Value::int)
                    .ok_or(VmError::IntegerOverflow)
            }
        }
        (x, y) => Ok(Value::float(to_f64(x) / to_f64(y))),
    }
}

/// `a % b`. Integer remainder by zero errors; `i32::MIN % -1` overflows. A float
/// operand promotes and follows IEEE-754 remainder semantics.
#[inline]
pub(crate) fn rem(a: Value, b: Value) -> Result<Value, VmError> {
    match nums(a, b, "rem")? {
        (Num::Int(x), Num::Int(y)) => {
            if y == 0 {
                Err(VmError::DivideByZero)
            } else {
                x.checked_rem(y)
                    .map(Value::int)
                    .ok_or(VmError::IntegerOverflow)
            }
        }
        (x, y) => Ok(Value::float(to_f64(x) % to_f64(y))),
    }
}

/// `-a`. Integer negation is overflow-checked (`i32::MIN` faults).
#[inline]
pub(crate) fn neg(a: Value) -> Result<Value, VmError> {
    match num(a) {
        Some(Num::Int(i)) => i
            .checked_neg()
            .map(Value::int)
            .ok_or(VmError::IntegerOverflow),
        Some(Num::Float(f)) => Ok(Value::float(-f)),
        None => Err(VmError::TypeMismatch { op: "neg" }),
    }
}

/// Structural / numeric equality.
///
/// Two numbers compare by value across int and float (`1` equals `1.0`); other
/// kinds compare within their kind (`nil` to `nil`, boolean to boolean, symbol to
/// symbol) and are never equal across kinds. Float comparison is IEEE-754, so
/// `NaN` is equal to nothing, including itself.
#[inline]
pub(crate) fn eq(a: Value, b: Value) -> bool {
    match (num(a), num(b)) {
        (Some(Num::Int(x)), Some(Num::Int(y))) => x == y,
        (Some(x), Some(y)) => to_f64(x) == to_f64(y),
        // At least one operand is non-numeric: compare within-kind.
        _ => match (a.unpack(), b.unpack()) {
            (Unpacked::Nil, Unpacked::Nil) => true,
            (Unpacked::Bool(x), Unpacked::Bool(y)) => x == y,
            (Unpacked::Sym(x), Unpacked::Sym(y)) => x == y,
            _ => false,
        },
    }
}

/// [`eq`] as an instruction result: the boolean wrapped back into a [`Value`].
#[inline]
pub(crate) fn eq_op(a: Value, b: Value) -> Result<Value, VmError> {
    Ok(Value::bool(eq(a, b)))
}

/// The negation of [`eq`] as an instruction result.
#[inline]
pub(crate) fn ne_op(a: Value, b: Value) -> Result<Value, VmError> {
    Ok(Value::bool(!eq(a, b)))
}

/// Numeric ordering shared by `<`, `<=`, `>`, `>=`.
///
/// Returns `None` when either operand is non-numeric (a type mismatch) or when a
/// float comparison is unordered (`NaN`), letting the caller decide the result
/// for its specific predicate.
#[inline]
fn partial_cmp(a: Value, b: Value) -> Option<core::cmp::Ordering> {
    match (num(a)?, num(b)?) {
        (Num::Int(x), Num::Int(y)) => Some(x.cmp(&y)),
        (x, y) => to_f64(x).partial_cmp(&to_f64(y)),
    }
}

/// `a < b` over numbers. `NaN` and non-numeric operands are handled by `op`.
#[inline]
pub(crate) fn lt(a: Value, b: Value) -> Result<Value, VmError> {
    order(a, b, "lt", |o| o == core::cmp::Ordering::Less)
}

/// `a <= b` over numbers.
#[inline]
pub(crate) fn le(a: Value, b: Value) -> Result<Value, VmError> {
    order(a, b, "le", |o| o != core::cmp::Ordering::Greater)
}

/// `a > b` over numbers.
#[inline]
pub(crate) fn gt(a: Value, b: Value) -> Result<Value, VmError> {
    order(a, b, "gt", |o| o == core::cmp::Ordering::Greater)
}

/// `a >= b` over numbers.
#[inline]
pub(crate) fn ge(a: Value, b: Value) -> Result<Value, VmError> {
    order(a, b, "ge", |o| o != core::cmp::Ordering::Less)
}

/// Shared ordering core. A non-numeric operand is a type mismatch; an unordered
/// comparison (`NaN`) yields `false` for every predicate, matching Rust's own
/// float ordering.
#[inline]
fn order(
    a: Value,
    b: Value,
    op: &'static str,
    pred: impl Fn(core::cmp::Ordering) -> bool,
) -> Result<Value, VmError> {
    if num(a).is_none() || num(b).is_none() {
        return Err(VmError::TypeMismatch { op });
    }
    Ok(Value::bool(partial_cmp(a, b).is_some_and(pred)))
}

/// Logical negation of a boolean. Any non-boolean operand is a type mismatch.
#[inline]
pub(crate) fn not(a: Value) -> Result<Value, VmError> {
    a.as_bool()
        .map(|b| Value::bool(!b))
        .ok_or(VmError::TypeMismatch { op: "not" })
}

/// Read a boolean condition for a branch, naming `op` on a type mismatch.
#[inline]
pub(crate) fn cond(a: Value, op: &'static str) -> Result<bool, VmError> {
    a.as_bool().ok_or(VmError::TypeMismatch { op })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_add_int_int_stays_int() {
        assert_eq!(add(Value::int(2), Value::int(3)).unwrap().as_int(), Some(5));
    }

    #[test]
    fn test_add_mixed_promotes_to_float() {
        let r = add(Value::int(2), Value::float(0.5)).unwrap();
        assert_eq!(r.as_float(), Some(2.5));
    }

    #[test]
    fn test_add_overflow_errors() {
        assert_eq!(
            add(Value::int(i32::MAX), Value::int(1)),
            Err(VmError::IntegerOverflow)
        );
    }

    #[test]
    fn test_add_non_numeric_type_mismatch() {
        assert_eq!(
            add(Value::int(1), Value::bool(true)),
            Err(VmError::TypeMismatch { op: "add" })
        );
    }

    #[test]
    fn test_div_int_by_zero_errors() {
        assert_eq!(
            div(Value::int(1), Value::int(0)),
            Err(VmError::DivideByZero)
        );
    }

    #[test]
    fn test_div_min_by_neg_one_overflows() {
        assert_eq!(
            div(Value::int(i32::MIN), Value::int(-1)),
            Err(VmError::IntegerOverflow)
        );
    }

    #[test]
    fn test_div_float_by_zero_is_infinite() {
        let r = div(Value::float(1.0), Value::float(0.0)).unwrap();
        assert_eq!(r.as_float(), Some(f64::INFINITY));
    }

    #[test]
    fn test_rem_by_zero_errors() {
        assert_eq!(
            rem(Value::int(5), Value::int(0)),
            Err(VmError::DivideByZero)
        );
    }

    #[test]
    fn test_neg_min_overflows() {
        assert_eq!(neg(Value::int(i32::MIN)), Err(VmError::IntegerOverflow));
    }

    #[test]
    fn test_eq_int_equals_float() {
        assert!(eq(Value::int(1), Value::float(1.0)));
    }

    #[test]
    fn test_eq_nan_is_never_equal() {
        assert!(!eq(Value::float(f64::NAN), Value::float(f64::NAN)));
    }

    #[test]
    fn test_eq_across_kinds_is_false() {
        assert!(!eq(Value::nil(), Value::bool(false)));
        assert!(eq(Value::nil(), Value::nil()));
        assert!(eq(Value::bool(true), Value::bool(true)));
    }

    #[test]
    fn test_lt_orders_numbers() {
        assert_eq!(
            lt(Value::int(1), Value::int(2)).unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(
            le(Value::int(2), Value::int(2)).unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(
            gt(Value::float(3.0), Value::int(2)).unwrap().as_bool(),
            Some(true)
        );
    }

    #[test]
    fn test_lt_nan_is_false_not_error() {
        assert_eq!(
            lt(Value::float(f64::NAN), Value::float(1.0))
                .unwrap()
                .as_bool(),
            Some(false)
        );
    }

    #[test]
    fn test_ordering_non_numeric_errors() {
        assert_eq!(
            lt(Value::nil(), Value::int(1)),
            Err(VmError::TypeMismatch { op: "lt" })
        );
    }

    #[test]
    fn test_not_requires_bool() {
        assert_eq!(not(Value::bool(true)).unwrap().as_bool(), Some(false));
        assert_eq!(not(Value::int(0)), Err(VmError::TypeMismatch { op: "not" }));
    }
}
