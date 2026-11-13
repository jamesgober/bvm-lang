//! The VM heap: every object a program allocates, and the tracing collector
//! that reclaims them.
//!
//! Objects live in a slot vector addressed by the 32-bit index inside a
//! reference word (see [`dynv`](crate::dynv)); each slot carries a 16-bit
//! tag that the reference must match: a 15-bit generation, and in bit 15 the
//! flag that the object is a PHP reference box (so the word itself says so,
//! [`dynv::is_box`]). A freed slot's generation advances, so a stale
//! reference (only producible by a module a verifier would reject, or by a
//! host holding a value across runs) reads as `nil` instead of aliasing a
//! newer object. A slot whose generation would wrap is retired rather than
//! reused, so aliasing is impossible, not merely unlikely.
//!
//! **Sharing** (LSB §5.16). Arrays and maps keep their contents behind an
//! `Arc`, shared copy-on-write between a container and its `dup`s (and the
//! loads of an aggregate constant). Each carries the two bits of LSB §5.16's
//! conforming implementation for tracing collectors: `cow`, "my contents may
//! be shared" (`dup` sets it on both containers, a constant load on the
//! copy; the first write clears it on the written container), and `aliased`,
//! "another container's contents may hold me" (set on every array and map in
//! contents that a write to a `cow` container, or an eager `dup` of a struct
//! or cell, copies; see [`Heap::mark_aliased`]). `dsep_*` replaces exactly
//! the aliased elements. The decisions follow the bits, never the `Arc`'s
//! count (which only says whether the physical copy is still needed), so
//! which writes copy and which elements separate is a deterministic function
//! of the program, independent of when collections run. Both questions are
//! asked only through `contents_shared` (on [`ArrayObj`] and [`MapObj`]) and
//! [`Heap::is_aliased`]: the seam a reference-counting memory profile (D22)
//! replaces with "the count is above one".
//!
//! Collection is a stop-the-world mark and sweep with an explicit work list
//! (no recursion, whatever the object graph's depth) over the exact roots the
//! VM supplies: reference-typed registers, globals, constant caches, and the
//! running closures (LSB §1.4). Its cost is linear in the heap, which the
//! memory budget bounds, and it runs only when enough has been allocated since
//! the previous collection (at least as much as survived it), so the total
//! collection work stays proportional to the total allocation.
//!
//! **Close on drop** (LSB §5.13 rule 13). A suspended coroutine that the mark
//! phase did not reach is not freed: it is *resurrected* (marked, with
//! everything it reaches) and handed back to the VM, which closes it (runs its
//! pending `finally` blocks) at the next safepoint. The heap keeps the slot
//! indices of its coroutines so this costs O(coroutines), not a heap scan.
//! Each coroutine is resurrected at most once (`finalized`); a later
//! collection that finds it unreachable again frees it. Objects whose
//! struct type has a destructor ([`Heap::dtors`]) are resurrected and queued
//! the same way, after the coroutines, in heap order.
//!
//! **Reference counting** (the `rc` memory profile, DECISIONS D22). The same
//! slots also carry a reference count and three collector bits. In that
//! profile every holder of a reference word (a reference-typed register of a
//! live or suspended frame, a frame's running closure, a reference-typed slot
//! of an object, a global, a constant cache, a running or queued coroutine,
//! a value handed to the host) contributes one count, kept by [`Heap::inc`]
//! and [`Heap::dec`] at every store. Array and map *storage* (the
//! copy-on-write `Arc`) counts its words once, however many containers share
//! it: a container adds its elements' counts only when it creates fresh
//! storage, and releases them only when it drops the last `Arc`. A count
//! reaching zero never frees anything at once: the word goes on the
//! [`Heap::zero`] list, which the VM settles at the next instruction boundary
//! ([`crate::rc`]), so no word an instruction holds can go stale while it
//! runs. Freeing is iterative (the list is the work list), whatever the depth
//! of what dies. Garbage cycles are found by trial deletion
//! ([`crate::cycle`]) from the possible roots [`Heap::dec`] buffers. A count
//! that would pass [`Heap::rc_max`] sticks there (the object is never freed
//! again) and raises the `OutOfMemory` trap at the next boundary.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use bytecode_lang::{CoroState, ErrorKind, Kind, ValType};

use crate::dynv;
use crate::fault::Fault;
use crate::machine::Frame;
use crate::map::{Cursor, MapStore};
use crate::program::Program;

/// A heap object.
#[derive(Debug)]
pub(crate) enum Object {
    /// An immutable byte string.
    Str(Box<[u8]>),
    /// A `dyn` int outside the inline range.
    Int(i64),
    Array(ArrayObj),
    Map(MapObj),
    Struct(StructObj),
    Func(FuncObj),
    Cell(CellObj),
    Iter(Box<IterObj>),
    Error(ErrorObj),
    /// A coroutine (LSB §5.13).
    Coro(Box<CoroObj>),
    /// A PHP reference (LSB §5.17, kind `reference`): a box holding one
    /// `dyn` value, never itself a box. Its word carries
    /// [`dynv::BOX_FLAG`].
    Ref(u64),
}

impl ArrayObj {
    /// LSB §5.16 `cow` for this array: the one definition every
    /// copy-on-write decision reads, in both memory profiles (see
    /// [`Heap::is_aliased`] for why the RC profile does not use counts
    /// here). Whether the copy is *physical* is the storage `Arc`'s own
    /// count: in the RC profile that count is exact, and a physical copy is
    /// where PHP's reference unbinding happens.
    #[inline]
    pub(crate) fn contents_shared(&self) -> bool {
        self.cow
    }
}

impl MapObj {
    /// As [`ArrayObj::contents_shared`].
    #[inline]
    pub(crate) fn contents_shared(&self) -> bool {
        self.cow
    }
}

/// An array. The element storage is shared copy-on-write between an array
/// and its `dup`s and between the loads of one aggregate constant.
#[derive(Debug)]
pub(crate) struct ArrayObj {
    pub(crate) elem: ValType,
    /// Part of a materialised aggregate constant (nested inside another), so
    /// shared by every load of it: mutation raises `TypeError` (LSB §3.2
    /// requires a `dup` first).
    pub(crate) frozen: bool,
    /// LSB §5.16 `cow`: the elements may be shared with another container.
    pub(crate) cow: bool,
    /// LSB §5.16 `aliased`: another container's contents may hold this one.
    pub(crate) aliased: bool,
    pub(crate) items: Arc<Vec<u64>>,
}

