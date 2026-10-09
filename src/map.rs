//! The insertion-ordered hash map behind LSB maps (PHP arrays).
//!
//! Entries live in a vector in insertion order; an open-addressing index
//! (linear probing, power-of-two capacity, at most half full) maps hashes to
//! entry positions. Deleting marks the entry dead and removes it from the
//! index at once (backward-shift deletion, so the index never holds
//! tombstones); dead entries are compacted away when they outnumber the live
//! ones, which keeps every operation amortised O(1).
//!
//! Every entry carries a sequence number that increases with insertion order
//! and survives compaction. An iterator remembers the sequence number of the
//! last entry it produced, so it resumes correctly however the map changed:
//! deleted entries are skipped, entries added during iteration are visited
//! (LSB §5.10), and a compaction only costs the iterator one binary search.
//!
//! **Packed mode** (PHP's packed arrays): while the keys are exactly the
//! integers `0, 1, 2, ...` in insertion order with nothing deleted, the
//! position of key `k` is `k`, so the store keeps no index at all and a
//! lookup is one bounds check. The first insertion that breaks the pattern,
//! or any deletion, builds the index once and switches to hashed mode for
//! good. Lists, the commonest PHP array, never pay for hashing.
//!
//! The store knows nothing about key semantics: callers pass the hash, the
//! key's integer value (if it is an integer), and an equality test, because
//! comparing string keys needs the heap.

use alloc::vec::Vec;

const EMPTY: u32 = u32::MAX;

/// One entry. `key` is the normalised key word, `value` the value word, both
/// in the map's declared representation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub(crate) hash: u64,
    pub(crate) key: u64,
    pub(crate) value: u64,
    pub(crate) seq: u64,
    pub(crate) live: bool,
}

/// An ordered hash map of 64-bit words.
#[derive(Clone, Debug, Default)]
pub(crate) struct MapStore {
    entries: Vec<Entry>,
    index: Vec<u32>,
    live: usize,
    next_seq: u64,
    /// Bumped whenever entries move (compaction), invalidating position hints.
    epoch: u32,
    /// `1 + the largest integer key ever inserted`, or `None` before any.
    next_int: Option<i128>,
    /// Hashed mode (an index exists); packed mode while false.
    hashed: bool,
}

/// Where an iterator stands: the last sequence number it produced and a hint
/// for the position after it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cursor {
    pub(crate) last_seq: Option<u64>,
    pub(crate) pos: usize,
    pub(crate) epoch: u32,
}

