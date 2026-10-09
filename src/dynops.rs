//! The numeric fast path of the dynamic instructions (LSB §5.6) and the
//! built-in rules of `deq`/`dlt`/`dle`, truthiness, and concatenation.
//!
//! Every function here returns `None` where LSB hands the operation to a
//! hook; the interpreter then calls the hook or raises the instruction's
//! fallback error.

use core::cmp::Ordering;

use bytecode_lang::{DivZero, ErrorKind, Overflow, Policy};

use crate::conv::{self, Num};
use crate::dynv::{self, Raw};
use crate::fault::Fault;
use crate::fmath;
use crate::heap::{Heap, Object};
use crate::int::{self, Bin};
use crate::pow;

/// The binary dynamic arithmetic instructions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    FloorDiv,
    FloorMod,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    /// `dpow` (OPS v2).
    Pow,
}

impl DOp {
    fn bin(self) -> Bin {
        match self {
            DOp::Add => Bin::Add,
            DOp::Sub => Bin::Sub,
            DOp::Mul => Bin::Mul,
            DOp::Div => Bin::Div,
            DOp::Rem => Bin::Rem,
            DOp::FloorDiv => Bin::FloorDiv,
            DOp::FloorMod => Bin::FloorMod,
            DOp::And => Bin::And,
            DOp::Or => Bin::Or,
            DOp::Xor => Bin::Xor,
            DOp::Shl => Bin::Shl,
            DOp::Shr => Bin::Shr,
            DOp::Pow => Bin::Pow,
        }
    }
}

/// A numeric result before encoding.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Out {
    I(i64),
    F(f64),
}

impl Out {
    /// Encodes the result as a `dyn` word (boxing a large int).
    #[inline]
    pub(crate) fn encode(self, heap: &mut Heap) -> Result<u64, Fault> {
        match self {
            Out::I(i) => conv::encode_int(heap, i),
            Out::F(f) => Ok(dynv::from_f64(f)),
        }
    }
}

#[cold]
#[inline(never)]
fn div_zero(pol: Policy) -> Fault {
    Fault::ops(ErrorKind::DivByZero, pol.div_zero() == DivZero::Trap)
}

/// The integer path at `i64` under `pol`, `promote` included.
#[inline]
pub(crate) fn int_arith(op: DOp, pol: Policy, a: i64, b: i64) -> Result<Out, Fault> {
    if pol.overflow() == Overflow::Promote {
        // The exact result as an i128 (products of two i64 fit), returned as
        // an int when representable, else as the nearest f64.
        let exact = |r: i128| match i64::try_from(r) {
            Ok(i) => Out::I(i),
            Err(_) => Out::F(r as f64),
        };
        match op {
            DOp::Add => return Ok(exact(i128::from(a) + i128::from(b))),
            DOp::Sub => return Ok(exact(i128::from(a) - i128::from(b))),
            DOp::Mul => return Ok(exact(i128::from(a) * i128::from(b))),
            DOp::Div => {
                if b == 0 {
                    return Err(div_zero(pol));
                }
                // PHP's `/`: an int only for an exact, representable
                // quotient; otherwise the correctly rounded rational.
                if a.wrapping_rem(b) == 0 {
                    if let Some(q) = a.checked_div(b) {
                        return Ok(Out::I(q));
                    }
                }
                return Ok(Out::F(fmath::div_i64_to_f64(a, b)));
            }
            DOp::FloorDiv => {
                if b == 0 {
                    return Err(div_zero(pol));
                }
                if a == i64::MIN && b == -1 {
                    return Ok(Out::F(9_223_372_036_854_775_808.0));
                }
            }
            // The exact power when it fits, else its nearest f64; a
            // negative exponent takes the float rule (PHP's `2 ** -1`).
            DOp::Pow => {
                return Ok(match pow::pow_promote(a, b) {
                    Ok(i) => Out::I(i),
                    Err(f) => Out::F(f),
                });
            }
            // rem, floor_mod, bitwise ops, and shifts never overflow.
            _ => {}
        }
    }
    int::bin_t::<i64>(op.bin(), pol, a, b).map(Out::I)
}

/// The float path (OPS §4; CPython's algorithm for floor division/modulo).
/// `None` for the bitwise instructions, which have no float path.
#[inline]
pub(crate) fn float_arith(op: DOp, a: f64, b: f64) -> Option<f64> {
    Some(match op {
        DOp::Add => a + b,
        DOp::Sub => a - b,
        DOp::Mul => a * b,
        DOp::Div => a / b,
        DOp::Rem => a % b,
        DOp::FloorDiv => fmath::py_divmod(a, b).0,
        DOp::FloorMod => fmath::py_divmod(a, b).1,
        DOp::Pow => pow::ls_pow(a, b),
        _ => return None,
    })
}

