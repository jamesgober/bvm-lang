//! Typed integer operations and conversions, exactly as OPS §3 and §5 define
//! them, under every policy.
//!
//! A typed integer register holds its value sign-extended (signed types) or
//! zero-extended (unsigned types) to 64 bits. Every read goes through
//! [`Int::from_bits`], which narrows to the type, so a register holding stray
//! high bits (possible only in a module a verifier would reject) still reads as
//! a value of its type, and every result written back is normalised.

use core::ops::{BitAnd, BitOr, BitXor, Not};

use bytecode_lang::{DivZero, ErrorKind, IntConv, IntOp, IntPair, IntTy, Overflow, Policy, Shift};

use crate::fault::Fault;

/// The binary integer operations of LSB §5.2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Bin {
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
    Min,
    Max,
    /// OPS v2 `pow`: the exact power by repeated squaring.
    Pow,
}

/// The unary integer operations of LSB §5.2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Un {
    Neg,
    Not,
    Abs,
}

/// The integer comparisons of LSB §5.2 (also used by the float and char
/// comparison instructions).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    /// Applies the comparison to two ordered values.
    #[inline]
    pub(crate) fn test<T: PartialOrd>(self, a: T, b: T) -> bool {
        match self {
            Cmp::Eq => a == b,
            Cmp::Ne => a != b,
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
        }
    }
}

/// One of the eight LSB integer types, as a Rust primitive.
pub(crate) trait Int:
    Copy
    + Ord
    + BitAnd<Output = Self>
    + BitOr<Output = Self>
    + BitXor<Output = Self>
    + Not<Output = Self>
{
    const BITS: u32;
    fn from_bits(bits: u64) -> Self;
    fn to_bits(self) -> u64;
    fn to_i128(self) -> i128;
    fn checked_add(self, o: Self) -> Option<Self>;
    fn checked_sub(self, o: Self) -> Option<Self>;
    fn checked_mul(self, o: Self) -> Option<Self>;
    fn checked_div(self, o: Self) -> Option<Self>;
    fn wrapping_add(self, o: Self) -> Self;
    fn wrapping_sub(self, o: Self) -> Self;
    fn wrapping_mul(self, o: Self) -> Self;
    fn wrapping_div(self, o: Self) -> Self;
    fn wrapping_rem(self, o: Self) -> Self;
    fn checked_neg(self) -> Option<Self>;
    fn wrapping_neg(self) -> Self;
    fn checked_abs(self) -> Option<Self>;
    fn wrapping_abs(self) -> Self;
    fn is_zero(self) -> bool;
    fn is_negative(self) -> bool;
    fn minus_one(self) -> Self;
    fn wrapping_shl(self, n: u32) -> Self;
    fn wrapping_shr(self, n: u32) -> Self;
}