/// A map, with the same sharing as [`ArrayObj`].
#[derive(Debug)]
pub(crate) struct MapObj {
    pub(crate) key: ValType,
    pub(crate) value: ValType,
    pub(crate) frozen: bool,
    /// As [`ArrayObj::cow`].
    pub(crate) cow: bool,
    /// As [`ArrayObj::aliased`].
    pub(crate) aliased: bool,
    pub(crate) store: Arc<MapStore>,
}

/// A struct instance: its struct type and its fields (the parent's first).
#[derive(Debug)]
pub(crate) struct StructObj {
    pub(crate) ty: u32,
    /// Its destructor has run or been queued: it never runs twice, even for
    /// an object a destructor resurrected.
    pub(crate) destructed: bool,
    pub(crate) fields: Box<[u64]>,
}

/// What a function value calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Callable {
    Func(u32),
    Import(u32),
}

/// A function value: a closure (a function and its captured values), a
/// method reference, or an import reference.
#[derive(Debug)]
pub(crate) struct FuncObj {
    pub(crate) target: Callable,
    pub(crate) captures: Box<[u64]>,
}

/// A mutable cell.
#[derive(Debug)]
pub(crate) struct CellObj {
    pub(crate) elem: ValType,
    pub(crate) value: u64,
}

/// An iterator over an array or a map.
#[derive(Debug)]
pub(crate) struct IterObj {
    /// The array or map (a reference word).
    pub(crate) src: u64,
    /// Made by `diter_new`: keys and values are produced as `dyn`.
    pub(crate) dynamic: bool,
    /// Reported `has = false` once; stays exhausted.
    pub(crate) done: bool,
    /// Array position.
    pub(crate) index: usize,
    /// Map position.
    pub(crate) cursor: Cursor,
    /// Key of the element the last `iter_next` produced.
    pub(crate) key: Option<u64>,
    /// Representation of `key` (for tracing).
    pub(crate) key_ty: ValType,
    /// The source is a coroutine: `iter_next` resumes it (LSB §5.13 rule 7).
    pub(crate) coro: bool,
}

/// A coroutine: a suspendable stack of frames (LSB §5.13, "What a coroutine
/// owns").
///
/// While it runs, its frames live on the VM's active stack like any others
/// and `frames`/`regs` are empty; when it suspends they are copied here (the
/// copy-on-suspend answer to LSB §10 question 7: a switch costs one copy of
/// the suspended registers, and the dispatch loop keeps one flat stack, so a
/// register read stays one indexed load). A created coroutine holds its
/// body's first frame with the arguments in place, so starting one is an
/// ordinary resume.
#[derive(Debug)]
pub(crate) struct CoroObj {
    pub(crate) state: CoroState,
    /// Being closed (rule 11): a `yield` is refused with `CloseIgnored`, and
    /// the close signal escaping the body counts as success.
    pub(crate) closing: bool,
    /// Already queued for a drop-close once: never queued again.
    pub(crate) finalized: bool,
    /// Owned by the VM's built-in scheduler.
    pub(crate) task: bool,
    /// Creation order: the order dropped coroutines are closed in.
    pub(crate) seq: u64,
    /// Suspended frames, bottom (the body) first; bases index `regs`.
    pub(crate) frames: Vec<Frame>,
    /// Their registers.
    pub(crate) regs: Vec<u64>,
    /// The register of the suspending instruction that receives the value
    /// sent by the next `resume`.
    pub(crate) resume_dst: u16,
    /// The current suspension's value (yielded value or awaitable).
    pub(crate) payload: u64,
    /// The key of the value most recently yielded (nil before the first).
    pub(crate) key: u64,
    /// PHP's generator key counter (rule 10, format 2): the largest integer
    /// key yielded so far, starting at -1 and never below it.
    pub(crate) max_key: i64,
    /// The return value (`returned`) or the error that escaped (`failed`).
    pub(crate) result: u64,
    /// The close signal while `closing`.
    pub(crate) signal: u64,
}

impl CoroObj {
    /// Bytes held beyond the object itself: the suspended stack.
    fn stack_bytes(&self) -> usize {
        self.regs.capacity() * 8 + self.frames.capacity() * FRAME_BYTES
    }
}

/// Bytes charged per suspended frame record.
pub(crate) const FRAME_BYTES: usize = core::mem::size_of::<Frame>();

/// A runtime error value (kind `error`): its code, where it was raised, and
/// its payload (`raise`'s operand; nil for an error an instruction raised).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorObj {
    pub(crate) kind: ErrorKind,
    pub(crate) func: u32,
    pub(crate) pc: u32,
    pub(crate) payload: u64,
}

impl Object {
    /// The object's dynamic kind (LSB §2.2).
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Object::Str(_) => Kind::Str,
            Object::Int(_) => Kind::Int,
            Object::Array(_) => Kind::Array,
            Object::Map(_) => Kind::Map,
            Object::Struct(_) => Kind::Object,
            Object::Func(_) => Kind::Function,
            Object::Cell(_) => Kind::Cell,
            Object::Iter(_) => Kind::Iter,
            Object::Error(_) => Kind::Error,
            Object::Coro(_) => Kind::Coroutine,
            Object::Ref(_) => Kind::Reference,
        }
    }

    /// Approximate bytes held, for the memory budget. Shared storage is split
    /// evenly between the objects sharing it.
    pub(crate) fn bytes(&self) -> usize {
        SLOT_BYTES
            + match self {
                Object::Str(b) => b.len(),
                Object::Int(_) | Object::Cell(_) | Object::Error(_) | Object::Ref(_) => 0,
                Object::Array(a) => a.items.capacity() * 8 / Arc::strong_count(&a.items).max(1),
                Object::Map(m) => m.store.bytes() / Arc::strong_count(&m.store).max(1),
                Object::Struct(s) => s.fields.len() * 8,
                Object::Func(f) => f.captures.len() * 8,
                Object::Iter(_) => core::mem::size_of::<IterObj>(),
                Object::Coro(c) => core::mem::size_of::<CoroObj>() + c.stack_bytes(),
            }
    }
}

/// Bytes of data work (copied, hashed, or compared) that cost one extra unit
/// of fuel, per instruction, rounded down: an instruction that moves less
/// costs nothing extra, so ordinary code keeps LSB's exact fuel, while one
/// that copies a megabyte pays 4,096 units. Copying 256 bytes takes about as
/// long as running a few instructions, which is what one unit pays for in a
/// short loop.
pub(crate) const WORK_BYTES_PER_FUEL: usize = 256;

