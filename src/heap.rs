//! The VM heap: every object a program allocates, and the tracing collector
//! that reclaims them.
//!
//! Objects live in a slot vector addressed by the 32-bit index inside a
//! reference word (see [`dynv`](crate::dynv)); each slot carries a 16-bit
//! generation that the reference must match. A freed slot's generation
//! advances, so a stale reference (only producible by a module a verifier
//! would reject, or by a host holding a value across runs) reads as `nil`
//! instead of aliasing a newer object. A slot whose generation would wrap is
//! retired rather than reused, so aliasing is impossible, not merely unlikely.
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
//! collection that finds it unreachable again frees it.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

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
    pub(crate) items: Arc<Vec<u64>>,
}

/// A map, with the same sharing as [`ArrayObj`].
#[derive(Debug)]
pub(crate) struct MapObj {
    pub(crate) key: ValType,
    pub(crate) value: ValType,
    pub(crate) frozen: bool,
    pub(crate) store: Arc<MapStore>,
}

/// A struct instance: its struct type and its fields (the parent's first).
#[derive(Debug)]
pub(crate) struct StructObj {
    pub(crate) ty: u32,
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
    /// The largest integer key yielded so far (rule 10).
    pub(crate) max_key: Option<i64>,
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

/// A runtime error value (kind `error`): its code and where it was raised.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorObj {
    pub(crate) kind: ErrorKind,
    pub(crate) func: u32,
    pub(crate) pc: u32,
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
        }
    }

    /// Approximate bytes held, for the memory budget. Shared storage is split
    /// evenly between the objects sharing it.
    fn bytes(&self) -> usize {
        SLOT_BYTES
            + match self {
                Object::Str(b) => b.len(),
                Object::Int(_) | Object::Cell(_) | Object::Error(_) => 0,
                Object::Array(a) => a.items.capacity() * 8 / Arc::strong_count(&a.items).max(1),
                Object::Map(m) => m.store.bytes() / Arc::strong_count(&m.store).max(1),
                Object::Struct(s) => s.fields.len() * 8,
                Object::Func(f) => f.captures.len() * 8,
                Object::Iter(_) => core::mem::size_of::<IterObj>(),
                Object::Coro(c) => core::mem::size_of::<CoroObj>() + c.stack_bytes(),
            }
    }
}

/// Bytes charged per object for its slot and header.
pub(crate) const SLOT_BYTES: usize = core::mem::size_of::<Slot>() + 16;

/// The least allocation between two collections.
const MIN_GC_BYTES: usize = 256 * 1024;

#[derive(Debug)]
struct Slot {
    generation: u16,
    obj: Option<Object>,
}

/// Collection counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GcStats {
    pub(crate) collections: u64,
    pub(crate) freed: u64,
}

/// The object store and collector.
#[derive(Debug)]
pub(crate) struct Heap {
    slots: Vec<Slot>,
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
        }
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
        let (index, generation) = dynv::ref_parts(v);
        let slot = self.slots.get(index as usize)?;
        if slot.generation != generation {
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
        let (index, generation) = dynv::ref_parts(v);
        let slot = self.slots.get_mut(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        slot.obj.as_mut()
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
    pub(crate) fn alloc(&mut self, obj: Object) -> Result<u64, Fault> {
        self.charge(obj.bytes())?;
        let coro = matches!(obj, Object::Coro(_));
        if let Some(index) = self.free.pop() {
            if let Some(slot) = self.slots.get_mut(index as usize) {
                slot.obj = Some(obj);
                if coro {
                    self.coros.push(index);
                }
                return Ok(dynv::from_ref(index, slot.generation));
            }
        }
        let Ok(index) = u32::try_from(self.slots.len()) else {
            return Err(Fault::Trap(ErrorKind::OutOfMemory));
        };
        self.slots.push(Slot {
            generation: 0,
            obj: Some(obj),
        });
        if coro {
            self.coros.push(index);
        }
        Ok(dynv::from_ref(index, 0))
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
                generation,
                obj: Some(Object::Coro(c)),
            }) = self.slots.get(i as usize)
            {
                let suspended = matches!(c.state, CoroState::Yielded | CoroState::Awaiting);
                if suspended && !c.finalized {
                    found.push((c.seq, dynv::from_ref(i, *generation)));
                }
            }
        }
        if !found.is_empty() {
            found.sort_unstable();
            for &(_, word) in &found {
                if let Some(c) = self.coro_mut(word) {
                    c.finalized = true;
                }
                self.mark(word, &mut work);
                dropped.push(word);
            }
            self.trace(&mut work, prog);
        }
        self.work = work;
        self.sweep();
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
                None | Some(Object::Str(_) | Object::Int(_) | Object::Error(_)) => {}
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
            if slot.generation == u16::MAX {
                // Retired: never reused, so no stale reference can alias.
                continue;
            }
            slot.generation += 1;
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
        let (index, generation) = dynv::ref_parts(v);
        let i = index as usize;
        let live = self
            .slots
            .get(i)
            .is_some_and(|s| s.generation == generation && s.obj.is_some());
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