/// The fastest path: both operands inline ints and the result inline too.
/// `None` defers to [`arith`] (which handles every other case, including
/// overflow under each policy). Inline ints are below 2^48 in magnitude, so
/// sums, differences, and bitwise results cannot overflow `i64`.
#[inline(always)]
pub(crate) fn arith_inline(op: DOp, a: u64, b: u64) -> Option<u64> {
    if !(dynv::is_inline_int(a) && dynv::is_inline_int(b)) {
        return None;
    }
    let (x, y) = (dynv::inline_int_value(a), dynv::inline_int_value(b));
    let r = match op {
        DOp::Add => x + y,
        DOp::Sub => x - y,
        DOp::Mul => x.checked_mul(y)?,
        DOp::And => x & y,
        DOp::Or => x | y,
        DOp::Xor => x ^ y,
        _ => return None,
    };
    dynv::inline_int(r)
}

/// A binary dynamic arithmetic instruction: `Some(result)` on the fast path,
/// `None` when the hook takes over.
#[inline]
pub(crate) fn arith(
    heap: &mut Heap,
    op: DOp,
    pol: Policy,
    a: u64,
    b: u64,
) -> Result<Option<u64>, Fault> {
    // The common case first: two inline ints.
    if dynv::is_inline_int(a) && dynv::is_inline_int(b) {
        let r = int_arith(
            op,
            pol,
            dynv::inline_int_value(a),
            dynv::inline_int_value(b),
        )?;
        return r.encode(heap).map(Some);
    }
    let (Some(x), Some(y)) = (conv::dyn_num(heap, a), conv::dyn_num(heap, b)) else {
        return Ok(None);
    };
    let out = match (x, y) {
        (Num::I(i), Num::I(j)) => int_arith(op, pol, i, j)?,
        (x, y) => {
            let f = |n: Num| match n {
                Num::I(i) => i as f64,
                Num::F(f) => f,
            };
            match float_arith(op, f(x), f(y)) {
                Some(r) => Out::F(r),
                None => return Ok(None),
            }
        }
    };
    out.encode(heap).map(Some)
}

/// `dneg` on the fast path.
pub(crate) fn neg(heap: &mut Heap, pol: Policy, a: u64) -> Result<Option<u64>, Fault> {
    let out = match conv::dyn_num(heap, a) {
        Some(Num::I(i)) => match i.checked_neg() {
            Some(r) => Out::I(r),
            None => match pol.overflow() {
                Overflow::Wrap => Out::I(i),
                Overflow::Promote => Out::F(9_223_372_036_854_775_808.0),
                other => return Err(int::overflow_fault(other)),
            },
        },
        Some(Num::F(f)) => Out::F(-f),
        None => return Ok(None),
    };
    out.encode(heap).map(Some)
}

/// `dabs` on the fast path: `abs` at `i64` (`abs(MIN)` per `overflow`,
/// `promote` giving `2^63`), `fabs` on a float.
pub(crate) fn abs(heap: &mut Heap, pol: Policy, a: u64) -> Result<Option<u64>, Fault> {
    let out = match conv::dyn_num(heap, a) {
        Some(Num::I(i)) => match i.checked_abs() {
            Some(r) => Out::I(r),
            None => match pol.overflow() {
                Overflow::Wrap => Out::I(i),
                Overflow::Promote => Out::F(9_223_372_036_854_775_808.0),
                other => return Err(int::overflow_fault(other)),
            },
        },
        Some(Num::F(f)) => Out::F(f64::from_bits(f.to_bits() & !(1 << 63))),
        None => return Ok(None),
    };
    out.encode(heap).map(Some)
}

/// `dbit_not` (bitwise complement) on the fast path.
pub(crate) fn not(heap: &mut Heap, a: u64) -> Result<Option<u64>, Fault> {
    match conv::dyn_num(heap, a) {
        Some(Num::I(i)) => conv::encode_int(heap, !i).map(Some),
        _ => Ok(None),
    }
}

/// Exact numeric comparison.
#[inline]
pub(crate) fn cmp_num(x: Num, y: Num) -> Option<Ordering> {
    match (x, y) {
        (Num::I(i), Num::I(j)) => Some(i.cmp(&j)),
        (Num::I(i), Num::F(f)) => fmath::cmp_int_float(i, f),
        (Num::F(f), Num::I(i)) => fmath::cmp_int_float(i, f).map(Ordering::reverse),
        (Num::F(f), Num::F(g)) => f.partial_cmp(&g),
    }
}