/// Bytes charged per object for its slot and header.
pub(crate) const SLOT_BYTES: usize = core::mem::size_of::<Slot>() + 16;

/// The least allocation between two collections.
const MIN_GC_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct Slot {
    /// The generation (bits 0..15) and, while the slot holds a reference
    /// box, [`TAG_BOX`]: the 16 bits a reference word must carry.
    pub(crate) tag: u16,
    /// RC profile: the trial-deletion color ([`COLOR`]), [`BUFFERED`], and
    /// [`STUCK`]. Always zero in the tracing profile.
    pub(crate) flags: u8,
    /// RC profile: the reference count. Always zero in the tracing profile.
    pub(crate) rc: u32,
    pub(crate) obj: Option<Object>,
}

/// The trial-deletion color bits of [`Slot::flags`] (black is zero).
pub(crate) const COLOR: u8 = 0b11;
/// The object is in the possible-roots buffer.
pub(crate) const BUFFERED: u8 = 0b100;
/// An increment was lost at [`Heap::rc_max`]: the count is no longer
/// exact, so the object is never decremented or freed again.
pub(crate) const STUCK: u8 = 0b1000;

/// The possible-roots buffer size that starts a cycle collection (PHP's
/// default); it grows by this much after a collection that frees little, so
/// a program whose buffered roots are all live cannot make every collection
/// a wasted one.
pub(crate) const ROOT_THRESHOLD: usize = 10_000;

/// The largest the root threshold grows to.
pub(crate) const ROOT_THRESHOLD_MAX: usize = 1_000_000;

/// No destructor (an entry of [`Heap::dtors`]).
pub(crate) const NO_DTOR: u32 = u32::MAX;

/// The box flag within a slot tag (bit 47 of the word).
const TAG_BOX: u16 = (dynv::BOX_FLAG >> 32) as u16;
/// The generation within a slot tag.
const TAG_GEN: u16 = 0x7FFF;

/// Collection counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GcStats {
    /// Tracing collections, or cycle collections in the RC profile.
    pub(crate) collections: u64,
    /// Objects freed (by a sweep, a count reaching zero, or a cycle
    /// collection).
    pub(crate) freed: u64,
    /// Of those, freed by the cycle collector.
    pub(crate) cycle_freed: u64,
    /// Destructors and drop-closes started.
    pub(crate) finalized: u64,
    /// RC: references unbound by a contents copy (PHP's refcount-1 rule).
    pub(crate) unbound: u64,
}

/// The object store and collector.
#[derive(Debug)]
pub(crate) struct Heap {
    pub(crate) slots: Vec<Slot>,
    free: Vec<u32>,
    /// Slots holding coroutines (for the close-on-drop pass).
    coros: Vec<u32>,
    marks: Vec<u64>,
    work: Vec<u32>,
    /// Estimated bytes held by objects (exact recount at every collection).
    used: usize,
    since_gc: usize,
    threshold: usize,
    /// The memory budget, in bytes.
    pub(crate) limit: usize,
    pub(crate) stats: GcStats,
    /// The reference-counting profile is active (fixed when the VM is made).
    pub(crate) rc: bool,
    /// RC: words whose count reached zero, or that were just allocated,
    /// since the last settle. A stack, so what dies first is settled first
    /// and an object's children right after it, first child first (PHP's
    /// order).
    pub(crate) zero: Vec<u64>,
    /// RC: the next instruction boundary has work: the zero list, a queued
    /// finalization, or a saturated count.
    pub(crate) pending: bool,
    /// RC: possible roots of garbage cycles (objects decremented to a count
    /// above zero), each at most once ([`BUFFERED`]).
    pub(crate) roots: Vec<u64>,
    /// RC: the buffer size that starts a cycle collection.
    pub(crate) root_threshold: usize,
    /// RC: an increment was lost at `rc_max` since the last settle.
    pub(crate) saturated: bool,
    /// RC: the largest count; an increment past it sticks ([`STUCK`]).
    pub(crate) rc_max: u32,
    /// Per canonical type id, the destructor of a struct type ([`NO_DTOR`]
    /// for none); empty when no destructor is set.
    pub(crate) dtors: Vec<u32>,
    /// Pooled word list for gathering an object's children.
    scratch: Vec<u64>,
    /// Bytes of data work (copies, hashing, comparing) not yet paid for in
    /// fuel. Atomic only so the read-only lookups can add to it; it is
    /// never shared between threads, and plain loads and stores suffice.
    work_bytes: AtomicUsize,
}

impl Heap {
    /// An empty heap with a budget.
    pub(crate) fn new(limit: usize) -> Heap {
        Heap {
            slots: Vec::new(),
            free: Vec::new(),
            coros: Vec::new(),
            marks: Vec::new(),
            work: Vec::new(),
            used: 0,
            since_gc: 0,
            threshold: MIN_GC_BYTES,
            limit,
            stats: GcStats::default(),
            rc: false,
            zero: Vec::new(),
            pending: false,
            roots: Vec::new(),
            root_threshold: ROOT_THRESHOLD,
            saturated: false,
            rc_max: u32::MAX,
            dtors: Vec::new(),
            scratch: Vec::new(),
            work_bytes: AtomicUsize::new(0),
        }
    }

    /// Whether any live object is a map.
    pub(crate) fn holds_map(&self) -> bool {
        self.slots
            .iter()
            .any(|s| matches!(s.obj, Some(Object::Map(_))))
    }

    /// Records `bytes` of data work (a copy, a hash, a comparison). The
    /// dispatch loop takes it when the instruction finishes and charges
    /// fuel for it.
    #[inline]
    pub(crate) fn note_work(&self, bytes: usize) {
        let w = self.work_bytes.load(Ordering::Relaxed);
        self.work_bytes
            .store(w.saturating_add(bytes), Ordering::Relaxed);
    }

    /// Takes the data work recorded since the last take (the work of one
    /// instruction), leaving zero.
    #[inline]
    pub(crate) fn take_work(&mut self) -> usize {
        core::mem::take(self.work_bytes.get_mut())
    }

    /// Puts back work taken with [`take_work`](Heap::take_work) (a host
    /// call's own work is not the module's to pay for).
    pub(crate) fn restore_work(&mut self, bytes: usize) {
        *self.work_bytes.get_mut() = bytes;
    }