macro_rules! impl_int {
    ($($t:ty, $signed:tt, $via:ty);* $(;)?) => {$(
        impl Int for $t {
            const BITS: u32 = <$t>::BITS;
            #[inline]
            fn from_bits(bits: u64) -> Self {
                bits as $t
            }
            #[inline]
            fn to_bits(self) -> u64 {
                // Sign-extend through i64 for signed types, zero-extend for
                // unsigned ones.
                self as $via as u64
            }
            #[inline]
            fn to_i128(self) -> i128 {
                self as i128
            }
            #[inline]
            fn checked_add(self, o: Self) -> Option<Self> {
                <$t>::checked_add(self, o)
            }
            #[inline]
            fn checked_sub(self, o: Self) -> Option<Self> {
                <$t>::checked_sub(self, o)
            }
            #[inline]
            fn checked_mul(self, o: Self) -> Option<Self> {
                <$t>::checked_mul(self, o)
            }
            #[inline]
            fn checked_div(self, o: Self) -> Option<Self> {
                <$t>::checked_div(self, o)
            }
            #[inline]
            fn wrapping_add(self, o: Self) -> Self {
                <$t>::wrapping_add(self, o)
            }
            #[inline]
            fn wrapping_sub(self, o: Self) -> Self {
                <$t>::wrapping_sub(self, o)
            }
            #[inline]
            fn wrapping_mul(self, o: Self) -> Self {
                <$t>::wrapping_mul(self, o)
            }
            #[inline]
            fn wrapping_div(self, o: Self) -> Self {
                <$t>::wrapping_div(self, o)
            }
            #[inline]
            fn wrapping_rem(self, o: Self) -> Self {
                <$t>::wrapping_rem(self, o)
            }
            #[inline]
            fn checked_neg(self) -> Option<Self> {
                <$t>::checked_neg(self)
            }
            #[inline]
            fn wrapping_neg(self) -> Self {
                <$t>::wrapping_neg(self)
            }
            #[inline]
            fn checked_abs(self) -> Option<Self> {
                impl_int!(@abs $signed, self)
            }
            #[inline]
            fn wrapping_abs(self) -> Self {
                impl_int!(@wabs $signed, self)
            }
            #[inline]
            fn is_zero(self) -> bool {
                self == 0
            }
            #[inline]
            #[allow(unused_comparisons)]
            fn is_negative(self) -> bool {
                self < 0
            }
            #[inline]
            fn minus_one(self) -> Self {
                self.wrapping_sub(1)
            }
            #[inline]
            fn wrapping_shl(self, n: u32) -> Self {
                <$t>::wrapping_shl(self, n)
            }
            #[inline]
            fn wrapping_shr(self, n: u32) -> Self {
                <$t>::wrapping_shr(self, n)
            }
        }
    )*};
    (@abs true, $x:expr) => { $x.checked_abs() };
    (@abs false, $x:expr) => { Some($x) };
    (@wabs true, $x:expr) => { $x.wrapping_abs() };
    (@wabs false, $x:expr) => { $x };
}

impl_int! {
    i8, true, i64;
    i16, true, i64;
    i32, true, i64;
    i64, true, i64;
    u8, false, u64;
    u16, false, u64;
    u32, false, u64;
    u64, false, u64;
}

/// Runs `$body` with `$T` bound to the Rust type of `$ty`.
macro_rules! with_int {
    ($ty:expr, $T:ident => $body:expr) => {
        match $ty {
            IntTy::I8 => {
                type $T = i8;
                $body
            }
            IntTy::I16 => {
                type $T = i16;
                $body
            }
            IntTy::I32 => {
                type $T = i32;
                $body
            }
            IntTy::I64 => {
                type $T = i64;
                $body
            }
            IntTy::U8 => {
                type $T = u8;
                $body
            }
            IntTy::U16 => {
                type $T = u16;
                $body
            }
            IntTy::U32 => {
                type $T = u32;
                $body
            }
            IntTy::U64 => {
                type $T = u64;
                $body
            }
        }
    };
}

/// The fault for an overflow under `overflow` (never called for `wrap`).
#[cold]
#[inline(never)]
pub(crate) fn overflow_fault(overflow: Overflow) -> Fault {
    Fault::ops(ErrorKind::ArithOverflow, overflow == Overflow::Trap)
}

#[cold]
#[inline(never)]
fn div_zero_fault(policy: Policy) -> Fault {
    Fault::ops(ErrorKind::DivByZero, policy.div_zero() == DivZero::Trap)
}

#[cold]
#[inline(never)]
fn shift_fault() -> Fault {
    Fault::Raise(ErrorKind::ShiftOutOfRange)
}

/// The value to use when an exact result does not fit: the wrapped result
/// under `wrap`, the policy's fault otherwise. `promote` cannot reach a typed
/// instruction (the loader rejects it), so it is treated as `error`.
#[inline]
fn on_overflow<T: Int>(policy: Policy, wrapped: T) -> Result<T, Fault> {
    match policy.overflow() {
        Overflow::Wrap => Ok(wrapped),
        other => Err(overflow_fault(other)),
    }
}