/// The built-in part of `deq`: `Some` for the pairs LSB defines, `None`
/// when the hook (or identity) decides.
pub(crate) fn eq_builtin(heap: &Heap, a: u64, b: u64) -> Option<bool> {
    if let (Some(x), Some(y)) = (conv::dyn_num(heap, a), conv::dyn_num(heap, b)) {
        return Some(cmp_num(x, y) == Some(Ordering::Equal));
    }
    match (dynv::decode(a), dynv::decode(b)) {
        (Raw::Nil, Raw::Nil) => Some(true),
        (Raw::Bool(x), Raw::Bool(y)) => Some(x == y),
        (Raw::Char(x), Raw::Char(y)) => Some(x == y),
        (Raw::Ref(..), Raw::Ref(..)) => match (heap.str(a), heap.str(b)) {
            (Some(x), Some(y)) => Some(x == y),
            _ => None,
        },
        _ => None,
    }
}

/// The built-in part of `dlt` (`le` false) and `dle` (`le` true).
pub(crate) fn lt_builtin(heap: &Heap, a: u64, b: u64, le: bool) -> Option<bool> {
    let ord = if let (Some(x), Some(y)) = (conv::dyn_num(heap, a), conv::dyn_num(heap, b)) {
        // NaN compares false either way.
        return Some(match cmp_num(x, y) {
            Some(o) => o == Ordering::Less || (le && o == Ordering::Equal),
            None => false,
        });
    } else {
        match (dynv::decode(a), dynv::decode(b)) {
            (Raw::Char(x), Raw::Char(y)) => x.cmp(&y),
            (Raw::Ref(..), Raw::Ref(..)) => match (heap.str(a), heap.str(b)) {
                (Some(x), Some(y)) => x.cmp(y),
                _ => return None,
            },
            _ => return None,
        }
    };
    Some(ord == Ordering::Less || (le && ord == Ordering::Equal))
}

/// Truthiness (LSB §5.6). `Ok(b)` for the four scalar kinds, which never
/// reach the hook; `Err(b)` with the built-in rule for every other kind,
/// which the `truthy` hook overrides when bound.
pub(crate) fn truthy(heap: &Heap, v: u64) -> Result<bool, bool> {
    match dynv::decode(v) {
        Raw::Nil => Ok(false),
        Raw::Bool(b) => Ok(b),
        Raw::Int(i) => Ok(i != 0),
        Raw::Float(f) => Ok(f != 0.0 || f.is_nan()),
        Raw::Char(_) => Err(true),
        Raw::Ref(..) => match heap.get(v) {
            // A collected object reads as nil.
            None => Ok(false),
            // Boxed ints are never zero (zero is inline).
            Some(Object::Int(_)) => Ok(true),
            Some(Object::Str(s)) => Err(!s.is_empty()),
            Some(Object::Array(a)) => Err(!a.items.is_empty()),
            Some(Object::Map(m)) => Err(m.store.len() != 0),
            Some(_) => Err(true),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_promote_add_overflow_gives_nearest_float() {
        let p = Policy::new().with_overflow(Overflow::Promote);
        assert_eq!(
            int_arith(DOp::Add, p, i64::MAX, 1),
            Ok(Out::F(9_223_372_036_854_775_808.0))
        );
        assert_eq!(int_arith(DOp::Add, p, 2, 3), Ok(Out::I(5)));
        assert_eq!(int_arith(DOp::Div, p, 7, 2), Ok(Out::F(3.5)));
        assert_eq!(int_arith(DOp::Div, p, 8, 2), Ok(Out::I(4)));
        assert_eq!(
            int_arith(DOp::Div, p, i64::MIN, -1),
            Ok(Out::F(9_223_372_036_854_775_808.0))
        );
        assert_eq!(
            int_arith(DOp::Div, p, 1, 0),
            Err(Fault::Raise(ErrorKind::DivByZero))
        );
        assert_eq!(int_arith(DOp::Rem, p, i64::MIN, -1), Ok(Out::I(0)));
    }

    #[test]
    fn test_float_paths() {
        assert_eq!(float_arith(DOp::FloorDiv, -7.0, 2.0), Some(-4.0));
        assert_eq!(float_arith(DOp::FloorMod, -7.0, 2.0), Some(1.0));
        assert_eq!(float_arith(DOp::Rem, -7.0, 2.0), Some(-1.0));
        assert_eq!(float_arith(DOp::Shl, 1.0, 2.0), None);
    }

    #[test]
    fn test_cmp_num_is_exact() {
        assert_eq!(
            cmp_num(Num::I((1 << 53) + 1), Num::F(9007199254740992.0)),
            Some(Ordering::Greater)
        );
        assert_eq!(cmp_num(Num::F(f64::NAN), Num::I(0)), None);
    }
}
