//! The reference-counting memory profile (DECISIONS D22) at the machine
//! level: settling the zero list at instruction boundaries, the counts the
//! machine itself holds (frames, running and queued coroutines, the
//! scheduler, values handed to the host), cycle collections, and, in debug
//! builds, a recount of every count.
//!
//! **Timing.** A count reaching zero puts the word on the zero list
//! ([`Heap::dec`]); the dispatch loop settles the list before it executes
//! the next instruction ([`Machine::settle`]). An object that needs code
//! before it goes gets it then, on top of the current frames, and the
//! interrupted instruction runs when that code returns: a struct with a
//! destructor has it called (PHP's `__destruct` at the last reference), a
//! suspended coroutine is closed (LSB §5.13 rule 13 for `rc` runtimes).
//! Everything else is freed, and its children are released, which may
//! settle them in turn (the list is the work list, so freeing is iterative
//! and the first child is settled first, as PHP destroys an array's
//! elements in order). Outside a run, and when a run ends abnormally, no
//! code can run: such objects are queued, held by the queue, and their code
//! runs at the next run's first boundary or on [`Vm::run_finalizers`].
//!
//! Errors escaping a destructor or a drop-close are discarded (a drop-close
//! has no closer, LSB §5.13 rule 13; destructors follow the same rule, see
//! the release notes on how this differs from PHP). Traps end the run as
//! anywhere else. Code started here is charged fuel like any code, and its
//! frames count against the depth and register limits; one the limits
//! cannot hold is skipped and the object freed.
//!
//! [`Vm::run_finalizers`]: crate::Vm::run_finalizers

use alloc::vec::Vec;

use bytecode_lang::CoroState;

use crate::dynv;
use crate::heap::{Dead, Object};
use crate::machine::{Cont, Driver, Frame, Machine};
use crate::program::Program;

/// Nodes the cycle collector visits per unit of fuel it charges.
pub(crate) const VISITS_PER_FUEL: usize = 64;

/// What settling started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Settled {
    /// Nothing is left to settle: execute the instruction.
    Done,
    /// A destructor frame was pushed: continue in it.
    Call,
    /// A dropped coroutine was entered to be closed: raise the drop signal
    /// (`nil`) at its suspension point.
    Close,
    /// A count saturated: the `OutOfMemory` trap.
    Trap,
}

impl Machine {
    /// Settles the zero list and the finalization queue (RC). With `run`
    /// (inside a run, between two instructions) the first object needing
    /// code gets it now; without, such objects are queued.
    pub(crate) fn settle(&mut self, stack: &mut Vec<u64>, prog: &Program, run: bool) -> Settled {
        self.heap.pending = false;
        if self.heap.saturated {
            self.heap.saturated = false;
            return Settled::Trap;
        }
        loop {
            let floor = self.floors.last().copied().unwrap_or(0);
            while self.heap.zero.len() > floor {
                let Some(w) = self.heap.zero.pop() else {
                    break;
                };
                match self.heap.dead(w) {
                    Dead::Alive => {}
                    Dead::Free => self.heap.free_counted(w, prog),
                    Dead::Destruct(f) => {
                        if let Some(Object::Struct(s)) = self.heap.get_mut(w) {
                            s.destructed = true;
                        }
                        if !run {
                            self.queue_finalizer(w);
                        } else if self.start_destructor(stack, prog, w, f) {
                            self.heap.pending = true;
                            return Settled::Call;
                        } else {
                            self.heap.free_counted(w, prog);
                        }
                    }
                    Dead::Close => {
                        if let Some(c) = self.heap.coro_mut(w) {
                            c.finalized = true;
                        }
                        if !run {
                            self.queue_finalizer(w);
                        } else if self.start_close(stack, w) {
                            self.heap.pending = true;
                            return Settled::Close;
                        } else {
                            self.heap.free_counted(w, prog);
                        }
                    }
                }
            }
            // Queued finalizations (found by the cycle collector, or left
            // from outside a run), oldest first and one at a time: the next
            // starts once the running one ended. Each holds a count, given up
            // once its code holds one (or it is skipped).
            if !run || !self.floors.is_empty() {
                return Settled::Done;
            }
            let Some(w) = self.finalize.pop_front() else {
                return Settled::Done;
            };
            let started = self.start_queued(stack, prog, w);
            self.heap.dec(w);
            if let Some(s) = started {
                self.heap.pending = true;
                return s;
            }
        }
    }

