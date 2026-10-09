//! Float primitives with exact, platform-independent results.
//!
//! OPS §4 requires correctly rounded `sqrt` and `fma`, exact `floor`/`ceil`/
//! `trunc`/`round`, a truncated remainder (`fmod`), and the IEEE 754
//! `remainder`. With the `std` feature the first group comes from the standard
//! library (hardware instructions where the target has them, all correctly
//! rounded by IEEE 754); without it, the [`soft`] routines below compute the
//! same bits in integer arithmetic. `remainder` has no `std` counterpart, so
//! [`rem_ieee`] is always the software routine. The tests compare every
//! software routine with the standard library bit for bit.
//!
//! The module also holds the two exact numeric algorithms the dynamic
//! instructions need: the correctly rounded quotient of two `i64`s (`ddiv`
//! under `promote`, LSB §5.6) and CPython's float floor division and modulo.

use core::cmp::Ordering;

const SIGN64: u64 = 1 << 63;
const FRAC64: u64 = (1 << 52) - 1;

/// A binary floating-point format: precision (with the implicit bit), minimum
/// normal exponent, exponent bias, and exponent field width.
#[derive(Clone, Copy)]
pub(crate) struct Fmt {
    precision: u32,
    emin: i32,
    bias: i32,
    exp_bits: u32,
}

pub(crate) const F64: Fmt = Fmt {
    precision: 53,
    emin: -1022,
    bias: 1023,
    exp_bits: 11,
};

pub(crate) const F32: Fmt = Fmt {
    precision: 24,
    emin: -126,
    bias: 127,
    exp_bits: 8,
};

impl Fmt {
    const fn frac_bits(self) -> u32 {
        self.precision - 1
    }

    const fn inf_bits(self, neg: bool) -> u64 {
        let exp = ((1u64 << self.exp_bits) - 1) << self.frac_bits();
        exp | ((neg as u64) << (self.exp_bits + self.frac_bits()))
    }

    const fn zero_bits(self, neg: bool) -> u64 {
        (neg as u64) << (self.exp_bits + self.frac_bits())
    }

    const fn nan_bits(self) -> u64 {
        self.inf_bits(false) | (1u64 << (self.frac_bits() - 1))
    }
}

/// Rounds `(m + s) * 2^e` to the nearest value of `fmt` (ties to even), where
/// `s` is 0 when `sticky` is false and strictly between 0 and 1 otherwise.
/// Returns the result's bits in `fmt`. Callers that pass `sticky` supply at
/// least `precision + 2` significant bits in `m`, so the inexact part always
/// lies below the rounding position.
pub(crate) fn round_to(fmt: Fmt, neg: bool, m: u128, e: i32, sticky: bool) -> u64 {
    if m == 0 {
        return fmt.zero_bits(neg);
    }
    let len = 128 - m.leading_zeros() as i32;
    let p = fmt.precision as i32;
    let top = e + len - 1;
    // The exponent of the result's least significant bit: p bits below the
    // leading bit for a normal result, fixed for a subnormal one.
    let lsb = if top < fmt.emin {
        fmt.emin - (p - 1)
    } else {
        top - (p - 1)
    };
    let shift = lsb - e;
    let mut mant: u128 = if shift <= 0 {
        // Exact: at most p significant bits, shifted into place.
        m << ((-shift) as u32)
    } else if shift > 128 {
        // Half an ulp is 2^(shift-1) >= 2^128 > m: rounds to zero.
        0
    } else {
        let shift = shift as u32;
        let (kept, rem) = if shift == 128 {
            (0, m)
        } else {
            (m >> shift, m & ((1u128 << shift) - 1))
        };
        let half = 1u128 << (shift - 1);
        let up = rem > half || (rem == half && (sticky || kept & 1 == 1));
        kept + up as u128
    };
    let mut lsb = lsb;
    if mant == 1u128 << p {
        // Rounding carried into a new bit.
        mant >>= 1;
        lsb += 1;
    }
    if mant == 0 {
        return fmt.zero_bits(neg);
    }
    let frac_mask = (1u128 << fmt.frac_bits()) - 1;
    let biased = if mant >> fmt.frac_bits() == 0 {
        0 // subnormal
    } else {
        lsb + (p - 1) + fmt.bias
    };
    if biased >= (1 << fmt.exp_bits) - 1 {
        return fmt.inf_bits(neg);
    }
    let bits = ((biased as u64) << fmt.frac_bits()) | (mant & frac_mask) as u64;
    bits | fmt.zero_bits(neg)
}

