//! `pow` (OPS v2): `ls_pow`, the family's shared float power routine, and
//! the exact integer power that `overflow = promote` rounds.
//!
//! **`ls_pow`** is specified operation by operation in
//! `_lexersketch/specs/ops-vectors/pow.md`, so every tier computes the same
//! bits (OPS §4 forbids a platform libm for `pow`, whose results differ
//! between libraries). The routine uses only IEEE 754 binary64 `+ - * /`
//! with round-to-nearest-even, comparisons, and integer arithmetic on bit
//! patterns, evaluated exactly as written (a compiler must not contract
//! `a * b + c` into an fma; Rust never does). Its error-free products use
//! Veltkamp/Dekker splitting rather than an fma: both give the exact
//! rounding error of a product in the ranges used here, so a tier with a
//! hardware fma may use it and still match, but no tier needs one.
//!
//! The structure is the textbook one, carried in double-double arithmetic
//! (about 106 bits): `x^y = e^(y ln x)`, with `ln x` relative to `x` (so `x`
//! near 1 keeps its precision) by one Newton step on a double approximation,
//! and `e^z` by reduction to `|r| <= ln2/2`, a scaling by 2^-10, a Taylor
//! series, and ten exact squarings of `e^s - 1`. The result before the final
//! rounding is within about 2^-85 of `x^y` relative, so `ls_pow` is correctly
//! rounded except when `x^y` lies that close to a rounding boundary: an exact
//! midpoint is the only such case met in practice, and integer exponents
//! (where every exact midpoint with an integer exponent arises) take an exact
//! integer path instead. A non-integer exponent can still produce an exact
//! midpoint (`(c^2) ** 1.5` with a 54-bit odd `c^3`); `ls_pow` may round
//! such a case either way, deterministically.

use crate::fmath;

const INV_LN2: f64 = f64::from_bits(0x3FF7_1547_652B_82FE);
/// `ln 2` in three parts: `LN2_HI` has 42 significant bits, so `n * LN2_HI`
/// is exact for every `|n| < 2^11`.
const LN2_HI: f64 = f64::from_bits(0x3FE6_2E42_FEFA_3800);
const LN2_MID: f64 = f64::from_bits(0x3D2E_F357_93C7_6730);
const LN2_LO: f64 = f64::from_bits(0x398F_97B5_7A07_9A19);
/// `1/6` and `1/24` as double-doubles.
const C6: Dd = Dd(
    f64::from_bits(0x3FC5_5555_5555_5555),
    f64::from_bits(0x3C65_5555_5555_5555),
);
const C24: Dd = Dd(
    f64::from_bits(0x3FA5_5555_5555_5555),
    f64::from_bits(0x3C45_5555_5555_5555),
);
/// `1/120`, `1/720`, `1/5040`, `1/40320`, rounded.
const R120: f64 = f64::from_bits(0x3F81_1111_1111_1111);
const R720: f64 = f64::from_bits(0x3F56_C16C_16C1_6C17);
const R5040: f64 = f64::from_bits(0x3F2A_01A0_1A01_A01A);
const R40320: f64 = f64::from_bits(0x3EFA_01A0_1A01_A01A);
/// `sqrt(2)`, rounded: mantissas above it are halved.
const SQRT2: f64 = f64::from_bits(0x3FF6_A09E_667F_3BCD);
/// `2/3, 2/5, ..., 2/23`: the `atanh` series of `ln`, for the first guess.
const LOG_C: [f64; 11] = [
    f64::from_bits(0x3FE5_5555_5555_5555),
    f64::from_bits(0x3FD9_9999_9999_999A),
    f64::from_bits(0x3FD2_4924_9249_2492),
    f64::from_bits(0x3FCC_71C7_1C71_C71C),
    f64::from_bits(0x3FC7_45D1_745D_1746),
    f64::from_bits(0x3FC3_B13B_13B1_3B14),
    f64::from_bits(0x3FC1_1111_1111_1111),
    f64::from_bits(0x3FBE_1E1E_1E1E_1E1E),
    f64::from_bits(0x3FBA_F286_BCA1_AF28),
    f64::from_bits(0x3FB8_6186_1861_8618),
    f64::from_bits(0x3FB6_42C8_590B_2164),
];
/// Veltkamp's splitting constant, `2^27 + 1`.
const SPLIT: f64 = 134_217_729.0;
const TWO_POW_54: f64 = 18_014_398_509_481_984.0;
/// `2^-60`: below it `e^z` rounds to 1.
const TINY_Z: f64 = f64::from_bits(0x3C30_0000_0000_0000);
const MIN_NORMAL: f64 = f64::from_bits(0x0010_0000_0000_0000);
const SIGN: u64 = 1 << 63;
/// The canonical quiet NaN `ls_pow` returns.
const NAN: f64 = f64::from_bits(0x7FF8_0000_0000_0000);