    /// Starts the code a queued object needs, if it still needs any.
    pub(crate) fn start_queued(
        &mut self,
        stack: &mut Vec<u64>,
        prog: &Program,
        w: u64,
    ) -> Option<Settled> {
        match self.heap.get(w) {
            Some(Object::Struct(s)) => {
                let f = self.heap.dtor_of(s.ty)?;
                self.start_destructor(stack, prog, w, f)
                    .then_some(Settled::Call)
            }
            Some(Object::Coro(c))
                if matches!(c.state, CoroState::Yielded | CoroState::Awaiting) =>
            {
                self.start_close(stack, w).then_some(Settled::Close)
            }
            _ => None,
        }
    }

    /// Pushes a frame calling destructor `f` with the object `w` as its one
    /// argument; its result is discarded and the interrupted frame resumes
    /// at its pc ([`Cont::Finalizer`]). `false` when the limits cannot hold
    /// the frame.
    pub(crate) fn start_destructor(
        &mut self,
        stack: &mut Vec<u64>,
        prog: &Program,
        w: u64,
        f: u32,
    ) -> bool {
        let Ok(base) = self.push_frame(stack, prog, f, dynv::NIL, Cont::Finalizer) else {
            return false;
        };
        if let Some(slot) = stack.get_mut(base) {
            *slot = w;
        }
        self.heap.inc(w);
        self.heap.stats.finalized += 1;
        if self.heap.rc {
            self.floors.push(self.heap.zero.len());
        }
        true
    }

    /// A destructor or drop-close ended (returned, failed, or was refused):
    /// what died before it may be settled again.
    pub(crate) fn end_finalizer(&mut self) {
        if self.heap.rc {
            let _ = self.floors.pop();
        }
        self.set_finalizing(false);
    }

    /// Enters the dropped suspended coroutine `w` to close it (rule 13): the
    /// caller raises the drop signal at its suspension point. `false` when
    /// the limits cannot hold its frames.
    pub(crate) fn start_close(&mut self, stack: &mut Vec<u64>, w: u64) -> bool {
        if self.coro_enter_dyn(stack, w, Driver::Finalize).is_err() {
            return false;
        }
        if let Some(c) = self.heap.coro_mut(w) {
            c.closing = true;
            let old = core::mem::replace(&mut c.signal, dynv::NIL);
            self.heap.dec(old);
        }
        self.heap.stats.finalized += 1;
        if self.heap.rc {
            self.floors.push(self.heap.zero.len());
        }
        true
    }

    /// Queues an object whose code cannot run now (RC); the queue holds a
    /// count on it.
    pub(crate) fn queue_finalizer(&mut self, w: u64) {
        self.heap.inc(w);
        self.finalize.push_back(w);
        self.heap.pending = true;
    }

    /// Holds a value handed to the host (a run's result, an uncaught error,
    /// a string the host made) until the next run starts or the host
    /// collects: the RC profile's version of "valid until the next run".
    pub(crate) fn pin(&mut self, w: u64) {
        if self.heap.rc && dynv::is_ref(w) && self.heap.get(w).is_some() {
            self.heap.inc(w);
            self.pins.push(w);
        }
    }

    /// Gives up every count [`pin`](Machine::pin) took.
    pub(crate) fn release_pins(&mut self) {
        let pins = core::mem::take(&mut self.pins);
        for &w in &pins {
            self.heap.dec(w);
        }
        self.pins = pins;
        self.pins.clear();
    }

    /// Releases the counts a frame's registers and closure hold (it is being
    /// popped or replaced). Nothing in the tracing profile.
    #[inline(always)]
    pub(crate) fn release_frame(&mut self, stack: &[u64], prog: &Program, frame: &Frame) {
        if self.heap.rc {
            self.release_frame_counted(stack, prog, frame);
        }
    }

    #[inline(never)]
    fn release_frame_counted(&mut self, stack: &[u64], prog: &Program, frame: &Frame) {
        // Released last register first and the closure before them, so the
        // zero list settles the first register's object first (PHP destroys
        // a function's variables in declaration order, then its closure).
        self.heap.dec(frame.closure);
        let refs = prog.func(frame.func).map_or(&[][..], |i| &i.ref_regs[..]);
        for &r in refs.iter().rev() {
            if let Some(&w) = stack.get(frame.base + usize::from(r)) {
                self.heap.dec(w);
            }
        }
    }

