//! `ls_pow` transcribed from `_lexersketch/specs/ops-vectors/pow.md` (§2-§6)
//! for the reference interpreters, without reading the crate's `src/pow.rs`:
//! the spec's pseudocode, operation for operation, with `std`'s IEEE
//! `round_ties_even`/`trunc` and unbounded-enough `u128` integers. The
//! vectors test (`tests/pow_vectors.rs`) pins the crate to the spec's table;
//! this copy lets the differential tests derive expected `pow` results
//! independently.

#![allow(dead_code, clippy::many_single_char_names)]

fn f(bits: u64) -> f64 {
    f64::from_bits(bits)
}

const INV_LN2: u64 = 0x3FF7_1547_652B_82FE;
const LN2_HI: u64 = 0x3FE6_2E42_FEFA_3800;
const LN2_MID: u64 = 0x3D2E_F357_93C7_6730;
const LN2_LO: u64 = 0x398F_97B5_7A07_9A19;
const C6: (u64, u64) = (0x3FC5_5555_5555_5555, 0x3C65_5555_5555_5555);
const C24: (u64, u64) = (0x3FA5_5555_5555_5555, 0x3C45_5555_5555_5555);
const R120: u64 = 0x3F81_1111_1111_1111;
const R720: u64 = 0x3F56_C16C_16C1_6C17;
const R5040: u64 = 0x3F2A_01A0_1A01_A01A;
const R40320: u64 = 0x3EFA_01A0_1A01_A01A;
const SQRT2: u64 = 0x3FF6_A09E_667F_3BCD;
const LOG_C: [u64; 11] = [
    0x3FE5_5555_5555_5555,
    0x3FD9_9999_9999_999A,
    0x3FD2_4924_9249_2492,
    0x3FCC_71C7_1C71_C71C,
    0x3FC7_45D1_745D_1746,
    0x3FC3_B13B_13B1_3B14,
    0x3FC1_1111_1111_1111,
    0x3FBE_1E1E_1E1E_1E1E,
    0x3FBA_F286_BCA1_AF28,
    0x3FB8_6186_1861_8618,
    0x3FB6_42C8_590B_2164,
];
const TINY_Z: u64 = 0x3C30_0000_0000_0000;
const MIN_NORMAL: u64 = 0x0010_0000_0000_0000;
const NAN: u64 = 0x7FF8_0000_0000_0000;

type Dd = (f64, f64);

fn pow2(n: i64) -> f64 {
    f(((n + 1023) as u64) << 52)
}

fn two_sum(a: f64, b: f64) -> Dd {
    let s = a + b;
    let t = s - a;
    (s, (a - (s - t)) + (b - t))
}

fn fast_two_sum(a: f64, b: f64) -> Dd {
    let s = a + b;
    (s, b - (s - a))
}

fn split(a: f64) -> Dd {
    let t = 134_217_729.0 * a;
    let h = t - (t - a);
    (h, a - h)
}

fn two_prod(a: f64, b: f64) -> Dd {
    let p = a * b;
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    (p, ((ah * bh - p) + ah * bl + al * bh) + al * bl)
}

fn dd_add(a: Dd, b: Dd) -> Dd {
    let s = two_sum(a.0, b.0);
    let t = two_sum(a.1, b.1);
    let s = fast_two_sum(s.0, s.1 + t.0);
    fast_two_sum(s.0, s.1 + t.1)
}

fn dd_mul(a: Dd, b: Dd) -> Dd {
    let p = two_prod(a.0, b.0);
    fast_two_sum(p.0, p.1 + (a.0 * b.1 + a.1 * b.0))
}

fn is_int(y: f64) -> bool {
    y.is_finite() && y.trunc() == y
}

fn is_odd_int(y: f64) -> bool {
    is_int(y) && y.abs() < 9_007_199_254_740_992.0 && (y * 0.5).trunc() != y * 0.5
}

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