/// `shl` (`left`) or `shr` of `a` by the amount `n` under `policy` (OPS v2):
/// an amount in `0..width` shifts; otherwise `mask` shifts by `n mod
/// width`, `saturate` (PHP) gives 0, or -1 for `shr` of a negative signed
/// value, and `error` raises. A negative amount is `ShiftOutOfRange` under
/// `saturate` too.
#[inline]
fn shift<T: Int>(a: T, n: T, policy: Policy, left: bool) -> Result<T, Fault> {
    let bits = n.to_bits();
    let in_range = !n.is_negative() && bits < u64::from(T::BITS);
    let amount = if in_range {
        // In range, so the narrowing is exact.
        bits as u32
    } else {
        match policy.shift() {
            // `n mod width`, taken on the two's-complement bits, so a
            // negative signed amount masks the same way the hardware would.
            Shift::Mask => (bits & u64::from(T::BITS - 1)) as u32,
            Shift::Saturate if !n.is_negative() => {
                let zero = T::from_bits(0);
                return Ok(if !left && a.is_negative() {
                    !zero
                } else {
                    zero
                });
            }
            _ => return Err(shift_fault()),
        }
    };
    Ok(if left {
        a.wrapping_shl(amount)
    } else {
        a.wrapping_shr(amount)
    })
}

/// OPS v2 `pow` at type `T`: the exact power by repeated squaring (`0 ** 0 =
/// 1`); a result that does not fit follows `overflow` (`wrap` keeps the low
/// bits of the exact power, which wrapping multiplication computes); a
/// negative exponent is `NegativeExponent` under every policy (it is not an
/// overflow, so `trap` does not apply).
#[inline]
pub(crate) fn pow_t<T: Int>(policy: Policy, base: T, exp: T) -> Result<T, Fault> {
    if exp.is_negative() {
        return Err(Fault::raise(ErrorKind::NegativeExponent));
    }
    let (r, overflowed) = pow_exact(base, exp.to_bits());
    if overflowed {
        on_overflow(policy, r)
    } else {
        Ok(r)
    }
}

/// `base ** e` with wrapping multiplication, and whether the exact power
/// overflows `T`. A square is taken only when a higher exponent bit remains,
/// so it is multiplied in later: an overflowing square (or product) means the
/// exact power overflows too, since every later factor has magnitude at
/// least 2 when `|base| >= 2`, and `|base| < 2` never overflows.
pub(crate) fn pow_exact<T: Int>(base: T, mut e: u64) -> (T, bool) {
    let mut acc = T::from_bits(1);
    let mut b = base;
    let mut overflowed = false;
    loop {
        if e & 1 == 1 {
            acc = match acc.checked_mul(b) {
                Some(r) => r,
                None => {
                    overflowed = true;
                    acc.wrapping_mul(b)
                }
            };
        }
        e >>= 1;
        if e == 0 {
            return (acc, overflowed);
        }
        b = match b.checked_mul(b) {
            Some(r) => r,
            None => {
                overflowed = true;
                b.wrapping_mul(b)
            }
        };
    }
}

/// One binary operation at type `T`.
#[inline]
pub(crate) fn bin_t<T: Int>(op: Bin, policy: Policy, a: T, b: T) -> Result<T, Fault> {
    Ok(match op {
        Bin::Add => match a.checked_add(b) {
            Some(r) => r,
            None => on_overflow(policy, a.wrapping_add(b))?,
        },
        Bin::Sub => match a.checked_sub(b) {
            Some(r) => r,
            None => on_overflow(policy, a.wrapping_sub(b))?,
        },
        Bin::Mul => match a.checked_mul(b) {
            Some(r) => r,
            None => on_overflow(policy, a.wrapping_mul(b))?,
        },
        Bin::Div => {
            if b.is_zero() {
                return Err(div_zero_fault(policy));
            }
            match a.checked_div(b) {
                Some(r) => r,
                // Only signed MIN / -1 overflows; `wrap` gives MIN.
                None => on_overflow(policy, a.wrapping_div(b))?,
            }
        }
        Bin::Rem => {
            if b.is_zero() {
                return Err(div_zero_fault(policy));
            }
            // MIN rem -1 is 0, never an error (OPS §3).
            a.wrapping_rem(b)
        }
        Bin::FloorDiv => {
            if b.is_zero() {
                return Err(div_zero_fault(policy));
            }
            let q = match a.checked_div(b) {
                Some(q) => q,
                None => return on_overflow(policy, a.wrapping_div(b)),
            };
            let r = a.wrapping_rem(b);
            if !r.is_zero() && (r.is_negative() != b.is_negative()) {
                // Truncation rounded toward zero on a negative quotient;
                // floor is one lower. Cannot overflow: q > MIN here.
                q.minus_one()
            } else {
                q
            }
        }
        Bin::FloorMod => {
            if b.is_zero() {
                return Err(div_zero_fault(policy));
            }
            let r = a.wrapping_rem(b);
            if !r.is_zero() && (r.is_negative() != b.is_negative()) {
                // |r| < |b| with opposite signs, so r + b is in range.
                r.wrapping_add(b)
            } else {
                r
            }
        }
        Bin::And => a & b,
        Bin::Or => a | b,
        Bin::Xor => a ^ b,
        Bin::Shl => shift(a, b, policy, true)?,
        Bin::Shr => shift(a, b, policy, false)?,
        Bin::Min => a.min(b),
        Bin::Max => a.max(b),
        Bin::Pow => pow_t(policy, a, b)?,
    })
}