    /// Counts the first `n` registers of a new frame of `func` at `base`
    /// (its arguments, just copied in) where they are references.
    #[inline(always)]
    pub(crate) fn adopt_args(
        &mut self,
        stack: &[u64],
        prog: &Program,
        func: u32,
        base: usize,
        n: usize,
    ) {
        if self.heap.rc {
            self.adopt_args_counted(stack, prog, func, base, n);
        }
    }

    #[inline(never)]
    fn adopt_args_counted(
        &mut self,
        stack: &[u64],
        prog: &Program,
        func: u32,
        base: usize,
        n: usize,
    ) {
        let refs = prog.func(func).map_or(&[][..], |i| &i.ref_regs[..]);
        for &r in refs {
            let r = usize::from(r);
            if r >= n {
                break;
            }
            if let Some(&w) = stack.get(base + r) {
                self.heap.inc(w);
            }
        }
    }

    /// Writes register `r` of the frame at `fi` (counted when it is a
    /// reference register).
    #[inline(always)]
    pub(crate) fn set_reg(&mut self, stack: &mut [u64], prog: &Program, fi: usize, r: u16, v: u64) {
        if self.heap.rc {
            self.set_reg_counted(stack, prog, fi, r, v);
        } else if let Some(slot) = self
            .frames
            .get(fi)
            .and_then(|f| stack.get_mut(f.base + usize::from(r)))
        {
            *slot = v;
        }
    }

    #[inline(never)]
    fn set_reg_counted(&mut self, stack: &mut [u64], prog: &Program, fi: usize, r: u16, v: u64) {
        let Some(f) = self.frames.get(fi) else {
            return;
        };
        let (func, base) = (f.func, f.base);
        let Some(slot) = stack.get_mut(base + usize::from(r)) else {
            return;
        };
        let old = core::mem::replace(slot, v);
        if prog
            .func(func)
            .and_then(|i| i.regs.get(usize::from(r)))
            .is_some_and(|t| t.is_reference())
        {
            self.heap.swap(v, old);
        }
    }

    /// The cycle collector (RC), from [`Machine::collect`]: recounts the
    /// bytes held when a recount is due, then collects when the
    /// possible-roots buffer reached its threshold, when the budget is
    /// tight (over seven eighths used after the recount), or always with
    /// `full` (the host's explicit collection, which neither adapts the
    /// threshold nor charges fuel). Finalizable garbage is queued.
    pub(crate) fn collect_cycles(&mut self, prog: &Program, full: bool) {
        let recounted = self.heap.recount_due();
        if recounted {
            self.heap.recount();
        }
        let crowded = self.heap.roots.len() >= self.heap.root_threshold;
        let tight = recounted && self.heap.used() > self.heap.limit / 8 * 7;
        if !full && !crowded && !tight {
            return;
        }
        let mut rescued = core::mem::take(&mut self.dropped);
        rescued.clear();
        let swept = self.heap.collect_cycles(prog, &mut rescued);
        for &w in &rescued {
            self.queue_finalizer(w);
        }
        rescued.clear();
        self.dropped = rescued;
        if !full {
            self.heap.adapt_threshold(swept.freed);
            // Collection work is charged against the run's fuel, so a
            // program cannot make it unbounded: one unit per
            // `VISITS_PER_FUEL` nodes, at least one per collection. A
            // budget spent here traps at the next charge point.
            let units = swept.visits.div_ceil(VISITS_PER_FUEL).max(1) as u64;
            self.fuel = self.fuel.saturating_sub(units);
        }
    }

    /// Checks that every live object is reachable (RC, debug builds, after a
    /// full collection with no run in progress): counting freed everything
    /// else and the cycle collector every garbage cycle.
    #[cfg(debug_assertions)]
    pub(crate) fn check_reachable(&mut self, prog: &Program) -> Result<(), alloc::string::String> {
        let mut roots: Vec<u64> = Vec::new();
        roots.extend(
            prog.global_refs
                .iter()
                .filter_map(|&g| self.globals.get(g as usize).copied()),
        );
        roots.extend(self.str_consts.iter().chain(self.dyn_consts.iter()));
        roots.extend(self.typed_consts.values());
        roots.extend(
            self.names
                .iter()
                .chain(self.fn_refs.iter())
                .chain(self.imp_refs.iter()),
        );
        roots.extend(self.finalize.iter().chain(self.pins.iter()));
        for &(t, wake) in &self.sched.ready {
            let (crate::machine::Wake::Send(v) | crate::machine::Wake::Throw(v)) = wake;
            roots.extend([t, v]);
        }
        for (&t, ws) in &self.sched.waiters {
            roots.push(t);
            roots.extend(ws.iter().copied());
        }
        // A stuck count is never freed, by design.
        let lost: Vec<usize> = self
            .heap
            .unreached(&roots, prog)
            .into_iter()
            .filter(|&i| {
                self.heap
                    .slots
                    .get(i)
                    .is_some_and(|s| s.flags & crate::heap::STUCK == 0)
            })
            .collect();
        if lost.is_empty() {
            return Ok(());
        }
        let kinds: Vec<_> = lost
            .iter()
            .take(8)
            .filter_map(|&i| {
                self.heap
                    .slots
                    .get(i)
                    .and_then(|s| s.obj.as_ref())
                    .map(Object::kind)
            })
            .collect();
        Err(alloc::format!(
            "{} live objects unreachable: {kinds:?}",
            lost.len()
        ))
    }

