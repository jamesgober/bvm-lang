//! Keyed hashing for map keys.
//!
//! Untrusted programs choose map keys, so a fixed, public hash function would
//! let them force every key into one probe chain and turn each map operation
//! linear (HashDoS). Byte strings are hashed with SipHash-1-3 under a per-VM
//! key; integers, floats, and identities, which are far more frequent and
//! cheap to compare, go through a keyed 64-bit mixer. With the `std` feature
//! the key is random per VM; without it the key is fixed (documented), since
//! `core` has no entropy source.
//!
//! Map iteration order is insertion order and never depends on hashes, so the
//! key changes nothing a program can observe except timing.

/// A per-VM hashing key.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seed {
    k0: u64,
    k1: u64,
}

impl Seed {
    /// A fresh key: random with `std`, fixed without.
    pub(crate) fn new() -> Seed {
        #[cfg(feature = "std")]
        {
            use std::hash::{BuildHasher, Hasher};
            let state = std::collections::hash_map::RandomState::new();
            let mut a = state.build_hasher();
            a.write_u64(0x5EED_0001);
            let mut b = state.build_hasher();
            b.write_u64(0x5EED_0002);
            Seed {
                k0: a.finish(),
                k1: b.finish(),
            }
        }
        #[cfg(not(feature = "std"))]
        {
            Seed {
                k0: 0x0706_0504_0302_0100,
                k1: 0x0F0E_0D0C_0B0A_0908,
            }
        }
    }

    /// Hashes a 64-bit word (ints, float bits, identities).
    #[inline]
    pub(crate) fn word(self, v: u64) -> u64 {
        // A bijective mix of the keyed word; every input bit reaches every
        // output bit, so neither low- nor high-bit patterns cluster.
        let mut x = v ^ self.k0;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (x ^ (x >> 31)) ^ self.k1
    }

    /// Hashes a byte string with SipHash-1-3.
    #[inline]
    pub(crate) fn bytes(self, data: &[u8]) -> u64 {
        sip(self.k0, self.k1, data, 1, 3)
    }
}

/// SipHash-c-d over `data` (the reference algorithm, rounds as parameters so
/// the tests can check it against SipHash-2-4).
fn sip(k0: u64, k1: u64, data: &[u8], c: u32, d: u32) -> u64 {
    let mut v0 = k0 ^ 0x736f_6d65_7073_6575;
    let mut v1 = k1 ^ 0x646f_7261_6e64_6f6d;
    let mut v2 = k0 ^ 0x6c79_6765_6e65_7261;
    let mut v3 = k1 ^ 0x7465_6462_7974_6573;

    macro_rules! round {
        () => {
            v0 = v0.wrapping_add(v1);
            v1 = v1.rotate_left(13);
            v1 ^= v0;
            v0 = v0.rotate_left(32);
            v2 = v2.wrapping_add(v3);
            v3 = v3.rotate_left(16);
            v3 ^= v2;
            v0 = v0.wrapping_add(v3);
            v3 = v3.rotate_left(21);
            v3 ^= v0;
            v2 = v2.wrapping_add(v1);
            v1 = v1.rotate_left(17);
            v1 ^= v2;
            v2 = v2.rotate_left(32);
        };
    }

    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        let m = u64::from_le_bytes(word);
        v3 ^= m;
        for _ in 0..c {
            round!();
        }
        v0 ^= m;
    }
    let rest = chunks.remainder();
    let mut last = [0u8; 8];
    last[..rest.len()].copy_from_slice(rest);
    let b = u64::from_le_bytes(last) | ((data.len() as u64) << 56);
    v3 ^= b;
    for _ in 0..c {
        round!();
    }
    v0 ^= b;
    v2 ^= 0xff;
    for _ in 0..d {
        round!();
    }
    v0 ^ v1 ^ v2 ^ v3
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn test_hashes_are_deterministic_per_seed() {
        let s = Seed::new();
        assert_eq!(s.bytes(b"hello"), s.bytes(b"hello"));
        assert_ne!(s.bytes(b"hello"), s.bytes(b"hellp"));
        assert_ne!(s.bytes(b""), s.bytes(b"\0"));
        assert_eq!(s.word(42), s.word(42));
        assert_ne!(s.word(1), s.word(2));
    }

    #[test]
    #[allow(deprecated)]
    fn test_sip_matches_std_siphash24() {
        use std::hash::{Hasher, SipHasher};
        let (k0, k1) = (0x0706_0504_0302_0100, 0x0F0E_0D0C_0B0A_0908);
        // The reference paper's vector for the empty input.
        assert_eq!(sip(k0, k1, b"", 2, 4), 0x726f_db47_dd0e_0e31);
        let data: Vec<u8> = (0u8..64).collect();
        for len in 0..=data.len() {
            let mut h = SipHasher::new_with_keys(k0, k1);
            h.write(&data[..len]);
            assert_eq!(sip(k0, k1, &data[..len], 2, 4), h.finish(), "len {len}");
        }
    }
}