/// A finite non-zero `f64` as `(negative, mantissa, exponent)` with the value
/// `mantissa * 2^exponent` and the mantissa normalised to exactly 53 bits.
fn parts(x: f64) -> (bool, u64, i32) {
    let bits = x.to_bits();
    let neg = bits & SIGN64 != 0;
    let exp = ((bits >> 52) & 0x7FF) as i32;
    let frac = bits & FRAC64;
    let (mut m, mut e) = if exp == 0 {
        (frac, -1074)
    } else {
        (frac | (1 << 52), exp - 1075)
    };
    while m & (1 << 52) == 0 {
        m <<= 1;
        e -= 1;
    }
    (neg, m, e)
}

/// The IEEE 754 `remainder`: `x - n*y` with `n` the integer nearest `x/y`
/// (ties to even). Exact, so the result is representable.
pub(crate) fn rem_ieee(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() || x.is_infinite() || y == 0.0 {
        return f64::NAN;
    }
    if y.is_infinite() || x == 0.0 {
        return x;
    }
    let (xneg, xm, xe) = parts(x);
    let (_, ym, ye) = parts(y);
    // `r` and `odd` are the truncated remainder |x| mod |y| (as `rm * 2^re`)
    // and the parity of the truncated quotient.
    let (rm, re, odd): (u128, i32, bool) = if xe >= ye {
        // Long division, one quotient bit per step; at most ~2100 steps.
        let mut m = u128::from(xm);
        let d = u128::from(ym);
        let mut q: u64 = 0;
        for _ in 0..(xe - ye) {
            if m >= d {
                m -= d;
                q = q.wrapping_add(1);
            }
            m <<= 1;
            q = q.wrapping_shl(1);
        }
        if m >= d {
            m -= d;
            q = q.wrapping_add(1);
        }
        (m, ye, q & 1 == 1)
    } else {
        (u128::from(xm), xe, false)
    };
    // Compare 2r with |y| at a common exponent.
    let (two_r, y_al, base) = if re >= ye {
        (rm << (re - ye + 1), u128::from(ym), ye)
    } else if ye - re <= 64 {
        (rm << 1, u128::from(ym) << (ye - re), re)
    } else {
        // |y| is astronomically larger than 2r: no adjustment.
        return x;
    };
    let flip = two_r > y_al || (two_r == y_al && odd);
    if flip {
        // r - |y|, negative relative to x: magnitude |y| - r.
        let r_al = two_r >> 1;
        let mag = y_al - r_al;
        f64::from_bits(round_to(F64, !xneg, mag, base, false))
    } else {
        f64::from_bits(round_to(F64, xneg, rm, re, false))
    }
}

/// `remainder` for `f32`: exact in `f64`, and the exact result is an `f32`.
pub(crate) fn rem_ieee_f32(x: f32, y: f32) -> f32 {
    rem_ieee(f64::from(x), f64::from(y)) as f32
}

/// The correctly rounded quotient `a / b` of two integers as an `f64`
/// (`b != 0`). Dividing after converting each operand to `f64` is not
/// correctly rounded once an operand exceeds 2^53 (LSB §5.6), so this works on
/// the exact rational.
pub(crate) fn div_i64_to_f64(a: i64, b: i64) -> f64 {
    let neg = (a < 0) != (b < 0);
    let n = u128::from(a.unsigned_abs());
    let d = u128::from(b.unsigned_abs());
    if n == 0 {
        return if neg { -0.0 } else { 0.0 };
    }
    let len = |v: u128| 128 - v.leading_zeros() as i32;
    // Scale so the integer quotient has at least 55 significant bits; the
    // shifted numerator stays below 2^121.
    let s = (56 + len(d) - len(n)).max(0);
    let num = n << s;
    let q = num / d;
    let sticky = num % d != 0;
    f64::from_bits(round_to(F64, neg, q, -s, sticky))
}