    /// Forgets recorded work (the start of a run).
    pub(crate) fn clear_work(&mut self) {
        *self.work_bytes.get_mut() = 0;
    }

    /// Bytes currently charged.
    pub(crate) fn used(&self) -> usize {
        self.used
    }

    /// Live objects (as of the last collection, plus allocations since).
    pub(crate) fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// The object a reference word names, if it is live.
    #[inline]
    pub(crate) fn get(&self, v: u64) -> Option<&Object> {
        if !dynv::is_ref(v) {
            return None;
        }
        let (index, tag) = dynv::ref_parts(v);
        let slot = self.slots.get(index as usize)?;
        if slot.tag != tag {
            return None;
        }
        slot.obj.as_ref()
    }

    /// Mutable access to the object a reference word names.
    #[inline]
    pub(crate) fn get_mut(&mut self, v: u64) -> Option<&mut Object> {
        if !dynv::is_ref(v) {
            return None;
        }
        let (index, tag) = dynv::ref_parts(v);
        let slot = self.slots.get_mut(index as usize)?;
        if slot.tag != tag {
            return None;
        }
        slot.obj.as_mut()
    }

    /// The value a slot word reads as (LSB §5.17: reference slots are
    /// transparent): a box's value, anything else itself. A stale box reads
    /// as nil.
    #[inline(always)]
    pub(crate) fn deref(&self, v: u64) -> u64 {
        if dynv::is_box(v) {
            self.box_value(v).unwrap_or(dynv::NIL)
        } else {
            v
        }
    }

    /// The value held by the box `v` names.
    #[inline]
    pub(crate) fn box_value(&self, v: u64) -> Option<u64> {
        match self.get(v)? {
            Object::Ref(x) => Some(*x),
            _ => None,
        }
    }

    /// Writes into the box `v` names (the caller has dereferenced `value`,
    /// so a box never holds a box). Returns whether `v` is a live box.
    #[inline]
    pub(crate) fn set_box(&mut self, v: u64, value: u64) -> bool {
        let old = match self.get_mut(v) {
            Some(Object::Ref(x)) => core::mem::replace(x, value),
            _ => return false,
        };
        self.swap(value, old);
        true
    }

    /// Whether `v` is an array or map that another container's contents
    /// may hold (LSB §5.16 `aliased`; a frozen constant part always is), so
    /// a nested write must separate it.
    ///
    /// Both memory profiles answer with the bit. LSB §5.16 would let the RC
    /// profile answer "the count is above one" instead, and that makes the
    /// same copies for code that keeps the value discipline, but not for
    /// code that stores a container without `dup` (the differential tests
    /// generate such code, and found a case): with the bit, both profiles
    /// separate alike on every program. A count would also see a dead
    /// temporary register as a second holder and copy on every pass of a
    /// loop.
    #[inline]
    pub(crate) fn is_aliased(&self, v: u64) -> bool {
        match self.get(v) {
            Some(Object::Array(a)) => a.aliased || a.frozen,
            Some(Object::Map(m)) => m.aliased || m.frozen,
            _ => false,
        }
    }

    /// Marks the arrays and maps among `words` as aliased: they were in
    /// contents that have just been copied, so two containers now hold them.
    pub(crate) fn mark_aliased(&mut self, words: impl IntoIterator<Item = u64>) {
        for w in words {
            if !dynv::is_ref(w) || dynv::is_box(w) {
                continue;
            }
            match self.get_mut(w) {
                Some(Object::Array(a)) => a.aliased = true,
                Some(Object::Map(m)) => m.aliased = true,
                _ => {}
            }
        }
    }

    /// The bytes of a string object.
    #[inline]
    pub(crate) fn str(&self, v: u64) -> Option<&[u8]> {
        match self.get(v)? {
            Object::Str(b) => Some(b),
            _ => None,
        }
    }

    /// The dynamic kind of a `dyn`/`str`/`ref` word (`Nil` for a stale
    /// reference).
    #[inline]
    pub(crate) fn kind(&self, v: u64) -> Kind {
        match dynv::decode(v) {
            dynv::Raw::Nil => Kind::Nil,
            dynv::Raw::Bool(_) => Kind::Bool,
            dynv::Raw::Int(_) => Kind::Int,
            dynv::Raw::Float(_) => Kind::Float,
            dynv::Raw::Char(_) => Kind::Char,
            dynv::Raw::Ref(..) => self.get(v).map_or(Kind::Nil, Object::kind),
        }
    }

    /// Whether a collection should run before an allocation of about `extra`
    /// bytes. Collections are rationed: an ordinary one needs as much new
    /// allocation as survived the last; one forced by the budget needs an
    /// eighth of the budget, so a program living at its limit cannot make
    /// every safepoint collect.
    #[inline]
    pub(crate) fn wants_gc(&self, extra: usize) -> bool {
        self.since_gc >= self.threshold
            || (self.used.saturating_add(extra) > self.limit && self.since_gc >= self.limit / 8)
    }

    /// Charges `bytes` against the budget.
    #[inline]
    pub(crate) fn charge(&mut self, bytes: usize) -> Result<(), Fault> {
        let used = self.used.saturating_add(bytes);
        if used > self.limit {
            return Err(Fault::Trap(ErrorKind::OutOfMemory));
        }
        self.used = used;
        self.since_gc = self.since_gc.saturating_add(bytes);
        Ok(())
    }

    /// Whether `bytes` more would fit in the budget (no charge).
    #[inline]
    pub(crate) fn fits(&self, bytes: usize) -> bool {
        self.used.saturating_add(bytes) <= self.limit
    }

    /// Stores an object, returning its reference word, or the `OutOfMemory`
    /// trap when the budget is spent.
    ///
    /// In the RC profile the new object starts at count zero on the zero list
    /// (freed at the next boundary unless something stores it first), and it
    /// holds a count on each child it does not share: the words of fresh
    /// array or map storage, a cell's or a box's value, an iterator's source
    /// and key, an error's payload. Structs, closures, and coroutines need the
    /// program to know which of their slots are references: their creators
    /// call [`adopt`](Heap::adopt).
    pub(crate) fn alloc(&mut self, obj: Object) -> Result<u64, Fault> {
        self.charge(obj.bytes())?;
        let coro = matches!(obj, Object::Coro(_));
        let flag = if matches!(obj, Object::Ref(_)) {
            TAG_BOX
        } else {
            0
        };
        let reused = self
            .free
            .pop()
            .and_then(|i| self.slots.get_mut(i as usize).map(|s| (i, s)));
        let (index, word) = if let Some((index, slot)) = reused {
            slot.obj = Some(obj);
            slot.tag = (slot.tag & TAG_GEN) | flag;
            // Always zero in the tracing profile; RC freeing clears them.
            (index, dynv::from_ref(index, slot.tag))
        } else {
            let Ok(index) = u32::try_from(self.slots.len()) else {
                return Err(Fault::Trap(ErrorKind::OutOfMemory));
            };
            self.slots.push(Slot {
                tag: flag,
                flags: 0,
                rc: 0,
                obj: Some(obj),
            });
            (index, dynv::from_ref(index, flag))
        };
        if self.rc {
            self.zero.push(word);
            self.pending = true;
            self.adopt_plain(index);
        } else if coro {
            self.coros.push(index);
        }
        Ok(word)
    }