/// One unary operation at type `T`.
#[inline]
pub(crate) fn un_t<T: Int>(op: Un, policy: Policy, a: T) -> Result<T, Fault> {
    Ok(match op {
        // `0 - x`: for unsigned types every non-zero `x` overflows.
        Un::Neg => match a.checked_neg() {
            Some(r) => r,
            None => on_overflow(policy, a.wrapping_neg())?,
        },
        Un::Not => !a,
        Un::Abs => match a.checked_abs() {
            Some(r) => r,
            None => on_overflow(policy, a.wrapping_abs())?,
        },
    })
}

/// A binary integer instruction on register bits. `i64`, by far the most
/// common type, is tested first so the hot path has no type dispatch.
#[inline(always)]
pub(crate) fn bin(op: Bin, int_op: IntOp, a: u64, b: u64) -> Result<u64, Fault> {
    if int_op.ty() == IntTy::I64 {
        return bin_t::<i64>(op, int_op.policy(), a as i64, b as i64).map(|r| r as u64);
    }
    bin_other(op, int_op, a, b)
}

#[inline(never)]
fn bin_other(op: Bin, int_op: IntOp, a: u64, b: u64) -> Result<u64, Fault> {
    let policy = int_op.policy();
    with_int!(int_op.ty(), T => {
        bin_t::<T>(op, policy, T::from_bits(a), T::from_bits(b)).map(Int::to_bits)
    })
}

/// A unary integer instruction on register bits.
#[inline]
pub(crate) fn un(op: Un, int_op: IntOp, a: u64) -> Result<u64, Fault> {
    let policy = int_op.policy();
    with_int!(int_op.ty(), T => {
        un_t::<T>(op, policy, T::from_bits(a)).map(Int::to_bits)
    })
}

/// An integer comparison on register bits (`i64` first, as in [`bin`]).
#[inline(always)]
pub(crate) fn cmp(op: Cmp, ty: IntTy, a: u64, b: u64) -> bool {
    if ty == IntTy::I64 {
        return op.test(a as i64, b as i64);
    }
    with_int!(ty, T => op.test(T::from_bits(a), T::from_bits(b)))
}

/// Narrows register bits to the type's canonical representation.
#[inline]
pub(crate) fn normalize(ty: IntTy, bits: u64) -> u64 {
    with_int!(ty, T => T::from_bits(bits).to_bits())
}

/// The exact value of register bits read at `ty`.
#[inline]
pub(crate) fn value(ty: IntTy, bits: u64) -> i128 {
    with_int!(ty, T => T::from_bits(bits).to_i128())
}

/// The register bits of `v` at `ty`, when it is representable.
#[inline]
pub(crate) fn from_value(ty: IntTy, v: i128) -> Option<u64> {
    with_int!(ty, T => T::try_from(v).ok().map(Int::to_bits))
}