/// Exact comparison of an `i64` with an `f64` (`None` when `f` is NaN).
pub(crate) fn cmp_int_float(i: i64, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    // 2^63 is exactly representable; every f64 at or above it exceeds all
    // i64s, every f64 below -2^63 is below all of them.
    if f >= 9_223_372_036_854_775_808.0 {
        return Some(Ordering::Less);
    }
    if f < -9_223_372_036_854_775_808.0 {
        return Some(Ordering::Greater);
    }
    let t = trunc(f);
    // |t| <= 2^63 and t is integral, so the conversion is exact (t = -2^63
    // converts exactly; t < 2^63 here).
    let ti = t as i64;
    Some(match i.cmp(&ti) {
        Ordering::Equal => {
            if f > t {
                Ordering::Less
            } else if f < t {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        other => other,
    })
}

/// CPython's float floor division and modulo (LSB §5.6), as one pair.
pub(crate) fn py_divmod(a: f64, b: f64) -> (f64, f64) {
    let mut m = a % b;
    let mut q = (a - m) / b;
    // "Non-zero" counts NaN as non-zero, as C's `if (mod)` does.
    if m != 0.0 || m.is_nan() {
        if (b < 0.0) != (m < 0.0) {
            m += b;
            q -= 1.0;
        }
    } else {
        m = copysign(0.0, b);
    }
    let fq = if q != 0.0 || q.is_nan() {
        let mut fq = floor(q);
        if q - fq > 0.5 {
            fq += 1.0;
        }
        fq
    } else {
        copysign(0.0, a / b)
    };
    (fq, m)
}

/// `x` with the sign of `s`.
#[inline]
pub(crate) fn copysign(x: f64, s: f64) -> f64 {
    f64::from_bits((x.to_bits() & !SIGN64) | (s.to_bits() & SIGN64))
}

/// `min` per OPS §4: NaN if either is NaN, `min(-0, +0) = -0`.
#[inline]
pub(crate) fn fmin<T: Float>(a: T, b: T) -> T {
    if a.nan() || b.nan() {
        T::NAN_VALUE
    } else if a < b {
        a
    } else if b < a {
        b
    } else if a.negative() {
        a
    } else {
        b
    }
}

/// `max` per OPS §4: NaN if either is NaN, `max(-0, +0) = +0`.
#[inline]
pub(crate) fn fmax<T: Float>(a: T, b: T) -> T {
    if a.nan() || b.nan() {
        T::NAN_VALUE
    } else if a > b {
        a
    } else if b > a || a.negative() {
        b
    } else {
        a
    }
}

/// The two float widths, for the generic helpers above.
pub(crate) trait Float: Copy + PartialOrd {
    const NAN_VALUE: Self;
    fn nan(self) -> bool;
    fn negative(self) -> bool;
}

impl Float for f64 {
    const NAN_VALUE: Self = f64::NAN;
    fn nan(self) -> bool {
        self.is_nan()
    }
    fn negative(self) -> bool {
        self.is_sign_negative()
    }
}

impl Float for f32 {
    const NAN_VALUE: Self = f32::NAN;
    fn nan(self) -> bool {
        self.is_nan()
    }
    fn negative(self) -> bool {
        self.is_sign_negative()
    }
}

// ---------------------------------------------------------------------------
// The rounding family, sqrt, and fma: std when available, software otherwise.
// ---------------------------------------------------------------------------

macro_rules! pick {
    ($(fn $name:ident($($a:ident: $t:ty),*) -> $r:ty = $std:expr, $soft:expr;)*) => {$(
        #[inline]
        pub(crate) fn $name($($a: $t),*) -> $r {
            #[cfg(feature = "std")]
            {
                $std
            }
            #[cfg(not(feature = "std"))]
            {
                $soft
            }
        }
    )*};
}

pick! {
    fn trunc(x: f64) -> f64 = x.trunc(), soft::trunc(x);
    fn floor(x: f64) -> f64 = x.floor(), soft::floor(x);
    fn ceil(x: f64) -> f64 = x.ceil(), soft::ceil(x);
    fn round(x: f64) -> f64 = x.round(), soft::round(x);
    fn round_even(x: f64) -> f64 = x.round_ties_even(), soft::round_even(x);
    fn sqrt(x: f64) -> f64 = x.sqrt(), soft::sqrt(x);
    fn fma(a: f64, b: f64, c: f64) -> f64 = a.mul_add(b, c), soft::fma(a, b, c);
    fn trunc_f32(x: f32) -> f32 = x.trunc(), soft::trunc(f64::from(x)) as f32;
    fn floor_f32(x: f32) -> f32 = x.floor(), soft::floor(f64::from(x)) as f32;
    fn ceil_f32(x: f32) -> f32 = x.ceil(), soft::ceil(f64::from(x)) as f32;
    fn round_f32(x: f32) -> f32 = x.round(), soft::round(f64::from(x)) as f32;
    fn round_even_f32(x: f32) -> f32 = x.round_ties_even(), soft::round_even(f64::from(x)) as f32;
    fn sqrt_f32(x: f32) -> f32 = x.sqrt(), soft::sqrt(f64::from(x)) as f32;
    fn fma_f32(a: f32, b: f32, c: f32) -> f32 = a.mul_add(b, c), soft::fma_f32(a, b, c);
}

/// Software implementations, used without `std` and tested against `std`.
#[cfg_attr(feature = "std", allow(dead_code))]
pub(crate) mod soft {
    use super::{F32, F64, FRAC64, Fmt, SIGN64, parts, round_to};

    /// Rounds toward zero.
    pub(crate) fn trunc(x: f64) -> f64 {
        let bits = x.to_bits();
        let exp = ((bits >> 52) & 0x7FF) as i32 - 1023;
        if exp >= 52 {
            return x; // integral, infinite, or NaN
        }
        if exp < 0 {
            return f64::from_bits(bits & SIGN64); // |x| < 1
        }
        let mask = FRAC64 >> exp;
        f64::from_bits(bits & !mask)
    }

    /// Rounds toward negative infinity.
    pub(crate) fn floor(x: f64) -> f64 {
        let t = trunc(x);
        if t != x && x < 0.0 { t - 1.0 } else { t }
    }

    /// Rounds toward positive infinity.
    pub(crate) fn ceil(x: f64) -> f64 {
        let t = trunc(x);
        if t != x && x > 0.0 { t + 1.0 } else { t }
    }

    fn away(t: f64, x: f64) -> f64 {
        if x < 0.0 { t - 1.0 } else { t + 1.0 }
    }

    fn abs(x: f64) -> f64 {
        f64::from_bits(x.to_bits() & !SIGN64)
    }

    /// Rounds to nearest, ties away from zero.
    pub(crate) fn round(x: f64) -> f64 {
        let t = trunc(x);
        // Exact: |x| < 2^52 whenever x has a fraction.
        if abs(x - t) >= 0.5 { away(t, x) } else { t }
    }

    /// Rounds to nearest, ties to even.
    pub(crate) fn round_even(x: f64) -> f64 {
        let t = trunc(x);
        let d = abs(x - t);
        // |t| < 2^52 when d != 0, so the conversion is exact.
        let odd = (t as i64) & 1 == 1;
        if d > 0.5 || (d == 0.5 && odd) {
            away(t, x)
        } else {
            t
        }
    }

    /// The correctly rounded square root.
    pub(crate) fn sqrt(x: f64) -> f64 {
        if x.is_nan() || x == 0.0 || x == f64::INFINITY {
            return x;
        }
        if x < 0.0 {
            return f64::NAN;
        }
        let (_, m, e) = parts(x);
        let (mut m, mut e) = (u128::from(m), e);
        if e & 1 != 0 {
            m <<= 1;
            e -= 1;
        }
        // m in [2^52, 2^54); m << 60 has a root of at least 56 bits.
        let wide = m << 60;
        let r = wide.isqrt();
        let sticky = r * r != wide;
        f64::from_bits(round_to(F64, false, r, (e - 60) / 2, sticky))
    }

    /// A 256-bit unsigned integer, enough for the exact sum inside `fma`.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct U256 {
        hi: u128,
        lo: u128,
    }

    impl U256 {
        const fn new(v: u128) -> Self {
            U256 { hi: 0, lo: v }
        }

        fn shl(self, n: u32) -> Self {
            match n {
                0 => self,
                1..=127 => U256 {
                    hi: (self.hi << n) | (self.lo >> (128 - n)),
                    lo: self.lo << n,
                },
                128..=255 => U256 {
                    hi: self.lo << (n - 128),
                    lo: 0,
                },
                _ => U256 { hi: 0, lo: 0 },
            }
        }

        fn add(self, o: Self) -> Self {
            let (lo, carry) = self.lo.overflowing_add(o.lo);
            U256 {
                hi: self.hi.wrapping_add(o.hi).wrapping_add(carry as u128),
                lo,
            }
        }

        fn sub(self, o: Self) -> Self {
            let (lo, borrow) = self.lo.overflowing_sub(o.lo);
            U256 {
                hi: self.hi.wrapping_sub(o.hi).wrapping_sub(borrow as u128),
                lo,
            }
        }

        fn bits(self) -> u32 {
            if self.hi != 0 {
                256 - self.hi.leading_zeros()
            } else {
                128 - self.lo.leading_zeros()
            }
        }

        /// Shifts right by `n`, reporting whether a set bit fell off.
        fn shr_sticky(self, n: u32) -> (u128, bool) {
            match n {
                0 => (self.lo, false),
                1..=127 => {
                    let lost = self.lo & ((1u128 << n) - 1) != 0;
                    ((self.lo >> n) | (self.hi << (128 - n)), lost)
                }
                128 => (self.hi, self.lo != 0),
                _ => {
                    let k = n - 128;
                    let lost = self.lo != 0 || self.hi & ((1u128 << k) - 1) != 0;
                    (self.hi >> k, lost)
                }
            }
        }
    }

    /// `a * b + c` with one rounding into `fmt` (inputs are exact in `f64`).
    fn fma_core(fmt: Fmt, a: f64, b: f64, c: f64) -> u64 {
        if a.is_nan() || b.is_nan() || c.is_nan() {
            return fmt.nan_bits();
        }
        let pneg = a.is_sign_negative() != b.is_sign_negative();
        if a.is_infinite() || b.is_infinite() {
            if a == 0.0 || b == 0.0 {
                return fmt.nan_bits(); // inf * 0
            }
            if c.is_infinite() && c.is_sign_negative() != pneg {
                return fmt.nan_bits(); // inf - inf
            }
            return fmt.inf_bits(pneg);
        }
        if c.is_infinite() {
            return fmt.inf_bits(c.is_sign_negative());
        }
        let cneg = c.is_sign_negative();
        if a == 0.0 || b == 0.0 {
            // Exact zero product: the result is c, or a zero whose sign
            // follows IEEE addition of zeros (round to nearest: -0 only when
            // both are -0).
            if c == 0.0 {
                return fmt.zero_bits(pneg && cneg);
            }
            let (_, cm, ce) = parts(c);
            return round_to(fmt, cneg, u128::from(cm), ce, false);
        }
        let (_, am, ae) = parts(a);
        let (_, bm, be) = parts(b);
        let pm = u128::from(am) * u128::from(bm); // 105 or 106 bits, exact
        let pe = ae + be;
        if c == 0.0 {
            return round_to(fmt, pneg, pm, pe, false);
        }
        let (_, cm, ce) = parts(c);
        let cm = u128::from(cm);
        let len = |m: u128| 128 - m.leading_zeros() as i32;
        let ptop = pe + len(pm);
        let ctop = ce + len(cm);
        // Order the terms by magnitude of their leading bit.
        let ((bneg, bm, be), (sneg, mut sm, mut se)) = if ptop >= ctop {
            ((pneg, pm, pe), (cneg, cm, ce))
        } else {
            ((cneg, cm, ce), (pneg, pm, pe))
        };
        if se + len(sm) <= be - 3 {
            // The small term lies wholly below the big one's last bit by
            // three places. Every rounding boundary of the result is a
            // multiple of 2^(be-2) (the result keeps at most `precision`
            // bits, and cancellation against so small a term costs at most
            // one), so no boundary lies strictly between big and big+small:
            // a nonzero unit at the same depth rounds identically.
            sm = 1;
            se = be - 3;
        }
        let base = be.min(se);
        // Both fit: the spread from `base` to the top is at most ~216 bits.
        let big = U256::new(bm).shl((be - base) as u32);
        let small = U256::new(sm).shl((se - base) as u32);
        let (neg, mag) = if bneg == sneg {
            (bneg, big.add(small))
        } else {
            match big.cmp(&small) {
                core::cmp::Ordering::Greater => (bneg, big.sub(small)),
                core::cmp::Ordering::Less => (sneg, small.sub(big)),
                // Exact cancellation: +0 under round to nearest.
                core::cmp::Ordering::Equal => return fmt.zero_bits(false),
            }
        };
        // Bring the exact sum into 128 bits, keeping a sticky bit; 120
        // significant bits are far more than any format's precision + 2.
        let bits = mag.bits();
        let drop = bits.saturating_sub(120);
        let (m, sticky) = mag.shr_sticky(drop);
        round_to(fmt, neg, m, base + drop as i32, sticky)
    }

    /// Fused multiply-add for `f64`.
    pub(crate) fn fma(a: f64, b: f64, c: f64) -> f64 {
        f64::from_bits(fma_core(F64, a, b, c))
    }

    /// Fused multiply-add for `f32`, rounded once into `f32`.
    pub(crate) fn fma_f32(a: f32, b: f32, c: f32) -> f32 {
        // Truncation is exact: the result bits are an f32 pattern.
        f32::from_bits(fma_core(F32, f64::from(a), f64::from(b), f64::from(c)) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_div_i64_to_f64_exact_and_large() {
        assert_eq!(div_i64_to_f64(7, 2), 3.5);
        assert_eq!(div_i64_to_f64(-7, 2), -3.5);
        assert_eq!(div_i64_to_f64(1, 3), 1.0 / 3.0);
        assert_eq!(div_i64_to_f64(i64::MIN, -1), 9_223_372_036_854_775_808.0);
        assert_eq!(div_i64_to_f64(0, -5).to_bits(), (-0.0f64).to_bits());
        // 2^53 + 1 over 1 rounds to even (2^53).
        assert_eq!(div_i64_to_f64((1 << 53) + 1, 1), 9007199254740992.0);
        assert_eq!(div_i64_to_f64((1 << 53) + 3, 1), 9007199254740996.0);
    }

    #[test]
    fn test_cmp_int_float_exact() {
        assert_eq!(cmp_int_float(1, 1.0), Some(Ordering::Equal));
        assert_eq!(cmp_int_float(1, 1.5), Some(Ordering::Less));
        assert_eq!(cmp_int_float(-1, -1.5), Some(Ordering::Greater));
        assert_eq!(
            cmp_int_float(i64::MAX, 9_223_372_036_854_775_808.0),
            Some(Ordering::Less)
        );
        assert_eq!(
            cmp_int_float(i64::MIN, -9_223_372_036_854_775_808.0),
            Some(Ordering::Equal)
        );
        assert_eq!(
            cmp_int_float((1 << 53) + 1, 9007199254740992.0),
            Some(Ordering::Greater)
        );
        assert_eq!(cmp_int_float(0, f64::NAN), None);
    }

    #[test]
    fn test_py_divmod_matches_cpython_examples() {
        let bits = |(q, m): (f64, f64)| (q.to_bits(), m.to_bits());
        assert_eq!(bits(py_divmod(-7.0, 2.0)), bits((-4.0, 1.0)));
        assert_eq!(py_divmod(7.0, -2.0).1, -1.0);
        assert_eq!(bits(py_divmod(-0.0, 1.0)).0, (-0.0f64).to_bits());
        assert_eq!(bits(py_divmod(0.0, -1.0)).1, (-0.0f64).to_bits());
        assert_eq!(py_divmod(1.0, f64::INFINITY).0, 0.0);
        assert_eq!(py_divmod(-1.0, f64::INFINITY).0, -1.0);
        assert_eq!(py_divmod(-1.0, f64::INFINITY).1, f64::INFINITY);
        assert!(py_divmod(f64::INFINITY, 1.0).0.is_nan());
        assert!(py_divmod(f64::INFINITY, 1.0).1.is_nan());
        assert!(py_divmod(1.0, 0.0).0.is_nan());
    }

    #[test]
    fn test_rem_ieee_examples() {
        assert_eq!(rem_ieee(5.0, 2.0), 1.0); // 5/2 = 2.5 -> 2 (even)
        assert_eq!(rem_ieee(7.0, 2.0), -1.0); // 3.5 -> 4
        assert_eq!(rem_ieee(-5.0, 2.0), -1.0);
        assert_eq!(rem_ieee(1.0, 3.0), 1.0);
        assert_eq!(rem_ieee(2.0, 3.0), -1.0);
        assert_eq!(rem_ieee(-0.0, 1.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(rem_ieee(4.0, 2.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(rem_ieee(-4.0, 2.0).to_bits(), (-0.0f64).to_bits());
        assert!(rem_ieee(f64::INFINITY, 2.0).is_nan());
        assert!(rem_ieee(1.0, 0.0).is_nan());
        assert_eq!(rem_ieee(1.0, f64::INFINITY), 1.0);
        // f64::MAX = (2^53 - 1) * 2^971 is 2 mod 3, so the nearest multiple of
        // 3 is one above it: the remainder is -1.
        assert_eq!(rem_ieee(f64::MAX, 3.0), -1.0);
    }

    #[test]
    fn test_min_max_signed_zero_and_nan() {
        assert_eq!(fmin(-0.0f64, 0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(fmin(0.0f64, -0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(fmax(-0.0f64, 0.0).to_bits(), 0.0f64.to_bits());
        assert!(fmin(f64::NAN, 1.0).is_nan());
        assert!(fmax(1.0f32, f32::NAN).is_nan());
    }

    #[test]
    fn test_soft_routines_on_edges() {
        let edges = [
            0.0,
            -0.0,
            0.5,
            -0.5,
            1.5,
            -1.5,
            2.5,
            -2.5,
            0.49999999999999994,
            4503599627370495.5,
            4503599627370497.0,
            1e300,
            -1e-300,
            5e-324,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
        ];
        for &x in &edges {
            assert_eq!(soft::trunc(x).to_bits(), x.trunc().to_bits(), "trunc {x}");
            assert_eq!(soft::floor(x).to_bits(), x.floor().to_bits(), "floor {x}");
            assert_eq!(soft::ceil(x).to_bits(), x.ceil().to_bits(), "ceil {x}");
            assert_eq!(soft::round(x).to_bits(), x.round().to_bits(), "round {x}");
            assert_eq!(
                soft::round_even(x).to_bits(),
                x.round_ties_even().to_bits(),
                "round_even {x}"
            );
            let s = soft::sqrt(x);
            let r = x.sqrt();
            assert!(
                s.to_bits() == r.to_bits() || (s.is_nan() && r.is_nan()),
                "sqrt {x}"
            );
        }
    }
}

#[cfg(test)]
mod props {
    use proptest::prelude::*;

    use super::*;

    fn any_f64() -> impl Strategy<Value = f64> {
        prop_oneof![
            any::<u64>().prop_map(f64::from_bits),
            (-1e6f64..1e6),
            (-4.0f64..4.0).prop_map(|x| (x * 2.0).round() / 2.0),
        ]
    }

    fn same(a: f64, b: f64) -> bool {
        (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20_000))]

        #[test]
        fn prop_soft_rounding_matches_std(x in any_f64()) {
            prop_assert!(same(soft::trunc(x), x.trunc()));
            prop_assert!(same(soft::floor(x), x.floor()));
            prop_assert!(same(soft::ceil(x), x.ceil()));
            prop_assert!(same(soft::round(x), x.round()));
            prop_assert!(same(soft::round_even(x), x.round_ties_even()));
            prop_assert!(same(soft::sqrt(x), x.sqrt()));
        }

        #[test]
        fn prop_soft_fma_matches_std(a in any_f64(), b in any_f64(), c in any_f64()) {
            prop_assert!(same(soft::fma(a, b, c), a.mul_add(b, c)), "{a:e} {b:e} {c:e}");
        }

        #[test]
        fn prop_soft_fma_near_cancellation(a in any::<u32>(), b in any::<u32>(), e in -60i32..60) {
            // Products that nearly cancel the addend: the hard case.
            let x = f64::from(a) * 2f64.powi(e);
            let y = f64::from(b) / 3.0;
            let c = -(x * y);
            prop_assert!(same(soft::fma(x, y, c), x.mul_add(y, c)));
        }

        #[test]
        fn prop_soft_fma_f32_matches_std(a in any::<u32>(), b in any::<u32>(), c in any::<u32>()) {
            let (a, b, c) = (f32::from_bits(a), f32::from_bits(b), f32::from_bits(c));
            let s = soft::fma_f32(a, b, c);
            let h = a.mul_add(b, c);
            prop_assert!((s.is_nan() && h.is_nan()) || s.to_bits() == h.to_bits(), "{a:e} {b:e} {c:e}");
        }

        #[test]
        fn prop_rem_ieee_on_integers_matches_exact_arithmetic(x in -(1i64 << 52)..(1i64 << 52), y in 1i64..(1i64 << 30), scale in -40i32..40) {
            // Exact reference: n = round-half-even(x / y) in integers.
            let q = x / y;
            let r = x % y;
            let twice = 2 * r.abs();
            let n = if twice > y || (twice == y && q % 2 != 0) { q + x.signum() } else { q };
            let want = (x - n * y) as f64 * 2f64.powi(scale);
            let got = rem_ieee(x as f64 * 2f64.powi(scale), y as f64 * 2f64.powi(scale));
            prop_assert!(same(got, want) || (got == 0.0 && want == 0.0), "{x} {y} {scale}: {got:e} vs {want:e}");
        }

        #[test]
        fn prop_quotient_is_correctly_rounded_for_exact_operands(a in -(1i64 << 53)..(1i64 << 53), b in -(1i64 << 53)..(1i64 << 53)) {
            prop_assume!(b != 0);
            // Both operands are exact in f64, so IEEE division is correctly
            // rounded: the two must agree.
            prop_assert!(same(div_i64_to_f64(a, b), a as f64 / b as f64));
        }

        #[test]
        fn prop_quotient_matches_long_division(a in any::<i64>(), b in any::<i64>()) {
            prop_assume!(b != 0);
            prop_assert!(same(div_i64_to_f64(a, b), long_division(a, b)), "{a}/{b}");
        }
    }
}

/// An independent correctly rounded quotient for the property above: binary
/// long division to 66 significant bits plus a sticky bit, then round to
/// nearest even by hand.
#[cfg(test)]
fn long_division(a: i64, b: i64) -> f64 {
    let neg = (a < 0) != (b < 0);
    let (n, d) = (u128::from(a.unsigned_abs()), u128::from(b.unsigned_abs()));
    if n == 0 {
        return if neg { -0.0 } else { 0.0 };
    }
    let mut q = n / d;
    let mut r = n % d;
    let mut exp = 0i32;
    while q < (1u128 << 66) {
        q <<= 1;
        r <<= 1;
        if r >= d {
            q |= 1;
            r -= d;
        }
        exp -= 1;
    }
    let len = 128 - q.leading_zeros() as i32;
    let drop = len - 53;
    let mut m = q >> drop;
    let rest = q & ((1u128 << drop) - 1);
    let half = 1u128 << (drop - 1);
    if rest > half || (rest == half && (r != 0 || m & 1 == 1)) {
        m += 1;
    }
    let v = (m as f64) * f64::from_bits(((exp + drop + 1023) as u64) << 52);
    if neg { -v } else { v }
}