fn exp_core(z: Dd) -> (f64, Dd) {
    let n = (z.0 * f(INV_LN2)).round_ties_even();
    let a = z.0 - n * f(LN2_HI);
    let p1 = two_prod(n, f(LN2_MID));
    let r = dd_add((a, 0.0), (-p1.0, -p1.1));
    let r = dd_add(r, (z.1 - n * f(LN2_LO), 0.0));
    let s = (r.0 * 0.000_976_562_5, r.1 * 0.000_976_562_5);
    let q = (((f(R40320) * s.0 + f(R5040)) * s.0 + f(R720)) * s.0 + f(R120)) * s.0;
    let mut w = dd_add((f(C24.0), f(C24.1)), (q, 0.0));
    w = dd_add(dd_mul(w, s), (f(C6.0), f(C6.1)));
    w = dd_add(dd_mul(w, s), (0.5, 0.0));
    w = dd_add(dd_mul(w, s), (1.0, 0.0));
    let mut u = dd_mul(w, s);
    for _ in 0..10 {
        u = dd_add((2.0 * u.0, 2.0 * u.1), dd_mul(u, u));
    }
    let h = fast_two_sum(1.0, u.0);
    (n, fast_two_sum(h.0, h.1 + u.1))
}

fn log_dd(x: f64) -> Dd {
    let (mut x, mut k) = (x, 0i64);
    if x < f(MIN_NORMAL) {
        x *= 18_014_398_509_481_984.0;
        k = -54;
    }
    let mut e = ((x.to_bits() >> 52) & 0x7FF) as i64 - 1023;
    let mut m = f((x.to_bits() & 0x000F_FFFF_FFFF_FFFF) | 0x3FF0_0000_0000_0000);
    if m > f(SQRT2) {
        m *= 0.5;
        e += 1;
    }
    let kk = (k + e) as f64;
    let fm = m - 1.0;
    let t = fm / (2.0 + fm);
    let t2 = t * t;
    let mut p = f(LOG_C[10]);
    for i in (0..10).rev() {
        p = p * t2 + f(LOG_C[i]);
    }
    let y0 = t * 2.0 + (t * t2) * p;
    let (n, ev) = exp_core((-y0, 0.0));
    let sc = pow2(n as i64);
    let ev = (ev.0 * sc, ev.1 * sc);
    let pr = two_prod(m, ev.0);
    let w = two_sum(pr.0 - 1.0, pr.1 + m * ev.1);
    let corr = w.0 + (w.1 - 0.5 * w.0 * w.0);
    let lm = two_sum(y0, corr);
    let acc = dd_add((kk * f(LN2_HI), 0.0), two_prod(kk, f(LN2_MID)));
    let acc = dd_add(acc, lm);
    dd_add(acc, (kk * f(LN2_LO), 0.0))
}

fn round_dd_int(h: f64, l: f64) -> f64 {
    let k = h.round_ties_even();
    let d = (h - k) + l;
    let odd = (k * 0.5).round_ties_even() * 2.0 != k;
    if d > 0.5 || (d == 0.5 && odd) {
        k + 1.0
    } else if d < -0.5 || (d == -0.5 && odd) {
        k - 1.0
    } else {
        k
    }
}

fn finish(v: Dd, n: i64) -> f64 {
    if n > -1022 {
        return scale(v.0 + v.1, n);
    }
    if n + 1074 < -2 {
        return 0.0;
    }
    let sc = pow2(n + 1074);
    round_dd_int(v.0 * sc, v.1 * sc) * f(1)
}