/// `int_cast` (OPS §5): exact if it fits, else per the overflow policy, where
/// `wrap` truncates to the target width and re-extends.
#[inline]
pub(crate) fn int_cast(conv: IntConv, bits: u64) -> Result<u64, Fault> {
    let v = value(conv.from(), bits);
    match from_value(conv.to(), v) {
        Some(r) => Ok(r),
        None => match conv.overflow() {
            Overflow::Wrap => Ok(normalize(conv.to(), v as u64)),
            other => Err(overflow_fault(other)),
        },
    }
}

/// The low `width` bits of `bits`.
#[inline]
fn low_bits(bits: u64, width: u32) -> u64 {
    if width >= 64 {
        bits
    } else {
        bits & ((1u64 << width) - 1)
    }
}

/// `zext`: the source bits, read unsigned at the source width, re-read at the
/// target type.
#[inline]
pub(crate) fn zext(pair: IntPair, bits: u64) -> u64 {
    normalize(pair.to(), low_bits(bits, pair.from().bits()))
}

/// `sext`: the source bits sign-extended from the source width, re-read at the
/// target type.
#[inline]
pub(crate) fn sext(pair: IntPair, bits: u64) -> u64 {
    let width = pair.from().bits();
    let shift = 64 - width;
    let extended = ((bits << shift) as i64 >> shift) as u64;
    normalize(pair.to(), extended)
}