/// A double-double: the unevaluated sum `hi + lo`, `|lo| <= ulp(hi) / 2`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Dd(f64, f64);

/// Knuth's error-free sum.
#[inline]
fn two_sum(a: f64, b: f64) -> Dd {
    let s = a + b;
    let bb = s - a;
    Dd(s, (a - (s - bb)) + (b - bb))
}

/// Dekker's error-free sum, for `|a| >= |b|` (or `a = 0`).
#[inline]
fn fast_two_sum(a: f64, b: f64) -> Dd {
    let s = a + b;
    Dd(s, b - (s - a))
}

/// Veltkamp's split into two 26-bit halves.
#[inline]
fn split(a: f64) -> (f64, f64) {
    let t = SPLIT * a;
    let hi = t - (t - a);
    (hi, a - hi)
}

/// Dekker's error-free product: `p + e = a * b` exactly (the operands here
/// never overflow the split or underflow the error).
#[inline]
fn two_prod(a: f64, b: f64) -> Dd {
    let p = a * b;
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    Dd(p, ((ah * bh - p) + ah * bl + al * bh) + al * bl)
}

#[inline]
fn dd_add(a: Dd, b: Dd) -> Dd {
    let s = two_sum(a.0, b.0);
    let t = two_sum(a.1, b.1);
    let s = fast_two_sum(s.0, s.1 + t.0);
    fast_two_sum(s.0, s.1 + t.1)
}

#[inline]
fn dd_mul(a: Dd, b: Dd) -> Dd {
    let p = two_prod(a.0, b.0);
    fast_two_sum(p.0, p.1 + (a.0 * b.1 + a.1 * b.0))
}

/// `e^z` for a double-double `|z| <= 746`: `(n, v)` with `e^z = v * 2^n`
/// and `v` a double-double in about `[0.7, 1.42]`.
fn exp_core(z: Dd) -> (f64, Dd) {
    let n = fmath::round_even(z.0 * INV_LN2);
    let a = z.0 - n * LN2_HI;
    let p1 = two_prod(n, LN2_MID);
    let r = dd_add(Dd(a, 0.0), Dd(-p1.0, -p1.1));
    let r = dd_add(r, Dd(z.1 - n * LN2_LO, 0.0));
    let s = Dd(r.0 * 0.000_976_562_5, r.1 * 0.000_976_562_5);
    let q = (((R40320 * s.0 + R5040) * s.0 + R720) * s.0 + R120) * s.0;
    let w = dd_add(C24, Dd(q, 0.0));
    let w = dd_add(dd_mul(w, s), C6);
    let w = dd_add(dd_mul(w, s), Dd(0.5, 0.0));
    let w = dd_add(dd_mul(w, s), Dd(1.0, 0.0));
    let mut u = dd_mul(w, s);
    // e^(2s) - 1 = 2u + u^2: ten squarings undo the scaling by 2^-10
    // without ever adding 1, so u keeps its relative precision.
    for _ in 0..10 {
        u = dd_add(Dd(2.0 * u.0, 2.0 * u.1), dd_mul(u, u));
    }
    let h = fast_two_sum(1.0, u.0);
    (n, fast_two_sum(h.0, h.1 + u.1))
}

