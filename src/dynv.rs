//! The 64-bit slot encoding of `dyn`, `str`, and `ref` registers.
//!
//! LSB requires one representation for heap references across `str`, `ref`,
//! and `dyn` registers, with `nil` as the null reference (LSB §2.1), so that
//! `to_dyn` of a reference and `cast` from `dyn` move bits unchanged. This
//! module is that representation. It is a NaN-box with a float offset (the
//! layout JavaScriptCore uses), chosen so that **the all-zero word is `nil`**:
//! every LSB default (`false`, `0`, `+0.0`, `U+0000`, `nil`) is then the zero
//! word, and a new frame or struct is initialised by zero-filling it.
//!
//! ```text
//! 0                              nil
//! [1, 2^48)                      immediates: tag in bits 32..48
//!                                  1 = bool (payload 0/1), 2 = char (scalar)
//! [2^48, 2^49)                   heap reference: generation in bits 32..48,
//!                                  slot index in bits 0..32
//! [2^49, 0xFFFE_0000_0000_0000)  float: f64 bits + 2^49 (NaN canonicalised)
//! [0xFFFE_0000_0000_0000, 2^64)  int in [-2^48, 2^48), sign-extended 49 bits
//! ```
//!
//! A `dyn` int is 64-bit (LSB §2.2); one outside the inline range lives in a
//! boxed heap object, and an int inside the range is *always* inline, so two
//! equal ints have equal encodings unless both are boxed.
//!
//! Decoding is total: every 64-bit word decodes to some value. Non-canonical
//! words can only appear through modules a verifier would reject (an integer
//! instruction writing a `dyn` register, say); they decode deterministically
//! and never break memory safety.

/// The null reference, the default of every reference register.
pub(crate) const NIL: u64 = 0;
const TAG_SHIFT: u32 = 32;
const TAG_BOOL: u64 = 1 << TAG_SHIFT;
const TAG_CHAR: u64 = 2 << TAG_SHIFT;
/// First word of the reference range.
pub(crate) const REF_BASE: u64 = 1 << 48;
/// First word of the float range, and the offset added to float bits.
const FLOAT_OFFSET: u64 = 1 << 49;
/// First word of the inline-int range.
const INT_BASE: u64 = 0xFFFE_0000_0000_0000;
const INT_PAYLOAD: u64 = (1 << 49) - 1;
/// Smallest int stored inline.
pub(crate) const INLINE_MIN: i64 = -(1 << 48);
/// Largest int stored inline.
pub(crate) const INLINE_MAX: i64 = (1 << 48) - 1;
/// The one NaN a `dyn` float holds (OPS §4: stores canonicalise NaNs).
pub(crate) const CANONICAL_NAN: u64 = 0x7FF8_0000_0000_0000;

/// The decoded form of a slot, before heap objects are looked up.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Raw {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A scalar value, or (from a non-canonical word) any `u32`.
    Char(u32),
    /// A heap reference: slot index and generation.
    Ref(u32, u16),
}

/// Encodes a boolean.
#[inline]
pub(crate) const fn from_bool(b: bool) -> u64 {
    TAG_BOOL | b as u64
}

/// Encodes a character (its scalar value).
#[inline]
pub(crate) const fn from_char(c: u32) -> u64 {
    TAG_CHAR | c as u64
}

/// Encodes a float, canonicalising NaN.
#[inline]
pub(crate) fn from_f64(f: f64) -> u64 {
    let bits = if f.is_nan() {
        CANONICAL_NAN
    } else {
        f.to_bits()
    };
    // The largest non-NaN pattern (-inf, 0xFFF0...) plus the offset stays
    // below INT_BASE, so this never overflows into the int range.
    bits + FLOAT_OFFSET
}

/// Encodes an int when it fits inline.
#[inline]
pub(crate) const fn inline_int(i: i64) -> Option<u64> {
    if i >= INLINE_MIN && i <= INLINE_MAX {
        Some(INT_BASE | (i as u64 & INT_PAYLOAD))
    } else {
        None
    }
}

