//! Arrays, maps, and iterators: the operations behind LSB §5.10, shared by
//! the typed instructions and the dynamic ones.
//!
//! Storage is copy-on-write: `dup` and every load of an aggregate constant
//! share the element storage, and the first mutation through either object
//! copies it (charging the copy to the memory budget first). PHP's value
//! semantics for arrays are therefore a `dup` per assignment, and cost O(1)
//! until someone writes.
//!
//! Map keys compare per LSB §2.4: integers, `bool`, and `char` by value,
//! strings bytewise, other references by identity, floats by bits after
//! folding `-0.0` into `+0.0` (every `dyn` NaN is already one NaN), and for
//! `dyn` keys the kind is part of the key (`int 1` and `float 1.0` differ).
//!
//! **Reference slots** (LSB §5.17). A `dyn` element or map value may hold a
//! PHP reference box; such a slot is transparent. Every value read here
//! returns the box's value, every value write stores into the box, and a
//! box given as the value to store is stored as its value: a slot becomes a
//! reference slot only through the reference instructions
//! ([`crate::refs`]), which use the `_raw` operations. The test is one
//! comparison on the word ([`dynv::is_box`]) plus the slot type being
//! `dyn`, so containers without references pay one predictable branch.
//!
//! **Copies mark what they share** (LSB §5.16). When a write copies shared
//! contents, every array and map among them becomes `aliased` (two
//! containers now hold it), before the copy is made.

use alloc::sync::Arc;
use alloc::vec::Vec;

use bytecode_lang::{ErrorKind, IntTy, ValType};

use crate::conv;
use crate::dynv::{self, Raw};
use crate::fault::Fault;
use crate::hash::Seed;
use crate::heap::{ArrayObj, Heap, IterObj, MapObj, Object};
use crate::int;
use crate::map::{Cursor, MapStore};

const F32_CANON_NAN: u32 = 0x7FC0_0000;

#[cold]
#[inline(never)]
pub(crate) fn null() -> Fault {
    Fault::Raise(ErrorKind::NullReference)
}

#[cold]
#[inline(never)]
pub(crate) fn out_of_bounds() -> Fault {
    Fault::Raise(ErrorKind::IndexOutOfBounds)
}