/// `ln x` as a double-double accurate relative to `ln x`, for finite
/// `x > 0`.
fn log_dd(x: f64) -> Dd {
    let (mut x, mut k) = (x, 0i64);
    if x < MIN_NORMAL {
        x *= TWO_POW_54;
        k = -54;
    }
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7FF) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000F_FFFF_FFFF_FFFF) | 0x3FF0_0000_0000_0000);
    if m > SQRT2 {
        m *= 0.5;
        e += 1;
    }
    let k = (k + e) as f64;
    // A first guess from the atanh series in plain doubles (relative error
    // about 2^-52), then one Newton step through e^(-y0) in double-double.
    let fm = m - 1.0;
    let t = fm / (2.0 + fm);
    let t2 = t * t;
    let mut p = LOG_C[10];
    for &c in LOG_C[..10].iter().rev() {
        p = p * t2 + c;
    }
    let y0 = t * 2.0 + (t * t2) * p;
    // |y0| is about ln(sqrt 2) at most, so exp_core's n is 0 or, for a
    // mantissa at the rounded sqrt(2) itself, possibly -1; the scaling by
    // 2^n is exact either way.
    let (n, ev) = exp_core(Dd(-y0, 0.0));
    let sc = pow2(n as i64);
    let ev = Dd(ev.0 * sc, ev.1 * sc);
    let pr = two_prod(m, ev.0);
    let w = two_sum(pr.0 - 1.0, pr.1 + m * ev.1);
    // ln(1 + w) = w - w^2/2 + O(w^3), and |w| is about 2^-52.
    let corr = w.0 + (w.1 - 0.5 * w.0 * w.0);
    let lm = two_sum(y0, corr);
    let p2 = two_prod(k, LN2_MID);
    let acc = dd_add(Dd(k * LN2_HI, 0.0), p2);
    let acc = dd_add(acc, lm);
    dd_add(acc, Dd(k * LN2_LO, 0.0))
}

/// `2^n` for `-1022 <= n <= 1023`.
#[inline]
fn pow2(n: i64) -> f64 {
    f64::from_bits(((n + 1023) as u64) << 52)
}

/// `v * 2^n` for `v` in about `[0.5, 2]`, with one rounding (the scalings by
/// `2^±1000` are exact).
fn scale(v: f64, n: i64) -> f64 {
    let (mut v, mut n) = (v, n);
    if n > 1000 {
        v *= pow2(1000);
        n -= 1000;
    }
    if n < -1000 {
        v *= pow2(-1000);
        n += 1000;
    }
    v * pow2(n)
}

/// The integer nearest `h + l` (a double-double, `0 <= h <= 2^53`), ties to
/// even.
fn round_dd_int(h: f64, l: f64) -> f64 {
    let k = fmath::round_even(h);
    let d = (h - k) + l;
    let odd = fmath::round_even(k * 0.5) * 2.0 != k;
    if d > 0.5 || (d == 0.5 && odd) {
        k + 1.0
    } else if d < -0.5 || (d == -0.5 && odd) {
        k - 1.0
    } else {
        k
    }
}

/// `v * 2^n` rounded once, for a double-double `v` from [`exp_core`]: a
/// subnormal result is rounded directly to a multiple of 2^-1074, so it is
/// not rounded twice.
fn finish(v: Dd, n: i64) -> f64 {
    // From n = -1022 down, the unit in the last place is 2^-1074 whatever v
    // is, so rounding to a multiple of it is the one correct rounding.
    if n > -1022 {
        return scale(v.0 + v.1, n);
    }
    if n + 1074 < -2 {
        return 0.0;
    }
    let sc = pow2(n + 1074);
    let k = round_dd_int(v.0 * sc, v.1 * sc);
    k * f64::from_bits(1)
}

/// Whether `y` is a finite integer.
#[inline]
fn is_int(y: f64) -> bool {
    y.is_finite() && fmath::trunc(y) == y
}

/// Whether `y` is an odd integer (every float at or above 2^53 is even).
#[inline]
fn is_odd_int(y: f64) -> bool {
    is_int(y) && f64::from_bits(y.to_bits() & !SIGN) < 9_007_199_254_740_992.0 && {
        let h = y * 0.5;
        fmath::trunc(h) != h
    }
}