    /// A new PHP reference box holding `value` (already dereferenced).
    pub(crate) fn alloc_box(&mut self, value: u64) -> Result<u64, Fault> {
        self.alloc(Object::Ref(value))
    }

    /// The coroutine a word names.
    #[inline]
    pub(crate) fn coro(&self, v: u64) -> Option<&CoroObj> {
        match self.get(v)? {
            Object::Coro(c) => Some(c),
            _ => None,
        }
    }

    /// Mutable access to the coroutine a word names.
    #[inline]
    pub(crate) fn coro_mut(&mut self, v: u64) -> Option<&mut CoroObj> {
        match self.get_mut(v)? {
            Object::Coro(c) => Some(c),
            _ => None,
        }
    }

    /// Allocates a string.
    pub(crate) fn alloc_str(&mut self, bytes: &[u8]) -> Result<u64, Fault> {
        if !self.fits(bytes.len().saturating_add(SLOT_BYTES)) {
            return Err(Fault::Trap(ErrorKind::OutOfMemory));
        }
        self.alloc(Object::Str(bytes.into()))
    }

    // -----------------------------------------------------------------------
    // Reference counting (the RC profile)
    // -----------------------------------------------------------------------

    /// The count of the live object `v` names (`None` for anything else).
    #[inline]
    pub(crate) fn count(&self, v: u64) -> Option<u32> {
        if !dynv::is_ref(v) {
            return None;
        }
        let (index, tag) = dynv::ref_parts(v);
        let slot = self.slots.get(index as usize)?;
        (slot.tag == tag && slot.obj.is_some()).then_some(slot.rc)
    }

    /// One more holder of `v`. Nothing in the tracing profile, or for a word
    /// that names no live object (a stale word was never counted).
    #[inline]
    pub(crate) fn inc(&mut self, v: u64) {
        if self.rc && dynv::is_ref(v) {
            self.inc_live(v);
        }
    }

    fn inc_live(&mut self, v: u64) {
        let (index, tag) = dynv::ref_parts(v);
        let max = self.rc_max;
        let Some(slot) = self.slots.get_mut(index as usize) else {
            return;
        };
        if slot.tag != tag || slot.obj.is_none() || slot.flags & STUCK != 0 {
            return;
        }
        if slot.rc >= max {
            // The count can no longer be exact: the object is never freed,
            // and the run traps at the next boundary.
            slot.flags |= STUCK;
            self.saturated = true;
            self.pending = true;
        } else {
            slot.rc += 1;
        }
    }

    /// One holder of `v` fewer. At zero the word goes on the zero list (the
    /// object is settled at the next boundary); above zero a container-like
    /// object becomes a possible root of a garbage cycle.
    #[inline]
    pub(crate) fn dec(&mut self, v: u64) {
        if self.rc && dynv::is_ref(v) {
            self.dec_live(v);
        }
    }

    fn dec_live(&mut self, v: u64) {
        let (index, tag) = dynv::ref_parts(v);
        let Some(slot) = self.slots.get_mut(index as usize) else {
            return;
        };
        if slot.tag != tag || slot.flags & STUCK != 0 {
            return;
        }
        let Some(obj) = &slot.obj else {
            return;
        };
        if slot.rc <= 1 {
            slot.rc = 0;
            self.zero.push(v);
            self.pending = true;
        } else {
            slot.rc -= 1;
            // Strings and boxed ints have no children: never in a cycle.
            if slot.flags & BUFFERED == 0 && !matches!(obj, Object::Str(_) | Object::Int(_)) {
                slot.flags |= BUFFERED;
                self.roots.push(v);
            }
        }
    }

    /// A counted location changed from `old` to `new`. One predictable
    /// branch in the tracing profile, wherever it is inlined.
    #[inline(always)]
    pub(crate) fn swap(&mut self, new: u64, old: u64) {
        if self.rc && new != old {
            self.swap_counted(new, old);
        }
    }

    #[inline(never)]
    fn swap_counted(&mut self, new: u64, old: u64) {
        self.inc(new);
        self.dec(old);
    }

    /// The counts a suspending coroutine's fields move (RC): its payload,
    /// key (new, old), and the close signal it gives up. Out of line, so the
    /// switch path stays small enough to inline.
    #[inline(never)]
    pub(crate) fn suspend_counts(
        &mut self,
        payload: (u64, u64),
        key: Option<(u64, u64)>,
        old_signal: u64,
    ) {
        self.swap(payload.0, payload.1);
        if let Some((k, old)) = key {
            self.swap(k, old);
        }
        self.dec(old_signal);
    }

    /// Counts the children of a just-made object that need the program to
    /// find (struct reference fields, closure captures, a coroutine's
    /// reference registers and closures); [`alloc`](Heap::alloc) counted
    /// the others.
    pub(crate) fn adopt(&mut self, v: u64, prog: &Program) {
        if !self.rc {
            return;
        }
        let mut kids = core::mem::take(&mut self.scratch);
        kids.clear();
        if let Some(obj @ (Object::Struct(_) | Object::Func(_) | Object::Coro(_))) = self.get(v) {
            owned_children(obj, prog, &mut kids);
        }
        for &w in &kids {
            self.inc(w);
        }
        self.scratch = kids;
    }

    /// [`alloc`](Heap::alloc)'s half of adoption.
    fn adopt_plain(&mut self, index: u32) {
        let mut kids = core::mem::take(&mut self.scratch);
        kids.clear();
        if let Some(obj) = self.slots.get(index as usize).and_then(|s| s.obj.as_ref()) {
            plain_children(obj, &mut kids);
        }
        for &w in &kids {
            self.inc(w);
        }
        self.scratch = kids;
    }