fn exact_int_pow(x: f64, y: f64) -> Option<f64> {
    let big_e = (x.to_bits() >> 52) & 0x7FF;
    let mut m = u128::from(x.to_bits() & 0x000F_FFFF_FFFF_FFFF);
    let mut e: i64 = if big_e == 0 {
        1 - 1075
    } else {
        m |= 1 << 52;
        big_e as i64 - 1075
    };
    if m == 0 {
        return None;
    }
    let z = m.trailing_zeros();
    m >>= z;
    e += i64::from(z);
    let n = y as u32;
    if (128 - m.leading_zeros()) * n > 127 {
        return None;
    }
    let mut p: u128 = 1;
    for _ in 0..n {
        p *= m;
    }
    e *= i64::from(n);
    let len = i64::from(128 - p.leading_zeros());
    if e + len - 1 < -1022 {
        return None;
    }
    if len > 53 {
        let s = (len - 53) as u32;
        let mut q = p >> s;
        let r = p & ((1u128 << s) - 1);
        let h = 1u128 << (s - 1);
        if r > h || (r == h && q & 1 == 1) {
            q += 1;
        }
        p = q;
        e += i64::from(s);
    }
    let top = i64::from(127 - p.leading_zeros());
    if e + top > 1023 {
        return Some(f64::INFINITY);
    }
    Some(scale(p as f64 * pow2(-top), e + top))
}

/// `ls_pow(x, y)` (pow.md §4).
pub fn ls_pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return f(NAN);
    }
    let ax = x.abs();
    if y.is_infinite() {
        if ax == 1.0 {
            return 1.0;
        }
        return if (ax < 1.0) == (y > 0.0) {
            0.0
        } else {
            f64::INFINITY
        };
    }
    if x.is_infinite() {
        let odd = x < 0.0 && is_odd_int(y);
        if y < 0.0 {
            return if odd { -0.0 } else { 0.0 };
        }
        return if odd {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if x == 0.0 {
        if is_odd_int(y) {
            if y < 0.0 {
                return if x.is_sign_negative() {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                };
            }
            return x;
        }
        return if y < 0.0 { f64::INFINITY } else { 0.0 };
    }
    let mut sign = 1.0;
    if x < 0.0 {
        if !is_int(y) {
            return f(NAN);
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
    let l = log_dd(ax);
    let zh = y * l.0;
    if zh > 710.0 {
        return sign * f64::INFINITY;
    }
    if zh < -746.0 {
        return sign * 0.0;
    }
    if zh.abs() < f(TINY_Z) {
        return sign;
    }
    let p = two_prod(y, l.0);
    let z = fast_two_sum(p.0, p.1 + y * l.1);
    let (n, v) = exp_core(z);
    sign * finish(v, n as i64)
}

/// The `f64` nearest the exact power `base^exp` (`exp >= 0`; infinity past
/// `f64::MAX`), by schoolbook big-integer multiplication and a final
/// rounding of the top bits with a sticky bit.
pub fn exact_pow_f64(base: i64, exp: i64) -> f64 {
    let neg = base < 0 && exp % 2 == 1;
    let b = base.unsigned_abs();
    if b <= 1 || exp == 0 {
        let r = if exp == 0 { 1.0 } else { b as f64 };
        return if neg { -r } else { r };
    }
    // Little-endian base-2^32 limbs.
    let mut acc: Vec<u64> = vec![1];
    for _ in 0..exp {
        let mut carry: u128 = 0;
        for limb in &mut acc {
            let cur = u128::from(*limb) * u128::from(b) + carry;
            *limb = (cur & 0xFFFF_FFFF) as u64;
            carry = cur >> 32;
        }
        while carry != 0 {
            acc.push((carry & 0xFFFF_FFFF) as u64);
            carry >>= 32;
        }
        if acc.len() * 32 > 1200 {
            return if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
    }
    let bit = |i: usize| (acc[i / 32] >> (i % 32)) & 1 == 1;
    let top = acc.len() - 1;
    let len = top * 32 + (64 - acc[top].leading_zeros() as usize);
    let r = if len <= 64 {
        let mut v: u64 = 0;
        for i in (0..len).rev() {
            v = (v << 1) | u64::from(bit(i));
        }
        v as f64
    } else {
        // The top 64 bits, the lowest of them made sticky.
        let shift = len - 64;
        let mut v: u64 = 0;
        for i in (shift..len).rev() {
            v = (v << 1) | u64::from(bit(i));
        }
        if (0..shift).any(bit) {
            v |= 1;
        }
        let mut r = v as f64;
        for _ in 0..shift {
            r *= 2.0;
        }
        r
    };
    if neg { -r } else { r }
}