/// `x^y` exactly in integers, for finite `x > 0` and an integer `y` in
/// `1..=127` whose power of `x`'s odd significand fits 127 bits, rounded
/// once; `None` otherwise or for a subnormal result. Catches every exact
/// midpoint an integer exponent can produce.
fn exact_int_pow(x: f64, y: f64) -> Option<f64> {
    let bits = x.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i64;
    let mut mant = u128::from(bits & 0x000F_FFFF_FFFF_FFFF);
    let mut ex = if biased == 0 {
        1
    } else {
        mant |= 1 << 52;
        biased
    } - 1075;
    if mant == 0 {
        return None;
    }
    let tz = mant.trailing_zeros();
    mant >>= tz;
    ex += i64::from(tz);
    let n = y as u32;
    let len = 128 - mant.leading_zeros();
    if len.saturating_mul(n) > 127 {
        return None;
    }
    let mut p: u128 = 1;
    for _ in 0..n {
        p *= mant;
    }
    let mut e = ex * i64::from(n);
    let length = i64::from(128 - p.leading_zeros());
    if e + length - 1 < -1022 {
        return None;
    }
    if length > 53 {
        let sh = (length - 53) as u32;
        let mut q = p >> sh;
        let rem = p & ((1u128 << sh) - 1);
        let half = 1u128 << (sh - 1);
        if rem > half || (rem == half && q & 1 == 1) {
            q += 1;
        }
        p = q;
        e += i64::from(sh);
    }
    let top = i64::from(127 - p.leading_zeros());
    if e + top > 1023 {
        return Some(f64::INFINITY);
    }
    // p <= 2^53, so it converts exactly; scaled into [1, 2] for `scale`.
    Some(scale(p as f64 * pow2(-top), e + top))
}

/// `ls_pow(x, y)`: OPS v2 float `pow`, C99 Annex F special cases first.
pub(crate) fn ls_pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return NAN;
    }
    let ax = f64::from_bits(x.to_bits() & !SIGN);
    if y.is_infinite() {
        return if ax == 1.0 {
            1.0
        } else if (ax < 1.0) == (y > 0.0) {
            0.0
        } else {
            f64::INFINITY
        };
    }
    if x.is_infinite() {
        let odd = x < 0.0 && is_odd_int(y);
        return match (y < 0.0, odd) {
            (true, true) => -0.0,
            (true, false) => 0.0,
            (false, true) => f64::NEG_INFINITY,
            (false, false) => f64::INFINITY,
        };
    }
    if x == 0.0 {
        let neg = x.to_bits() & SIGN != 0;
        if is_odd_int(y) {
            return match (y < 0.0, neg) {
                (true, true) => f64::NEG_INFINITY,
                (true, false) => f64::INFINITY,
                (false, _) => x,
            };
        }
        return if y < 0.0 { f64::INFINITY } else { 0.0 };
    }
    let mut sign = 1.0;
    if x < 0.0 {
        if !is_int(y) {
            return NAN;
        }
        if is_odd_int(y) {
            sign = -1.0;
        }
    }
    if is_int(y) && (1.0..=127.0).contains(&y) {
        if let Some(r) = exact_int_pow(ax, y) {
            return sign * r;
        }
    }
    let lg = log_dd(ax);
    let zh = y * lg.0;
    if zh > 710.0 {
        return sign * f64::INFINITY;
    }
    if zh < -746.0 {
        return sign * 0.0;
    }
    if f64::from_bits(zh.to_bits() & !SIGN) < TINY_Z {
        return sign;
    }
    let p = two_prod(y, lg.0);
    let z = fast_two_sum(p.0, p.1 + y * lg.1);
    let (n, v) = exp_core(z);
    sign * finish(v, n as i64)
}

/// `fpow` at `f32` (LSB §5.3): `ls_pow` on the operands widened to `f64`,
/// rounded to `f32` once.
pub(crate) fn ls_pow_f32(x: f32, y: f32) -> f32 {
    ls_pow(f64::from(x), f64::from(y)) as f32
}