    /// What the zero list's word `v` needs: nothing (it is held again, or
    /// already gone), freeing, its destructor, or its drop-close.
    pub(crate) fn dead(&self, v: u64) -> Dead {
        if !dynv::is_ref(v) {
            return Dead::Alive;
        }
        let (index, tag) = dynv::ref_parts(v);
        let Some(slot) = self.slots.get(index as usize) else {
            return Dead::Alive;
        };
        if slot.tag != tag || slot.rc != 0 || slot.flags & STUCK != 0 {
            return Dead::Alive;
        }
        match &slot.obj {
            None => Dead::Alive,
            Some(Object::Struct(s)) if !s.destructed => match self.dtor_of(s.ty) {
                Some(f) => Dead::Destruct(f),
                None => Dead::Free,
            },
            Some(Object::Coro(c))
                if !c.finalized && matches!(c.state, CoroState::Yielded | CoroState::Awaiting) =>
            {
                Dead::Close
            }
            Some(_) => Dead::Free,
        }
    }

    /// Frees the object `v` names, whose count is zero, and releases its
    /// children (each may join the zero list): RC freeing. Children are
    /// released last first, so the zero list settles the first child first.
    pub(crate) fn free_counted(&mut self, v: u64, prog: &Program) {
        let (index, tag) = dynv::ref_parts(v);
        if self.slots.get(index as usize).is_none_or(|s| s.tag != tag) {
            return;
        }
        let Some(obj) = self.vacate(index as usize) else {
            return;
        };
        let mut kids = core::mem::take(&mut self.scratch);
        kids.clear();
        // Gathered while this object still holds its storage, so "the last
        // holder" is decided before the `Arc` is dropped.
        owned_children(&obj, prog, &mut kids);
        drop(obj);
        for &w in kids.iter().rev() {
            self.dec(w);
        }
        self.scratch = kids;
    }

    /// Frees the object in slot `index` without releasing its children: the
    /// cycle collector already removed the counts its references held.
    pub(crate) fn free_uncounted(&mut self, index: usize) {
        let _obj = self.vacate(index);
    }

    /// Empties a slot (RC freeing): refunds the object's bytes, advances
    /// the generation (or retires the slot), and returns the object.
    fn vacate(&mut self, index: usize) -> Option<Object> {
        let slot = self.slots.get_mut(index)?;
        let obj = slot.obj.take()?;
        slot.rc = 0;
        slot.flags = 0;
        let generation = slot.tag & TAG_GEN;
        if generation == TAG_GEN {
            slot.tag = generation;
        } else {
            slot.tag = generation + 1;
            self.free.push(index as u32);
        }
        self.used = self.used.saturating_sub(freed_bytes(&obj));
        self.stats.freed += 1;
        Some(obj)
    }

    /// Recounts the bytes held (RC): frees refund only what is certainly
    /// theirs (a `dup` is charged its share of storage it does not own), so
    /// between recounts the estimate can only be high, never low. A recount
    /// makes it exact; recounts are rationed as tracing collections are
    /// (after as much was charged as was held at the last one), so their
    /// total cost stays proportional to allocation.
    pub(crate) fn recount(&mut self) {
        self.used = self
            .slots
            .iter()
            .filter_map(|s| s.obj.as_ref())
            .map(Object::bytes)
            .sum();
        self.since_gc = 0;
        self.threshold = self.used.max(MIN_GC_BYTES);
    }

    /// Whether a recount is due (RC).
    #[inline]
    pub(crate) fn recount_due(&self) -> bool {
        self.since_gc >= self.threshold || self.pressure(0)
    }

    /// Whether the collector should look before an allocation of about
    /// `extra` bytes (RC): the possible-roots buffer reached its threshold,
    /// a recount is due, or the budget is under pressure (rationed as for
    /// the tracing collector).
    #[inline]
    pub(crate) fn wants_cycles(&self, extra: usize) -> bool {
        self.roots.len() >= self.root_threshold
            || self.since_gc >= self.threshold
            || self.pressure(extra)
    }

    /// The budget would be exceeded by `extra` more bytes, and enough was
    /// allocated since the last collection or recount to look again.
    #[inline]
    pub(crate) fn pressure(&self, extra: usize) -> bool {
        self.used.saturating_add(extra) > self.limit && self.since_gc >= self.limit / 8
    }

    /// Marks everything reachable from `roots` and frees the rest, except
    /// suspended coroutines found unreachable for the first time: those are
    /// resurrected and appended to `dropped`, oldest first, for the VM to
    /// close (see the module docs).
    pub(crate) fn collect(
        &mut self,
        roots: impl IntoIterator<Item = u64>,
        prog: &Program,
        dropped: &mut Vec<u64>,
    ) {
        let words = self.slots.len().div_ceil(64);
        self.marks.clear();
        self.marks.resize(words, 0);
        let mut work = core::mem::take(&mut self.work);
        work.clear();
        for root in roots {
            self.mark(root, &mut work);
        }
        self.trace(&mut work, prog);
        // Close on drop: suspended coroutines the roots do not reach are kept,
        // with everything they reach, and handed to the VM in creation order,
        // so the order they are closed in is defined.
        let mut found: Vec<(u64, u64)> = Vec::new();
        for &i in &self.coros {
            if self.is_marked(i as usize) {
                continue;
            }
            if let Some(Slot {
                tag,
                obj: Some(Object::Coro(c)),
                ..
            }) = self.slots.get(i as usize)
            {
                let suspended = matches!(c.state, CoroState::Yielded | CoroState::Awaiting);
                if suspended && !c.finalized {
                    found.push((c.seq, dynv::from_ref(i, *tag)));
                }
            }
        }
        found.sort_unstable();
        // Objects with a destructor: after the coroutines, in heap order. A
        // scan of the slots, so only when some type has a destructor.
        if !self.dtors.is_empty() {
            for (i, slot) in self.slots.iter().enumerate() {
                if let Some(Object::Struct(s)) = &slot.obj {
                    if !s.destructed && self.dtor_of(s.ty).is_some() && !self.is_marked(i) {
                        found.push((u64::MAX, dynv::from_ref(i as u32, slot.tag)));
                    }
                }
            }
        }
        if !found.is_empty() {
            for &(_, word) in &found {
                match self.get_mut(word) {
                    Some(Object::Coro(c)) => c.finalized = true,
                    Some(Object::Struct(s)) => s.destructed = true,
                    _ => {}
                }
                self.mark(word, &mut work);
                dropped.push(word);
            }
            self.trace(&mut work, prog);
        }
        self.work = work;
        self.sweep();
    }