/// Encodes a heap reference.
#[inline]
pub(crate) const fn from_ref(index: u32, generation: u16) -> u64 {
    REF_BASE | ((generation as u64) << 32) | index as u64
}

/// Whether the word is an inline int.
#[inline]
pub(crate) const fn is_inline_int(v: u64) -> bool {
    v >= INT_BASE
}

/// The inline int of a word known to be one.
#[inline]
pub(crate) const fn inline_int_value(v: u64) -> i64 {
    ((v << 15) as i64) >> 15
}

/// Whether the word is a float.
#[inline]
pub(crate) const fn is_float(v: u64) -> bool {
    v >= FLOAT_OFFSET && v < INT_BASE
}

/// The float of a word known to be one.
#[inline]
pub(crate) fn float_value(v: u64) -> f64 {
    f64::from_bits(v - FLOAT_OFFSET)
}

/// Whether the word is a heap reference (live or not).
#[inline]
pub(crate) const fn is_ref(v: u64) -> bool {
    v >= REF_BASE && v < FLOAT_OFFSET
}

/// The slot index and generation of a reference word.
#[inline]
pub(crate) const fn ref_parts(v: u64) -> (u32, u16) {
    (v as u32, (v >> 32) as u16)
}

/// Decodes any word.
#[inline]
pub(crate) fn decode(v: u64) -> Raw {
    if v >= INT_BASE {
        Raw::Int(inline_int_value(v))
    } else if v >= FLOAT_OFFSET {
        Raw::Float(float_value(v))
    } else if v >= REF_BASE {
        let (i, g) = ref_parts(v);
        Raw::Ref(i, g)
    } else {
        match v >> TAG_SHIFT {
            1 => Raw::Bool(v as u32 != 0),
            2 => Raw::Char(v as u32),
            // 0 is nil; any other tag is a non-canonical word, read as nil.
            _ => Raw::Nil,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_word_is_nil() {
        assert_eq!(decode(0), Raw::Nil);
    }

    #[test]
    fn test_inline_int_round_trip_at_bounds() {
        for i in [0, 1, -1, INLINE_MIN, INLINE_MAX, 12345, -98765] {
            let v = inline_int(i).unwrap_or(0);
            assert!(is_inline_int(v));
            assert_eq!(decode(v), Raw::Int(i));
        }
        assert_eq!(inline_int(INLINE_MAX + 1), None);
        assert_eq!(inline_int(INLINE_MIN - 1), None);
        assert_eq!(inline_int(i64::MIN), None);
    }

    #[test]
    fn test_float_round_trip_keeps_signed_zero_and_infinities() {
        for f in [
            0.0,
            -0.0,
            1.5,
            -2.25,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            5e-324,
            f64::MAX,
            f64::MIN,
        ] {
            let v = from_f64(f);
            assert!(is_float(v));
            match decode(v) {
                Raw::Float(g) => assert_eq!(g.to_bits(), f.to_bits()),
                other => panic!("decoded {other:?}"),
            }
        }
    }

    #[test]
    fn test_nan_is_canonicalised() {
        let weird = f64::from_bits(0xFFF8_0000_0000_1234);
        let v = from_f64(weird);
        assert_eq!(v, from_f64(f64::NAN));
        assert!(matches!(decode(v), Raw::Float(f) if f.is_nan()));
    }

    #[test]
    fn test_ranges_do_not_overlap() {
        let neg_inf = from_f64(f64::NEG_INFINITY);
        assert!(neg_inf < INT_BASE);
        assert!(from_f64(0.0) >= FLOAT_OFFSET);
        let r = from_ref(u32::MAX, u16::MAX);
        assert!(is_ref(r));
        assert_eq!(ref_parts(r), (u32::MAX, u16::MAX));
        assert!(from_char(0x10FFFF) < REF_BASE);
        assert_eq!(decode(from_bool(true)), Raw::Bool(true));
        assert_eq!(decode(from_char(0x41)), Raw::Char(0x41));
    }

    #[test]
    fn test_every_word_decodes() {
        for v in [1u64, 3 << 32, (1 << 48) - 1, u64::MAX, INT_BASE - 1] {
            let _ = decode(v);
        }
    }
}