/// The result of an integer `pow` under `overflow = promote` (OPS v2): the
/// exact power as an `i64` when it fits, else the `f64` nearest the exact
/// power (ties to even; infinity beyond `f64::MAX`), and for a negative
/// exponent the float rule, `ls_pow` of the operands converted to `f64`.
pub(crate) fn pow_promote(base: i64, exp: i64) -> Result<i64, f64> {
    if exp < 0 {
        return Err(ls_pow(base as f64, exp as f64));
    }
    let (r, overflowed) = crate::int::pow_exact(base, exp as u64);
    if !overflowed {
        return Ok(r);
    }
    // |base| >= 2 and exp >= 2 here. Every power with more than 1100 bits
    // is beyond f64::MAX, so the exact power is needed only up to there.
    let negative = base < 0 && exp & 1 == 1;
    let magnitude = big_pow_to_f64(base.unsigned_abs(), exp as u64);
    Err(if negative { -magnitude } else { magnitude })
}

/// Little-endian limbs of an exact power, capped at 1100 bits.
const LIMBS: usize = 19;

/// `b^e` (`b >= 2`) rounded to the nearest `f64`, or infinity.
fn big_pow_to_f64(b: u64, e: u64) -> f64 {
    // A loose lower bound on the bit length: (bits(b) - 1) * e >= 1100 means
    // the power is above 2^1024.
    let bits = u64::from(64 - b.leading_zeros());
    if (bits - 1).saturating_mul(e) >= 1100 {
        return f64::INFINITY;
    }
    let mut acc = [0u64; LIMBS];
    acc[0] = 1;
    let mut sq = [0u64; LIMBS];
    sq[0] = b;
    let mut e = e;
    loop {
        if e & 1 == 1 && !big_mul(&mut acc, &sq) {
            return f64::INFINITY;
        }
        e >>= 1;
        if e == 0 {
            break;
        }
        let copy = sq;
        if !big_mul(&mut sq, &copy) {
            // The square is past the cap and a later bit multiplies it in.
            return f64::INFINITY;
        }
    }
    big_to_f64(&acc)
}

/// `a *= b` in place; `false` when the product needs more than [`LIMBS`]
/// limbs.
fn big_mul(a: &mut [u64; LIMBS], b: &[u64; LIMBS]) -> bool {
    let mut out = [0u64; LIMBS];
    for (i, &x) in a.iter().enumerate() {
        if x == 0 {
            continue;
        }
        let mut carry: u128 = 0;
        for (j, &y) in b.iter().enumerate() {
            let k = i + j;
            let cur = u128::from(x) * u128::from(y) + carry;
            if k >= LIMBS {
                if cur != 0 {
                    return false;
                }
                continue;
            }
            let sum = u128::from(out[k]) + (cur & u128::from(u64::MAX));
            out[k] = sum as u64;
            carry = (cur >> 64) + (sum >> 64);
        }
        let mut k = i + LIMBS;
        while carry != 0 {
            if k >= LIMBS {
                return false;
            }
            let sum = u128::from(out[k]) + carry;
            out[k] = sum as u64;
            carry = sum >> 64;
            k += 1;
        }
    }
    *a = out;
    true
}