    /// The destructor of a struct type, if one is set.
    #[inline]
    pub(crate) fn dtor_of(&self, ty: u32) -> Option<u32> {
        self.dtors
            .get(ty as usize)
            .copied()
            .filter(|&f| f != NO_DTOR)
    }

    /// The live objects `roots` do not reach (debug builds; the RC
    /// profile's "nothing leaks" check after a full collection). Marks
    /// without sweeping.
    #[cfg(debug_assertions)]
    pub(crate) fn unreached(&mut self, roots: &[u64], prog: &Program) -> Vec<usize> {
        self.marks.clear();
        self.marks.resize(self.slots.len().div_ceil(64), 0);
        let mut work = core::mem::take(&mut self.work);
        work.clear();
        for &root in roots {
            self.mark(root, &mut work);
        }
        self.trace(&mut work, prog);
        self.work = work;
        (0..self.slots.len())
            .filter(|&i| self.slots[i].obj.is_some() && !self.is_marked(i))
            .collect()
    }

    /// Whether slot `i` is marked.
    #[inline]
    fn is_marked(&self, i: usize) -> bool {
        self.marks
            .get(i >> 6)
            .is_some_and(|w| (w >> (i & 63)) & 1 == 1)
    }

    /// Drains the work list: marks every child of every queued object.
    fn trace(&mut self, work: &mut Vec<u32>, prog: &Program) {
        while let Some(index) = work.pop() {
            // Children are gathered first (shared borrow), then marked.
            let mut children: [u64; 2] = [0; 2];
            let mut extra: Option<(usize, usize)> = None;
            match self.slots.get(index as usize).and_then(|s| s.obj.as_ref()) {
                None | Some(Object::Str(_) | Object::Int(_)) => {}
                Some(Object::Error(e)) => children[0] = e.payload,
                Some(Object::Ref(v)) => children[0] = *v,
                Some(Object::Cell(c)) => {
                    if c.elem.is_reference() {
                        children[0] = c.value;
                    }
                }
                Some(Object::Iter(it)) => {
                    children[0] = it.src;
                    if it.key_ty.is_reference() {
                        children[1] = it.key.unwrap_or(0);
                    }
                }
                Some(_) => extra = Some((index as usize, 0)),
            }
            for child in children {
                self.mark(child, work);
            }
            if let Some((i, _)) = extra {
                self.trace_container(i, prog, work);
            }
        }
    }