/// `trunc`: the low bits of the target width.
#[inline]
pub(crate) fn trunc(pair: IntPair, bits: u64) -> u64 {
    normalize(pair.to(), bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(ty: IntTy, policy: Policy) -> IntOp {
        IntOp::new(ty).with_policy(policy)
    }

    #[test]
    fn test_add_overflow_under_each_policy() {
        let max = i64::MAX as u64;
        let one = 1u64;
        let p = Policy::new();
        assert_eq!(
            bin(Bin::Add, op(IntTy::I64, p), max, one),
            Err(Fault::Raise(ErrorKind::ArithOverflow))
        );
        assert_eq!(
            bin(
                Bin::Add,
                op(IntTy::I64, p.with_overflow(Overflow::Wrap)),
                max,
                one
            ),
            Ok(i64::MIN as u64)
        );
        assert_eq!(
            bin(
                Bin::Add,
                op(IntTy::I64, p.with_overflow(Overflow::Trap)),
                max,
                one
            ),
            Err(Fault::Trap(ErrorKind::ArithOverflow))
        );
    }

    #[test]
    fn test_narrow_results_are_normalised() {
        let p = Policy::new().with_overflow(Overflow::Wrap);
        // i8: 127 + 1 wraps to -128, stored sign-extended.
        assert_eq!(bin(Bin::Add, op(IntTy::I8, p), 127, 1), Ok(-128i64 as u64));
        // u8: 0 - 1 wraps to 255, stored zero-extended.
        assert_eq!(bin(Bin::Sub, op(IntTy::U8, p), 0, 1), Ok(255));
        // u8 not: complement stays within the type.
        assert_eq!(un(Un::Not, op(IntTy::U8, p), 0), Ok(255));
    }

    #[test]
    fn test_division_edges() {
        let p = Policy::new();
        let min = i64::MIN as u64;
        let neg1 = -1i64 as u64;
        assert_eq!(
            bin(Bin::Div, op(IntTy::I64, p), 5, 0),
            Err(Fault::Raise(ErrorKind::DivByZero))
        );
        assert_eq!(
            bin(
                Bin::Div,
                op(IntTy::I64, p.with_div_zero(DivZero::Trap)),
                5,
                0
            ),
            Err(Fault::Trap(ErrorKind::DivByZero))
        );
        assert_eq!(
            bin(Bin::Div, op(IntTy::I64, p), min, neg1),
            Err(Fault::Raise(ErrorKind::ArithOverflow))
        );
        assert_eq!(bin(Bin::Rem, op(IntTy::I64, p), min, neg1), Ok(0));
        assert_eq!(bin(Bin::FloorMod, op(IntTy::I64, p), min, neg1), Ok(0));
        assert_eq!(
            bin(Bin::FloorDiv, op(IntTy::I64, p), -7i64 as u64, 2),
            Ok(-4i64 as u64)
        );
        assert_eq!(
            bin(Bin::FloorMod, op(IntTy::I64, p), -7i64 as u64, 2),
            Ok(1)
        );
        assert_eq!(
            bin(Bin::FloorMod, op(IntTy::I64, p), 7, -2i64 as u64),
            Ok(-1i64 as u64)
        );
    }

    #[test]
    fn test_shift_policies() {
        let p = Policy::new();
        assert_eq!(
            bin(Bin::Shl, op(IntTy::I32, p), 1, 32),
            Err(Fault::Raise(ErrorKind::ShiftOutOfRange))
        );
        assert_eq!(
            bin(Bin::Shl, op(IntTy::I32, p.with_shift(Shift::Mask)), 1, 33),
            Ok(2)
        );
        assert_eq!(
            bin(Bin::Shr, op(IntTy::I32, p), -1i64 as u64, -1i64 as u64),
            Err(Fault::Raise(ErrorKind::ShiftOutOfRange))
        );
        assert_eq!(
            bin(Bin::Shr, op(IntTy::U8, p), 0x80, 7),
            Ok(1),
            "unsigned shr is logical"
        );
        assert_eq!(
            bin(Bin::Shr, op(IntTy::I8, p), -128i64 as u64, 7),
            Ok(-1i64 as u64),
            "signed shr is arithmetic"
        );
    }

    #[test]
    fn test_saturating_shifts_follow_php() {
        let sat = Policy::new().with_shift(Shift::Saturate);
        let neg = |v: i64| v as u64;
        // Amount >= width: 0, or -1 for shr of a negative signed value.
        assert_eq!(bin(Bin::Shl, op(IntTy::I64, sat), 1, 64), Ok(0));
        assert_eq!(bin(Bin::Shl, op(IntTy::I64, sat), 1, 1000), Ok(0));
        assert_eq!(bin(Bin::Shr, op(IntTy::I64, sat), neg(-8), 64), Ok(neg(-1)));
        assert_eq!(bin(Bin::Shr, op(IntTy::I64, sat), 8, 64), Ok(0));
        assert_eq!(bin(Bin::Shr, op(IntTy::U8, sat), 0x80, 8), Ok(0));
        assert_eq!(bin(Bin::Shr, op(IntTy::I8, sat), neg(-1), 100), Ok(neg(-1)));
        // In range: an ordinary shift.
        assert_eq!(bin(Bin::Shl, op(IntTy::I64, sat), 1, 63), Ok(1 << 63));
        // A negative amount is an error under saturate too (PHP's
        // ArithmeticError).
        assert_eq!(
            bin(Bin::Shl, op(IntTy::I64, sat), 1, neg(-1)),
            Err(Fault::Raise(ErrorKind::ShiftOutOfRange))
        );
        assert_eq!(
            bin(Bin::Shr, op(IntTy::I32, sat), 1, neg(-5)),
            Err(Fault::Raise(ErrorKind::ShiftOutOfRange))
        );
    }

    #[test]
    fn test_pow_is_exact_with_policies() {
        let p = Policy::new();
        let wrap = p.with_overflow(Overflow::Wrap);
        let neg = |v: i64| v as u64;
        assert_eq!(bin(Bin::Pow, op(IntTy::I64, p), 0, 0), Ok(1));
        assert_eq!(bin(Bin::Pow, op(IntTy::I64, p), 3, 4), Ok(81));
        assert_eq!(
            bin(Bin::Pow, op(IntTy::I64, p), neg(-2), 63),
            Ok(neg(i64::MIN))
        );
        assert_eq!(
            bin(Bin::Pow, op(IntTy::I64, p), 2, 63),
            Err(Fault::Raise(ErrorKind::ArithOverflow))
        );
        assert_eq!(bin(Bin::Pow, op(IntTy::I64, wrap), 2, 64), Ok(0));
        assert_eq!(
            bin(Bin::Pow, op(IntTy::I64, wrap), 3, 41),
            Ok(3u64.wrapping_pow(41))
        );
        assert_eq!(bin(Bin::Pow, op(IntTy::U8, wrap), 3, 5), Ok(243));
        assert_eq!(bin(Bin::Pow, op(IntTy::U8, wrap), 3, 6), Ok(729 % 256));
        assert_eq!(
            bin(Bin::Pow, op(IntTy::I8, p), neg(-1), 255),
            Err(Fault::Raise(ErrorKind::NegativeExponent))
        );
        assert_eq!(bin(Bin::Pow, op(IntTy::U8, p), 1, 255), Ok(1));
        assert_eq!(
            bin(Bin::Pow, op(IntTy::I64, p), neg(-1), neg(i64::MAX)),
            Ok(neg(-1))
        );
        // A negative exponent is never an overflow: not even `trap` traps.
        let trap = p.with_overflow(Overflow::Trap);
        assert_eq!(
            bin(Bin::Pow, op(IntTy::I32, trap), 2, neg(-1)),
            Err(Fault::Raise(ErrorKind::NegativeExponent))
        );
        assert_eq!(
            bin(Bin::Pow, op(IntTy::I32, trap), 2, 40),
            Err(Fault::Trap(ErrorKind::ArithOverflow))
        );
    }

    #[test]
    fn test_pow_matches_i128_for_every_small_case() {
        let wrap = Policy::new().with_overflow(Overflow::Wrap);
        for ty in [IntTy::I8, IntTy::U8, IntTy::I16] {
            let (lo, hi) = if ty.is_signed() {
                (-(1i128 << (ty.bits() - 1)), (1i128 << (ty.bits() - 1)) - 1)
            } else {
                (0, (1i128 << ty.bits()) - 1)
            };
            for base in [lo, lo + 1, -3, -2, -1, 0, 1, 2, 3, 7, hi - 1, hi] {
                if base < lo || base > hi {
                    continue;
                }
                for e in 0..20i128 {
                    let exact = (0..e).try_fold(1i128, |acc, _| acc.checked_mul(base));
                    let b = from_value(ty, base).unwrap_or(0);
                    let x = from_value(ty, e).unwrap_or(0);
                    let got = bin(Bin::Pow, op(ty, Policy::new()), b, x);
                    match exact.filter(|v| (lo..=hi).contains(v)) {
                        Some(v) => assert_eq!(
                            got,
                            from_value(ty, v).ok_or(Fault::Trap(ErrorKind::OutOfFuel)),
                            "{ty:?} {base}**{e}"
                        ),
                        None => assert_eq!(
                            got,
                            Err(Fault::Raise(ErrorKind::ArithOverflow)),
                            "{ty:?} {base}**{e}"
                        ),
                    }
                    // `wrap` keeps the low bits of the exact power.
                    let mut low: i128 = 1;
                    for _ in 0..e {
                        low = (low * base).rem_euclid(1i128 << ty.bits());
                    }
                    let wrapped = normalize(ty, low as u64);
                    assert_eq!(
                        bin(Bin::Pow, op(ty, wrap), b, x),
                        Ok(wrapped),
                        "{ty:?} {base}**{e} wrap"
                    );
                }
            }
        }
    }

    #[test]
    fn test_conversions() {
        let wrap = IntConv::new(IntTy::I64, IntTy::U8, Overflow::Wrap);
        assert_eq!(int_cast(wrap, 300), Ok(44));
        let err = IntConv::new(IntTy::I64, IntTy::U8, Overflow::Error);
        assert_eq!(
            int_cast(err, -1i64 as u64),
            Err(Fault::Raise(ErrorKind::ArithOverflow))
        );
        assert_eq!(zext(IntPair::new(IntTy::I8, IntTy::I32), -1i64 as u64), 255);
        assert_eq!(sext(IntPair::new(IntTy::U8, IntTy::U32), 0xFF), 0xFFFF_FFFF);
        assert_eq!(
            trunc(IntPair::new(IntTy::I64, IntTy::I8), 0x1FF),
            -1i64 as u64
        );
    }
}