/// The fault for a word that is not the object an instruction needs:
/// `NullReference` for `nil` (or a reference to a collected object, which
/// reads as `nil`), `TypeError` for anything else.
#[cold]
#[inline(never)]
pub(crate) fn not_a(heap: &Heap, v: u64) -> Fault {
    if heap.get(v).is_none() && (dynv::is_ref(v) || dynv::decode(v) == Raw::Nil) {
        null()
    } else {
        Fault::type_error()
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The canonical form of a key word of declared type `kty`.
#[inline]
pub(crate) fn normalize_key(kty: ValType, key: u64) -> u64 {
    match kty {
        ValType::Bool => u64::from(key != 0),
        ValType::Char => key & 0xFFFF_FFFF,
        ValType::F32 => {
            let f = f32::from_bits(key as u32);
            if f.is_nan() {
                u64::from(F32_CANON_NAN)
            } else if f == 0.0 {
                0
            } else {
                u64::from(f.to_bits())
            }
        }
        ValType::F64 => {
            let f = f64::from_bits(key);
            if f.is_nan() {
                dynv::CANONICAL_NAN
            } else if f == 0.0 {
                0
            } else {
                key
            }
        }
        ValType::Dyn => {
            if dynv::is_float(key) && dynv::float_value(key) == 0.0 {
                dynv::from_f64(0.0)
            } else {
                key
            }
        }
        ValType::Str | ValType::Ref(_) => key,
        _ => match kty.as_int() {
            Some(it) => int::normalize(it, key),
            None => key,
        },
    }
}

/// The hash of a normalised key.
#[inline]
pub(crate) fn hash_key(heap: &Heap, seed: Seed, kty: ValType, key: u64) -> u64 {
    match kty {
        ValType::Str => match heap.str(key) {
            Some(b) => seed.bytes(b),
            None => seed.word(key),
        },
        ValType::Dyn => {
            if dynv::is_inline_int(key) {
                return seed.word(dynv::inline_int_value(key) as u64);
            }
            match heap.get(key) {
                Some(Object::Str(b)) => seed.bytes(b),
                Some(Object::Int(i)) => seed.word(*i as u64),
                _ => seed.word(key),
            }
        }
        _ => seed.word(key),
    }
}

/// Whether two normalised keys are the same key.
#[inline]
pub(crate) fn key_eq(heap: &Heap, kty: ValType, a: u64, b: u64) -> bool {
    if a == b {
        return true;
    }
    match kty {
        ValType::Str => matches!((heap.str(a), heap.str(b)), (Some(x), Some(y)) if x == y),
        ValType::Dyn => match (heap.get(a), heap.get(b)) {
            (Some(Object::Str(x)), Some(Object::Str(y))) => x == y,
            // Ints in the inline range are always inline, so a boxed int
            // can only equal another boxed int.
            (Some(Object::Int(x)), Some(Object::Int(y))) => x == y,
            _ => false,
        },
        _ => false,
    }
}

/// The integer value of a key, for the next-integer-key rule.
#[inline]
fn int_key(heap: &Heap, kty: ValType, key: u64) -> Option<i128> {
    match kty {
        ValType::Dyn => conv::dyn_int(heap, key).map(i128::from),
        _ => kty.as_int().map(|it| int::value(it, key)),
    }
}

// ---------------------------------------------------------------------------
// Maps
// ---------------------------------------------------------------------------

/// The map behind a word, or the fault for a non-map.
#[inline]
pub(crate) fn map_ref(heap: &Heap, map: u64) -> Result<&MapObj, Fault> {
    match heap.get(map) {
        Some(Object::Map(m)) => Ok(m),
        _ => Err(not_a(heap, map)),
    }
}

/// Looks up `key` (already in the map's key representation). Returns the
/// value word.
pub(crate) fn map_get(heap: &Heap, seed: Seed, map: u64, key: u64) -> Result<Option<u64>, Fault> {
    let m = map_ref(heap, map)?;
    let kty = m.key;
    let key = normalize_key(kty, key);
    let ik = int_key(heap, kty, key);
    // A packed map needs only the integer value.
    let hash = if m.store.is_hashed() {
        hash_key(heap, seed, kty, key)
    } else {
        0
    };
    let found = m
        .store
        .find(hash, ik, |k| key_eq(heap, kty, k, key))
        .and_then(|p| m.store.entry(p))
        .map(|e| e.value);
    Ok(match found {
        Some(w) if dynv::is_box(w) && m.value == ValType::Dyn => Some(heap.deref(w)),
        other => other,
    })
}

/// Where a key is (or would go) in a map: everything a write needs, from
/// one lookup of the map object.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Found {
    /// The entry position, if the key is present.
    pub(crate) pos: Option<usize>,
    /// The key in the map's key representation, its hash and integer value.
    key: u64,
    hash: u64,
    ik: Option<i128>,
    /// Bytes an insertion would add.
    growth: usize,
}

/// Locates `key` (converted to the map's key representation) in `map`.
#[inline]
pub(crate) fn map_find(heap: &Heap, seed: Seed, map: u64, key: u64) -> Result<Found, Fault> {
    let m = map_ref(heap, map)?;
    let kty = m.key;
    let key = normalize_key(kty, key);
    let hash = hash_key(heap, seed, kty, key);
    let ik = int_key(heap, kty, key);
    let pos = m.store.find(hash, ik, |k| key_eq(heap, kty, k, key));
    let growth = if pos.is_none() {
        m.store.growth_bytes(ik)
    } else {
        0
    };
    Ok(Found {
        pos,
        key,
        hash,
        ik,
        growth,
    })
}

/// The raw word at a map position (a box stays a box).
pub(crate) fn map_word_at(heap: &Heap, map: u64, pos: usize) -> Option<u64> {
    match heap.get(map) {
        Some(Object::Map(m)) => m.store.entry(pos).map(|e| e.value),
        _ => None,
    }
}

/// Looks up a string key given as bytes (property access), without
/// allocating. Only maps keyed by `str` or `dyn` can hold one.
pub(crate) fn map_get_bytes(heap: &Heap, seed: Seed, map: u64, name: &[u8]) -> Option<u64> {
    let Some(Object::Map(m)) = heap.get(map) else {
        return None;
    };
    if !matches!(m.key, ValType::Str | ValType::Dyn) {
        return None;
    }
    let hash = seed.bytes(name);
    let w = m
        .store
        .find(hash, None, |k| heap.str(k) == Some(name))
        .and_then(|p| m.store.entry(p))
        .map(|e| e.value)?;
    Some(if m.value == ValType::Dyn {
        heap.deref(w)
    } else {
        w
    })
}

/// Mutable access to a map's store for writing (LSB §5.16): refuses frozen
/// constants; a write to a `cow` map marks the containers in its contents
/// aliased and clears the bit; storage still physically shared is copied
/// (the copy charged first).
pub(crate) fn map_store_mut(
    heap: &mut Heap,
    map: u64,
    extra: usize,
) -> Result<&mut MapStore, Fault> {
    let (shared_bytes, frozen, old) = match heap.get(map) {
        Some(Object::Map(m)) => {
            let cow = m.contents_shared();
            let holds_refs = m.key.is_reference() || m.value.is_reference();
            (
                if Arc::strong_count(&m.store) > 1 {
                    m.store.bytes()
                } else {
                    0
                },
                m.frozen,
                (cow && holds_refs).then(|| Arc::clone(&m.store)),
            )
        }
        _ => return Err(not_a(heap, map)),
    };
    if frozen {
        return Err(Fault::type_error());
    }
    heap.charge(shared_bytes.saturating_add(extra))?;
    if let Some(old) = old {
        heap.mark_aliased(
            old.entries()
                .iter()
                .filter(|e| e.live)
                .flat_map(|e| [e.key, e.value]),
        );
    }
    match heap.get_mut(map) {
        Some(Object::Map(m)) => {
            m.cow = false;
            Ok(Arc::make_mut(&mut m.store))
        }
        _ => Err(Fault::type_error()),
    }
}

/// Makes a map's contents its own before a write (copy-on-write).
pub(crate) fn unique_map(heap: &mut Heap, map: u64) -> Result<(), Fault> {
    map_store_mut(heap, map, 0).map(|_| ())
}

/// Makes an array's elements its own before a write (copy-on-write).
pub(crate) fn unique_array(heap: &mut Heap, arr: u64) -> Result<(), Fault> {
    array_items_mut(heap, arr, 0).map(|_| ())
}

/// `map_set`: updates an existing key in place or appends a new one. In a
/// map of `dyn` values a box is stored as its value, and a reference slot is
/// written through (LSB §5.17).
///
/// One lookup of the map object serves the search, the reference-slot test,
/// and the growth estimate: splitting them into a search returning a record
/// and a separate store measured ~15% slower on `map/int_keys_100k_set_get`.
pub(crate) fn map_set(
    heap: &mut Heap,
    seed: Seed,
    map: u64,
    key: u64,
    value: u64,
) -> Result<(), Fault> {
    let m = map_ref(heap, map)?;
    let kty = m.key;
    let key = normalize_key(kty, key);
    let hash = hash_key(heap, seed, kty, key);
    let ik = int_key(heap, kty, key);
    let pos = m.store.find(hash, ik, |k| key_eq(heap, kty, k, key));
    let mut value = value;
    let mut through = None;
    if m.value == ValType::Dyn {
        if let Some(w) = pos.and_then(|p| m.store.entry(p)).map(|e| e.value) {
            if dynv::is_box(w) {
                through = Some(w);
            }
        }
        if dynv::is_box(value) {
            value = heap.deref(value);
        }
    }
    let growth = if pos.is_none() {
        m.store.growth_bytes(ik)
    } else {
        0
    };
    if let Some(w) = through {
        if heap.set_box(w, value) {
            return Ok(());
        }
    }
    store_at(heap, map, (pos, key, hash, ik, growth), value)
}

/// Stores `value` under `key` as it is (a box makes a reference slot):
/// updates an existing key in place or appends a new one.
pub(crate) fn map_set_raw(
    heap: &mut Heap,
    seed: Seed,
    map: u64,
    key: u64,
    value: u64,
) -> Result<(), Fault> {
    let f = map_find(heap, seed, map, key)?;
    store_at(heap, map, (f.pos, f.key, f.hash, f.ik, f.growth), value)
}

/// The store half of a map write: (position, key, hash, integer key,
/// growth) from the search.
#[inline(always)]
fn store_at(
    heap: &mut Heap,
    map: u64,
    (pos, key, hash, ik, growth): (Option<usize>, u64, u64, Option<i128>, usize),
    value: u64,
) -> Result<(), Fault> {
    let store = map_store_mut(heap, map, growth)?;
    match pos {
        Some(p) => store.set_value(p, value),
        None => {
            if !store.insert(hash, key, value, ik) {
                return Err(Fault::Trap(ErrorKind::OutOfMemory));
            }
            if let Some(k) = ik {
                store.note_int_key(k);
            }
        }
    }
    Ok(())
}

/// `map_del`: removes the key if present.
pub(crate) fn map_del(heap: &mut Heap, seed: Seed, map: u64, key: u64) -> Result<(), Fault> {
    let m = map_ref(heap, map)?;
    let kty = m.key;
    let key = normalize_key(kty, key);
    let hash = hash_key(heap, seed, kty, key);
    let ik = int_key(heap, kty, key);
    let Some(pos) = m.store.find(hash, ik, |k| key_eq(heap, kty, k, key)) else {
        if m.frozen {
            return Err(Fault::type_error());
        }
        return Ok(());
    };
    map_store_mut(heap, map, 0)?.remove(pos);
    Ok(())
}

/// `map_push`: appends under the next integer key.
pub(crate) fn map_push(heap: &mut Heap, seed: Seed, map: u64, value: u64) -> Result<(), Fault> {
    let key = next_key(heap, map)?;
    map_set(heap, seed, map, key, value)
}

/// `map_push` storing `value` as it is (a box makes a reference slot): how a
/// by-reference `rest_map` parameter collects its positional items.
pub(crate) fn map_push_raw(heap: &mut Heap, seed: Seed, map: u64, value: u64) -> Result<(), Fault> {
    let key = next_key(heap, map)?;
    map_set_raw(heap, seed, map, key, value)
}

/// The key `map_push` inserts under (the next integer key, LSB §5.10).
fn next_key(heap: &mut Heap, map: u64) -> Result<u64, Fault> {
    let m = map_ref(heap, map)?;
    let kty = m.key;
    let next = m.store.next_int();
    let key = match kty {
        ValType::Dyn => {
            let i = i64::try_from(next).map_err(|_| Fault::raise(ErrorKind::ArithOverflow))?;
            conv::encode_int(heap, i)?
        }
        _ => match kty.as_int() {
            Some(it) => {
                if next > i128::from(i64::MAX) {
                    return Err(Fault::raise(ErrorKind::ArithOverflow));
                }
                int::from_value(it, next).ok_or(Fault::Raise(ErrorKind::ArithOverflow))?
            }
            None => return Err(Fault::type_error()),
        },
    };
    Ok(key)
}

/// A new map whose storage the caller fills.
pub(crate) fn new_map(heap: &mut Heap, key: ValType, value: ValType) -> Result<u64, Fault> {
    heap.alloc(Object::Map(MapObj {
        key,
        value,
        frozen: false,
        cow: false,
        aliased: false,
        store: Arc::new(MapStore::default()),
    }))
}

// ---------------------------------------------------------------------------
// Arrays
// ---------------------------------------------------------------------------

/// The array behind a word, or the fault for a non-array.
#[inline]
pub(crate) fn array_ref(heap: &Heap, arr: u64) -> Result<&ArrayObj, Fault> {
    match heap.get(arr) {
        Some(Object::Array(a)) => Ok(a),
        _ => Err(not_a(heap, arr)),
    }
}

/// The element at `idx` (a reference slot reads as its value).
#[inline]
pub(crate) fn array_get(heap: &Heap, arr: u64, idx: i64) -> Result<u64, Fault> {
    let a = array_ref(heap, arr)?;
    let w = usize::try_from(idx)
        .ok()
        .and_then(|i| a.items.get(i).copied())
        .ok_or_else(out_of_bounds)?;
    Ok(if dynv::is_box(w) && a.elem == ValType::Dyn {
        heap.deref(w)
    } else {
        w
    })
}

/// Mutable access to an array's storage for writing: as
/// [`map_store_mut`].
pub(crate) fn array_items_mut(
    heap: &mut Heap,
    arr: u64,
    extra: usize,
) -> Result<&mut Vec<u64>, Fault> {
    let (shared_bytes, frozen, old) = match heap.get(arr) {
        Some(Object::Array(a)) => {
            let cow = a.contents_shared();
            (
                if Arc::strong_count(&a.items) > 1 {
                    a.items.len() * 8
                } else {
                    0
                },
                a.frozen,
                (cow && a.elem.is_reference()).then(|| Arc::clone(&a.items)),
            )
        }
        _ => return Err(not_a(heap, arr)),
    };
    if frozen {
        return Err(Fault::type_error());
    }
    heap.charge(shared_bytes.saturating_add(extra))?;
    if let Some(old) = old {
        heap.mark_aliased(old.iter().copied());
    }
    match heap.get_mut(arr) {
        Some(Object::Array(a)) => {
            a.cow = false;
            Ok(Arc::make_mut(&mut a.items))
        }
        _ => Err(Fault::type_error()),
    }
}

/// `array_set`: in an array of `dyn` a box is stored as its value and a
/// reference slot is written through (LSB §5.17).
pub(crate) fn array_set(heap: &mut Heap, arr: u64, idx: i64, v: u64) -> Result<(), Fault> {
    let a = array_ref(heap, arr)?;
    let len = a.items.len();
    let dyn_elems = a.elem == ValType::Dyn;
    let i = usize::try_from(idx)
        .ok()
        .filter(|&i| i < len)
        .ok_or_else(out_of_bounds)?;
    let mut v = v;
    if dyn_elems {
        v = heap.deref(v);
        let old = a.items.get(i).copied().unwrap_or(dynv::NIL);
        if dynv::is_box(old) && heap.set_box(old, v) {
            return Ok(());
        }
    }
    array_set_raw(heap, arr, i, v)
}

/// Stores `v` at the in-range index `i` as it is (a box makes a reference
/// slot).
pub(crate) fn array_set_raw(heap: &mut Heap, arr: u64, i: usize, v: u64) -> Result<(), Fault> {
    if let Some(slot) = array_items_mut(heap, arr, 0)?.get_mut(i) {
        *slot = v;
    }
    Ok(())
}

/// `array_push` (a box is pushed as its value into an array of `dyn`).
pub(crate) fn array_push(heap: &mut Heap, arr: u64, v: u64) -> Result<(), Fault> {
    let a = array_ref(heap, arr)?;
    let grow = if a.items.len() == a.items.capacity() {
        a.items.capacity().max(4) * 8
    } else {
        0
    };
    let v = if a.elem == ValType::Dyn {
        heap.deref(v)
    } else {
        v
    };
    array_items_mut(heap, arr, grow)?.push(v);
    Ok(())
}

/// `array_pop` (a reference slot pops as its value).
pub(crate) fn array_pop(heap: &mut Heap, arr: u64) -> Result<u64, Fault> {
    let a = array_ref(heap, arr)?;
    if a.items.is_empty() {
        return Err(out_of_bounds());
    }
    let dyn_elems = a.elem == ValType::Dyn;
    let w = array_items_mut(heap, arr, 0)?
        .pop()
        .ok_or_else(out_of_bounds)?;
    Ok(if dyn_elems { heap.deref(w) } else { w })
}

/// A new array of `len` default elements.
pub(crate) fn new_array(heap: &mut Heap, elem: ValType, len: i64) -> Result<u64, Fault> {
    let n = usize::try_from(len).map_err(|_| out_of_bounds())?;
    // Check the budget before asking the allocator for the buffer, so a
    // hostile length is a trap, not an allocation failure (and never more
    // than a `Vec` can hold, whatever the budget).
    if n > isize::MAX as usize / 8 || !heap.fits(n.saturating_mul(8)) {
        return Err(Fault::Trap(ErrorKind::OutOfMemory));
    }
    heap.alloc(Object::Array(ArrayObj {
        elem,
        frozen: false,
        cow: false,
        aliased: false,
        items: Arc::new(alloc::vec![0; n]),
    }))
}

/// A new `dyn` array holding `items` (already values, never boxes unless
/// the caller makes reference slots on purpose).
pub(crate) fn new_dyn_array(heap: &mut Heap, items: Vec<u64>) -> Result<u64, Fault> {
    if !heap.fits(items.len().saturating_mul(8)) {
        return Err(Fault::Trap(ErrorKind::OutOfMemory));
    }
    heap.alloc(Object::Array(ArrayObj {
        elem: ValType::Dyn,
        frozen: false,
        cow: false,
        aliased: false,
        items: Arc::new(items),
    }))
}

// ---------------------------------------------------------------------------
// Iterators
// ---------------------------------------------------------------------------

/// A new iterator over an array, a map, or a coroutine (`dynamic` for
/// `diter_new`). `None` when `src` is none of them.
///
/// An iterator over a coroutine always produces `dyn` keys and values (LSB
/// §5.13 rule 7: `iter_new` gives a `ref iter dyn -> dyn`), and its
/// `iter_next` is executed by the coroutine machinery, which resumes it.
pub(crate) fn new_iter(heap: &mut Heap, src: u64, dynamic: bool) -> Option<Result<u64, Fault>> {
    let (key_ty, coro) = match heap.get(src)? {
        Object::Array(_) => (ValType::I64, false),
        Object::Map(m) => (m.key, false),
        Object::Coro(_) => (ValType::Dyn, true),
        _ => return None,
    };
    let dynamic = dynamic || coro;
    Some(heap.alloc(Object::Iter(alloc::boxed::Box::new(IterObj {
        src,
        dynamic,
        done: false,
        index: 0,
        cursor: Cursor::default(),
        key: None,
        key_ty: if dynamic { ValType::Dyn } else { key_ty },
        coro,
    }))))
}

/// `iter_next`: the next value (in the iterator's representation), or
/// `None` once exhausted.
pub(crate) fn iter_next(heap: &mut Heap, it: u64) -> Result<Option<u64>, Fault> {
    let (src, dynamic, done, index, mut cursor) = match heap.get(it) {
        Some(Object::Iter(i)) => (i.src, i.dynamic, i.done, i.index, i.cursor),
        _ => return Err(not_a(heap, it)),
    };
    if done {
        return Ok(None);
    }
    // (key, value) words in the collection's representation, and their types.
    let step: Option<(u64, u64, ValType, ValType)> = match heap.get(src) {
        Some(Object::Array(a)) => a
            .items
            .get(index)
            .map(|&v| (index as u64, v, ValType::I64, a.elem)),
        Some(Object::Map(m)) => m
            .store
            .next(&mut cursor)
            .and_then(|p| m.store.entry(p))
            .map(|e| (e.key, e.value, m.key, m.value)),
        _ => None,
    };
    let Some((k, v, kty, vty)) = step else {
        if let Some(Object::Iter(i)) = heap.get_mut(it) {
            i.done = true;
        }
        return Ok(None);
    };
    // A reference slot yields its value (LSB §5.10, §5.17).
    let v = if vty == ValType::Dyn {
        heap.deref(v)
    } else {
        v
    };
    let (k, v) = if dynamic {
        (conv::to_dyn(heap, kty, k)?, conv::to_dyn(heap, vty, v)?)
    } else {
        (k, v)
    };
    if let Some(Object::Iter(i)) = heap.get_mut(it) {
        i.index = index + 1;
        i.cursor = cursor;
        i.key = Some(k);
    }
    Ok(Some(v))
}

/// `iter_key`.
pub(crate) fn iter_key(heap: &Heap, it: u64) -> Result<u64, Fault> {
    match heap.get(it) {
        Some(Object::Iter(i)) => i.key.ok_or_else(out_of_bounds),
        _ => Err(not_a(heap, it)),
    }
}

/// Converts an `i64` index word read from a typed register.
#[inline]
pub(crate) fn index_of(bits: u64) -> i64 {
    int::value(IntTy::I64, bits) as i64
}