/// A big unsigned integer rounded to the nearest `f64` (ties to even).
fn big_to_f64(a: &[u64; LIMBS]) -> f64 {
    let Some(top) = a.iter().rposition(|&w| w != 0) else {
        return 0.0;
    };
    let len = top as i32 * 64 + (64 - a[top].leading_zeros() as i32);
    if len <= 128 {
        // Converting a u128 rounds to nearest even.
        return ((u128::from(a[1]) << 64) | u128::from(a[0])) as f64;
    }
    // The top 66 bits (two guard bits) and a sticky bit for the rest.
    let shift = len - 66;
    let mut m: u128 = 0;
    for i in 0..66 {
        let bit = shift + i;
        let w = a[(bit / 64) as usize];
        m |= u128::from((w >> (bit % 64)) & 1) << i;
    }
    let sticky = (0..shift).any(|bit| (a[(bit / 64) as usize] >> (bit % 64)) & 1 == 1);
    f64::from_bits(fmath::round_to(fmath::F64, false, m, shift, sticky))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_cases_are_exact() {
        assert_eq!(ls_pow(2.0, 10.0), 1024.0);
        assert_eq!(ls_pow(10.0, 22.0), 1e22);
        assert_eq!(ls_pow(3.0, 20.0), 3_486_784_401.0);
        assert_eq!(ls_pow(4.0, 0.5), 2.0);
        assert_eq!(ls_pow(2.0, -1.0), 0.5);
        assert_eq!(ls_pow(2.0, -1074.0), f64::from_bits(1));
        assert_eq!(ls_pow(2.0, 1023.0), f64::from_bits(0x7FE0_0000_0000_0000));
        assert_eq!(ls_pow(2.0, 1024.0), f64::INFINITY);
        assert_eq!(ls_pow(-2.0, 3.0), -8.0);
        // Exact midpoints round to even (481^6 and 5^23 have 54-bit odd
        // significands).
        assert_eq!(ls_pow(481.0, 6.0), 12_384_271_322_498_880.0);
        assert_eq!(ls_pow(5.0, 23.0), 11_920_928_955_078_124.0);
    }

    #[test]
    fn test_annex_f_special_cases() {
        let inf = f64::INFINITY;
        assert_eq!(ls_pow(f64::NAN, 0.0), 1.0);
        assert_eq!(ls_pow(1.0, f64::NAN), 1.0);
        assert_eq!(ls_pow(f64::NAN, 1.0).to_bits(), NAN.to_bits());
        assert_eq!(ls_pow(-1.0, inf), 1.0);
        assert_eq!(ls_pow(-1.0, -inf), 1.0);
        assert_eq!(ls_pow(0.5, inf), 0.0);
        assert_eq!(ls_pow(0.5, -inf), inf);
        assert_eq!(ls_pow(2.0, -inf), 0.0);
        assert_eq!(ls_pow(-inf, -3.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(ls_pow(-inf, 3.0), -inf);
        assert_eq!(ls_pow(-inf, 2.0), inf);
        assert_eq!(ls_pow(-0.0, -3.0), -inf);
        assert_eq!(ls_pow(-0.0, 3.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(ls_pow(-0.0, 2.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(ls_pow(0.0, -0.5), inf);
        assert!(ls_pow(-8.0, 1.0 / 3.0).is_nan());
        assert_eq!(ls_pow(-1.0, 1e300), 1.0); // every huge float is even
    }

    #[test]
    #[cfg(feature = "std")]
    fn test_close_to_the_platform_pow() {
        // Not the oracle (the spec's vectors and the decimal check are), but
        // a gross error would show here.
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let x = (s >> 11) as f64 / (1u64 << 53) as f64 * 64.0;
            let y = ((s & 0xFFFF) as f64 - 32_768.0) / 256.0;
            let (a, b) = (ls_pow(x, y), x.powf(y));
            if a.is_finite() && b.is_finite() && a != 0.0 {
                let ulps = (a.to_bits() as i64 - b.to_bits() as i64).abs();
                assert!(ulps <= 1, "{x} ** {y}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn test_promote_rounds_the_exact_power() {
        assert_eq!(pow_promote(2, 62), Ok(1 << 62));
        assert_eq!(pow_promote(2, 64), Err(18_446_744_073_709_551_616.0));
        assert_eq!(pow_promote(-2, 63), Ok(i64::MIN));
        assert_eq!(pow_promote(-2, 65), Err(-36_893_488_147_419_103_232.0));
        assert_eq!(pow_promote(2, -1), Err(0.5));
        assert_eq!(pow_promote(10, 400), Err(f64::INFINITY));
        assert_eq!(pow_promote(3, 1000), Err(f64::INFINITY));
        // 3^40 = 12157665459056928801, nearest double 12157665459056928768.
        assert_eq!(pow_promote(3, 40), Err(12_157_665_459_056_928_801.0));
        // 7^23 has 65 bits; its nearest double (checked by hand with exact
        // integers in u128).
        let exact: u128 = 7u128.pow(23);
        assert_eq!(pow_promote(7, 23), Err(exact as f64));
        // i64::MIN ** 2 = 2^126.
        assert_eq!(
            pow_promote(i64::MIN, 2),
            Err(f64::from_bits((126 + 1023) << 52))
        );
    }
}