    /// Frees every unmarked object and recounts the bytes held.
    fn sweep(&mut self) {
        let mut used = 0usize;
        let mut freed = 0u64;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let Some(obj) = slot.obj.as_ref() else {
                continue;
            };
            let marked = self
                .marks
                .get(i >> 6)
                .is_some_and(|w| (w >> (i & 63)) & 1 == 1);
            if marked {
                used += obj.bytes();
                continue;
            }
            slot.obj = None;
            freed += 1;
            let generation = slot.tag & TAG_GEN;
            if generation == TAG_GEN {
                // Retired: never reused, so no stale reference can alias.
                slot.tag = generation;
                continue;
            }
            slot.tag = generation + 1;
            self.free.push(i as u32);
        }
        self.used = used;
        self.since_gc = 0;
        self.threshold = used.max(MIN_GC_BYTES);
        self.stats.collections += 1;
        self.stats.freed += freed;
        // Forget freed coroutine slots before any can be reused.
        let slots = &self.slots;
        self.coros.retain(|&i| {
            matches!(
                slots.get(i as usize).and_then(|s| s.obj.as_ref()),
                Some(Object::Coro(_))
            )
        });
    }

    /// Marks the live object `v` names, queueing it for tracing.
    #[inline]
    fn mark(&mut self, v: u64, work: &mut Vec<u32>) {
        if !dynv::is_ref(v) {
            return;
        }
        let (index, tag) = dynv::ref_parts(v);
        let i = index as usize;
        let live = self
            .slots
            .get(i)
            .is_some_and(|s| s.tag == tag && s.obj.is_some());
        if !live {
            return;
        }
        if let Some(w) = self.marks.get_mut(i >> 6) {
            let bit = 1u64 << (i & 63);
            if *w & bit == 0 {
                *w |= bit;
                work.push(index);
            }
        }
    }

    /// Traces an array, map, struct, or closure, whose children are many.
    fn trace_container(&mut self, i: usize, prog: &Program, work: &mut Vec<u32>) {
        // Take the object out so its children can be marked while iterating
        // it; it goes back unchanged.
        let Some(obj) = self.slots.get_mut(i).and_then(|s| s.obj.take()) else {
            return;
        };
        match &obj {
            Object::Array(a) if a.elem.is_reference() => {
                for &v in a.items.iter() {
                    self.mark(v, work);
                }
            }
            Object::Map(m) => {
                let (k, v) = (m.key.is_reference(), m.value.is_reference());
                if k || v {
                    for e in m.store.entries().iter().filter(|e| e.live) {
                        if k {
                            self.mark(e.key, work);
                        }
                        if v {
                            self.mark(e.value, work);
                        }
                    }
                }
            }
            Object::Struct(s) => {
                for &f in prog.struct_ref_fields(s.ty) {
                    if let Some(&v) = s.fields.get(usize::from(f)) {
                        self.mark(v, work);
                    }
                }
            }
            Object::Func(f) => {
                if let Callable::Func(id) = f.target {
                    for (ty, &v) in prog.capture_types(id).iter().zip(f.captures.iter()) {
                        if ty.is_reference() {
                            self.mark(v, work);
                        }
                    }
                }
            }
            Object::Coro(c) => {
                for v in [c.payload, c.key, c.result, c.signal] {
                    self.mark(v, work);
                }
                // Suspended frames: exactly the registers whose declared types
                // are references, as for active frames (LSB §1.4), and each
                // frame's running closure.
                for f in &c.frames {
                    self.mark(f.closure, work);
                    let refs = prog.func(f.func).map_or(&[][..], |i| &i.ref_regs[..]);
                    for &r in refs {
                        if let Some(&v) = c.regs.get(f.base + usize::from(r)) {
                            self.mark(v, work);
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(slot) = self.slots.get_mut(i) {
            slot.obj = Some(obj);
        }
    }
}

/// What a word on the zero list needs ([`Heap::dead`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dead {
    /// Held again, or already freed: nothing.
    Alive,
    /// Free it.
    Free,
    /// Run this destructor on it first.
    Destruct(u32),
    /// Close it first (a suspended coroutine, LSB §5.13 rule 13).
    Close,
}

/// The children an object holds counts on that need no program: the words
/// of array or map storage it alone holds, a cell's or box's value, an
/// iterator's source and key, an error's payload.
pub(crate) fn plain_children(obj: &Object, out: &mut Vec<u64>) {
    match obj {
        Object::Array(a) => {
            if a.elem.is_reference() && Arc::strong_count(&a.items) == 1 {
                out.extend(a.items.iter().copied());
            }
        }
        Object::Map(m) => {
            if Arc::strong_count(&m.store) == 1 {
                map_words(m, out);
            }
        }
        Object::Cell(c) => {
            if c.elem.is_reference() {
                out.push(c.value);
            }
        }
        Object::Ref(v) => out.push(*v),
        Object::Iter(it) => {
            out.push(it.src);
            if it.key_ty.is_reference() {
                if let Some(k) = it.key {
                    out.push(k);
                }
            }
        }
        Object::Error(e) => out.push(e.payload),
        Object::Str(_) | Object::Int(_) | Object::Struct(_) | Object::Func(_) | Object::Coro(_) => {
        }
    }
}

/// The reference words of a map's live entries (keys, then values, per
/// entry, as their types say).
pub(crate) fn map_words(m: &MapObj, out: &mut Vec<u64>) {
    let (k, v) = (m.key.is_reference(), m.value.is_reference());
    if !k && !v {
        return;
    }
    for e in m.store.entries().iter().filter(|e| e.live) {
        if k {
            out.push(e.key);
        }
        if v {
            out.push(e.value);
        }
    }
}

/// Every child an object holds a count on (RC): the edges of the tracing
/// collector, with array and map storage counted only by its last holder.
pub(crate) fn owned_children(obj: &Object, prog: &Program, out: &mut Vec<u64>) {
    match obj {
        Object::Struct(s) => {
            for &f in prog.struct_ref_fields(s.ty) {
                if let Some(&v) = s.fields.get(usize::from(f)) {
                    out.push(v);
                }
            }
        }
        Object::Func(f) => {
            if let Callable::Func(id) = f.target {
                for (ty, &v) in prog.capture_types(id).iter().zip(f.captures.iter()) {
                    if ty.is_reference() {
                        out.push(v);
                    }
                }
            }
        }
        Object::Coro(c) => coro_children(c, prog, out),
        other => plain_children(other, out),
    }
}

/// A coroutine's children: its payload, key, result, and close signal,
/// and every suspended frame's closure and reference registers.
pub(crate) fn coro_children(c: &CoroObj, prog: &Program, out: &mut Vec<u64>) {
    out.extend([c.payload, c.key, c.result, c.signal]);
    for f in &c.frames {
        out.push(f.closure);
        let refs = prog.func(f.func).map_or(&[][..], |i| &i.ref_regs[..]);
        for &r in refs {
            if let Some(&v) = c.regs.get(f.base + usize::from(r)) {
                out.push(v);
            }
        }
    }
}

/// The bytes a freed object certainly held: its own, and its storage's
/// only when it was the last holder (so the estimate never drops below
/// what is held).
fn freed_bytes(obj: &Object) -> usize {
    SLOT_BYTES
        + match obj {
            Object::Str(b) => b.len(),
            Object::Int(_) | Object::Cell(_) | Object::Error(_) | Object::Ref(_) => 0,
            Object::Array(a) => {
                if Arc::strong_count(&a.items) == 1 {
                    a.items.capacity() * 8
                } else {
                    0
                }
            }
            Object::Map(m) => {
                if Arc::strong_count(&m.store) == 1 {
                    m.store.bytes()
                } else {
                    0
                }
            }
            Object::Struct(s) => s.fields.len() * 8,
            Object::Func(f) => f.captures.len() * 8,
            Object::Iter(_) => core::mem::size_of::<IterObj>(),
            Object::Coro(c) => core::mem::size_of::<CoroObj>() + c.stack_bytes(),
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Host;

    fn program() -> Program {
        let module = bytecode_lang::ModuleBuilder::new()
            .finish()
            .unwrap_or_default();
        match Program::load(module, &Host::new()) {
            Ok(p) => p,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn test_a_slot_is_retired_after_32767_reuses_and_stale_words_never_alias() {
        let p = program();
        let mut heap = Heap::new(usize::MAX);
        let mut dropped = Vec::new();
        let first = heap.alloc(Object::Int(0)).unwrap_or(0);
        let mut last = first;
        // Free and reallocate the same slot until its generation is spent.
        for i in 1..=i64::from(TAG_GEN) {
            heap.collect(core::iter::empty(), &p, &mut dropped);
            last = heap.alloc(Object::Int(i)).unwrap_or(0);
            assert_eq!(dynv::ref_parts(last).0, 0, "reuse {i}");
        }
        assert_eq!(dynv::ref_parts(last).1, TAG_GEN);
        // Every earlier word is stale: it reads as nothing.
        assert!(heap.get(first).is_none());
        // The next free of slot 0 retires it: a new object takes slot 1.
        heap.collect(core::iter::empty(), &p, &mut dropped);
        let next = heap.alloc(Object::Int(-1)).unwrap_or(0);
        assert_eq!(dynv::ref_parts(next).0, 1);
        assert!(heap.get(last).is_none());
    }

    #[test]
    fn test_a_box_word_carries_the_flag_and_a_plain_word_for_its_slot_is_stale() {
        let p = program();
        let mut heap = Heap::new(usize::MAX);
        let mut dropped = Vec::new();
        let plain = heap.alloc(Object::Int(7)).unwrap_or(0);
        assert!(!dynv::is_box(plain));
        heap.collect(core::iter::empty(), &p, &mut dropped);
        let b = heap.alloc_box(dynv::from_bool(true)).unwrap_or(0);
        assert!(dynv::is_box(b));
        assert_eq!(dynv::ref_parts(b).0, dynv::ref_parts(plain).0);
        assert_eq!(heap.box_value(b), Some(dynv::from_bool(true)));
        // The old word names the same slot without the flag: stale.
        assert!(heap.get(plain).is_none());
        assert_eq!(heap.deref(b), dynv::from_bool(true));
        assert_eq!(heap.deref(plain), plain);
        assert_eq!(heap.kind(b), bytecode_lang::Kind::Reference);
    }
}