impl MapStore {
    /// Live entries.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.live
    }

    /// Bytes the store holds, for memory accounting.
    pub(crate) fn bytes(&self) -> usize {
        self.entries.capacity() * core::mem::size_of::<Entry>() + self.index.capacity() * 4
    }

    /// The entry at a position returned by [`find`](Self::find).
    #[inline]
    pub(crate) fn entry(&self, pos: usize) -> Option<&Entry> {
        self.entries.get(pos)
    }

    /// All entries, live and dead, in order.
    #[inline]
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Whether the store has left packed mode.
    #[inline]
    pub(crate) fn is_hashed(&self) -> bool {
        self.hashed
    }

    /// The position of the live entry whose key satisfies `eq`; `int_key`
    /// is the key's integer value when it is an integer (packed lookups use
    /// only that, and ignore `hash`).
    #[inline]
    pub(crate) fn find(
        &self,
        hash: u64,
        int_key: Option<i128>,
        mut eq: impl FnMut(u64) -> bool,
    ) -> Option<usize> {
        if !self.hashed {
            // Packed: entry k holds key k, all live.
            return int_key
                .and_then(|k| usize::try_from(k).ok())
                .filter(|&i| i < self.entries.len());
        }
        if self.index.is_empty() {
            return None;
        }
        let mask = self.index.len() - 1;
        let mut slot = hash as usize & mask;
        loop {
            let pos = *self.index.get(slot)?;
            if pos == EMPTY {
                return None;
            }
            let e = self.entries.get(pos as usize)?;
            if e.hash == hash && eq(e.key) {
                return Some(pos as usize);
            }
            slot = (slot + 1) & mask;
        }
    }

    /// Overwrites the value of the entry at `pos`.
    #[inline]
    pub(crate) fn set_value(&mut self, pos: usize, value: u64) {
        if let Some(e) = self.entries.get_mut(pos) {
            e.value = value;
        }
    }

    /// Records that integer key `k` was inserted (for `map_push`).
    #[inline]
    pub(crate) fn note_int_key(&mut self, k: i128) {
        let next = k + 1;
        if self.next_int.is_none_or(|n| next > n) {
            self.next_int = Some(next);
        }
    }

    /// The next integer key (`1 + largest ever inserted`, or 0).
    #[inline]
    pub(crate) fn next_int(&self) -> i128 {
        self.next_int.unwrap_or(0)
    }

    /// Bytes inserting a new key (with integer value `int_key`, if any) may
    /// add, for budgeting before the insertion happens.
    pub(crate) fn growth_bytes(&self, int_key: Option<i128>) -> usize {
        let mut bytes = 0;
        if self.entries.len() == self.entries.capacity() {
            bytes += self.entries.capacity().max(4) * core::mem::size_of::<Entry>();
        }
        let stays_packed = !self.hashed && int_key == Some(self.entries.len() as i128);
        if !stays_packed {
            let needed = ((self.live + 1) * 2).next_power_of_two().max(8);
            if needed > self.index.len() {
                bytes += needed * 4;
            }
        }
        bytes
    }

    /// Appends a new entry (the caller checked the key is absent); `int_key`
    /// is the key's integer value, if it is an integer. Returns `false` only
    /// if the map would exceed `u32::MAX - 1` entries.
    pub(crate) fn insert(
        &mut self,
        hash: u64,
        key: u64,
        value: u64,
        int_key: Option<i128>,
    ) -> bool {
        if !self.hashed {
            let next = self.entries.len();
            if int_key == Some(next as i128) && next < (EMPTY - 1) as usize {
                self.entries.push(Entry {
                    hash,
                    key,
                    value,
                    seq: self.next_seq,
                    live: true,
                });
                self.next_seq += 1;
                self.live += 1;
                return true;
            }
            self.make_hashed();
        }
        if self.entries.len() >= (EMPTY - 1) as usize {
            if self.live < self.entries.len() {
                self.compact();
            }
            if self.entries.len() >= (EMPTY - 1) as usize {
                return false;
            }
        }
        if (self.live + 1) * 2 > self.index.len() {
            self.grow_index();
        }
        let pos = self.entries.len() as u32;
        self.entries.push(Entry {
            hash,
            key,
            value,
            seq: self.next_seq,
            live: true,
        });
        self.next_seq += 1;
        self.live += 1;
        self.place(hash, pos);
        true
    }

    /// Removes the live entry at `pos`; the others keep their order.
    pub(crate) fn remove(&mut self, pos: usize) {
        if !self.hashed {
            self.make_hashed();
        }
        let Some(e) = self.entries.get_mut(pos) else {
            return;
        };
        if !e.live {
            return;
        }
        e.live = false;
        let hash = e.hash;
        self.live -= 1;
        self.unplace(hash, pos as u32);
        let dead = self.entries.len() - self.live;
        if dead > 16 && dead > self.live {
            self.compact();
        }
    }

    /// The position of the next live entry after `cursor`, advancing it.
    pub(crate) fn next(&self, cursor: &mut Cursor) -> Option<usize> {
        let mut pos = match cursor.last_seq {
            None => 0,
            Some(seq) if cursor.epoch == self.epoch => {
                // The hint is exact while no compaction happened.
                let _ = seq;
                cursor.pos
            }
            Some(seq) => self.entries.partition_point(|e| e.seq <= seq),
        };
        while let Some(e) = self.entries.get(pos) {
            if e.live {
                cursor.last_seq = Some(e.seq);
                cursor.pos = pos + 1;
                cursor.epoch = self.epoch;
                return Some(pos);
            }
            pos += 1;
        }
        cursor.pos = pos;
        cursor.epoch = self.epoch;
        None
    }

    fn place(&mut self, hash: u64, pos: u32) {
        let mask = self.index.len() - 1;
        let mut slot = hash as usize & mask;
        while let Some(s) = self.index.get_mut(slot) {
            if *s == EMPTY {
                *s = pos;
                return;
            }
            slot = (slot + 1) & mask;
        }
    }

    /// Removes `pos` from the index with backward-shift deletion.
    fn unplace(&mut self, hash: u64, pos: u32) {
        if self.index.is_empty() {
            return;
        }
        let mask = self.index.len() - 1;
        let mut slot = hash as usize & mask;
        // Find the slot holding `pos`.
        loop {
            match self.index.get(slot) {
                Some(&p) if p == pos => break,
                Some(&EMPTY) | None => return,
                Some(_) => slot = (slot + 1) & mask,
            }
        }
        let mut hole = slot;
        let mut next = (hole + 1) & mask;
        loop {
            let p = match self.index.get(next) {
                Some(&p) if p != EMPTY => p,
                _ => break,
            };
            let ideal = self
                .entries
                .get(p as usize)
                .map_or(next, |e| e.hash as usize & mask);
            // Move `p` back into the hole unless its ideal slot lies
            // cyclically in (hole, next].
            let dist_next = next.wrapping_sub(ideal) & mask;
            let dist_hole = hole.wrapping_sub(ideal) & mask;
            if dist_hole < dist_next {
                if let Some(h) = self.index.get_mut(hole) {
                    *h = p;
                }
                hole = next;
            }
            next = (next + 1) & mask;
        }
        if let Some(h) = self.index.get_mut(hole) {
            *h = EMPTY;
        }
    }

    /// Leaves packed mode: builds the index over the existing entries.
    fn make_hashed(&mut self) {
        self.hashed = true;
        let cap = (self.live * 2).next_power_of_two().max(8);
        self.rebuild_index(cap);
    }

    fn grow_index(&mut self) {
        let cap = (self.index.len() * 2).max(8);
        self.rebuild_index(cap);
    }

    fn rebuild_index(&mut self, cap: usize) {
        self.index.clear();
        self.index.resize(cap, EMPTY);
        let mask = cap - 1;
        for (pos, e) in self.entries.iter().enumerate() {
            if !e.live {
                continue;
            }
            let mut slot = e.hash as usize & mask;
            while let Some(s) = self.index.get_mut(slot) {
                if *s == EMPTY {
                    *s = pos as u32;
                    break;
                }
                slot = (slot + 1) & mask;
            }
        }
    }

    fn compact(&mut self) {
        self.entries.retain(|e| e.live);
        self.epoch = self.epoch.wrapping_add(1);
        let cap = self.index.len().max(8);
        self.rebuild_index(cap);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn h(k: u64) -> u64 {
        // Deliberately weak, so probe chains and wrap-around get exercised.
        k % 7
    }

    fn get(m: &MapStore, k: u64) -> Option<u64> {
        m.find(h(k), Some(i128::from(k)), |x| x == k)
            .and_then(|p| m.entry(p))
            .map(|e| e.value)
    }

    #[test]
    fn test_insert_find_remove_keep_order() {
        let mut m = MapStore::default();
        for k in 0..100u64 {
            assert!(m.insert(h(k), k, k * 10, Some(i128::from(k))));
        }
        for k in (0..100u64).step_by(3) {
            let pos = m.find(h(k), Some(i128::from(k)), |x| x == k);
            assert!(pos.is_some());
            m.remove(pos.unwrap_or(0));
        }
        for k in 0..100u64 {
            let expect = if k % 3 == 0 { None } else { Some(k * 10) };
            assert_eq!(get(&m, k), expect, "key {k}");
        }
        let order: Vec<u64> = m
            .entries()
            .iter()
            .filter(|e| e.live)
            .map(|e| e.key)
            .collect();
        let expect: Vec<u64> = (0..100).filter(|k| k % 3 != 0).collect();
        assert_eq!(order, expect);
    }

    #[test]
    fn test_cursor_survives_compaction_and_sees_appends() {
        let mut m = MapStore::default();
        for k in 0..50u64 {
            assert!(m.insert(h(k), k, k, None));
        }
        let mut c = Cursor::default();
        let first = m.next(&mut c).and_then(|p| m.entry(p)).map(|e| e.key);
        assert_eq!(first, Some(0));
        // Delete most entries (forces compaction), then append one.
        for k in 1..45u64 {
            if let Some(p) = m.find(h(k), None, |x| x == k) {
                m.remove(p);
            }
        }
        assert!(m.insert(h(500), 500, 500, None));
        let mut rest = Vec::new();
        while let Some(p) = m.next(&mut c) {
            rest.push(m.entry(p).map_or(0, |e| e.key));
        }
        assert_eq!(rest, vec![45, 46, 47, 48, 49, 500]);
    }

    #[test]
    fn test_next_int_tracks_largest_ever() {
        let mut m = MapStore::default();
        assert_eq!(m.next_int(), 0);
        m.note_int_key(-5);
        assert_eq!(m.next_int(), -4);
        m.note_int_key(10);
        m.note_int_key(3);
        assert_eq!(m.next_int(), 11);
    }
}