    /// Recounts every count from its holders and compares (RC, debug
    /// builds): the invariant the profile rests on. Returns the first
    /// difference found.
    #[cfg(debug_assertions)]
    pub(crate) fn check_counts(
        &self,
        stack: &[u64],
        prog: &Program,
    ) -> Result<(), alloc::string::String> {
        use alloc::collections::{BTreeMap, BTreeSet};
        use alloc::sync::Arc;

        use crate::machine::Wake;

        let mut want: BTreeMap<u64, u64> = BTreeMap::new();
        let add = |w: u64, want: &mut BTreeMap<u64, u64>| {
            if self.heap.count(w).is_some() {
                *want.entry(w).or_insert(0) += 1;
            }
        };
        for f in &self.frames {
            let refs = prog.func(f.func).map_or(&[][..], |i| &i.ref_regs[..]);
            for &r in refs {
                if let Some(&w) = stack.get(f.base + usize::from(r)) {
                    add(w, &mut want);
                }
            }
            add(f.closure, &mut want);
        }
        for a in &self.coros {
            add(a.coro, &mut want);
            if let Driver::Iter { iter, .. } = a.driver {
                add(iter, &mut want);
            }
        }
        for &(t, wake) in self.sched.ready.iter().chain(&self.waking) {
            add(t, &mut want);
            let (Wake::Send(v) | Wake::Throw(v)) = wake;
            add(v, &mut want);
        }
        for (&t, ws) in &self.sched.waiters {
            add(t, &mut want);
            for &w in ws {
                add(w, &mut want);
            }
        }
        for &w in self.finalize.iter().chain(&self.pins) {
            add(w, &mut want);
        }
        for &g in &prog.global_refs {
            if let Some(&w) = self.globals.get(g as usize) {
                add(w, &mut want);
            }
        }
        let caches = self
            .str_consts
            .iter()
            .chain(self.dyn_consts.iter())
            .chain(self.typed_consts.values())
            .chain(self.names.iter())
            .chain(self.fn_refs.iter())
            .chain(self.imp_refs.iter());
        for &w in caches {
            add(w, &mut want);
        }
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        let mut kids = Vec::new();
        for slot in &self.heap.slots {
            let Some(obj) = &slot.obj else { continue };
            kids.clear();
            match obj {
                // Shared storage counts its words once.
                Object::Array(a) if Arc::strong_count(&a.items) > 1 => {
                    if a.elem.is_reference() && seen.insert(Arc::as_ptr(&a.items) as usize) {
                        kids.extend(a.items.iter().copied());
                    }
                }
                Object::Map(m) if Arc::strong_count(&m.store) > 1 => {
                    if seen.insert(Arc::as_ptr(&m.store) as usize) {
                        crate::heap::map_words(m, &mut kids);
                    }
                }
                other => crate::heap::owned_children(other, prog, &mut kids),
            }
            for &w in &kids {
                add(w, &mut want);
            }
        }
        for (i, slot) in self.heap.slots.iter().enumerate() {
            if slot.obj.is_none() || slot.flags & crate::heap::STUCK != 0 {
                continue;
            }
            let w = dynv::from_ref(i as u32, slot.tag);
            let expect = want.get(&w).copied().unwrap_or(0);
            if u64::from(slot.rc) != expect {
                return Err(alloc::format!(
                    "slot {i} ({:?}): count {} but {expect} holders",
                    slot.obj.as_ref().map(Object::kind),
                    slot.rc
                ));
            }
        }
        Ok(())
    }
}
