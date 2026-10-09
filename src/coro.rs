//! Coroutines (LSB §5.13): stackful, asymmetric, with keys, return values,
//! throwing in, closing, iteration, tasks, and close on drop.
//!
//! **Representation.** A coroutine is a heap object ([`CoroObj`]). While it
//! runs, its frames sit on the VM's one active frame stack above its
//! resumer's, the bottom one (its body) marked [`Cont::Coro`], and the chain
//! of running coroutines is [`Machine::coros`] (outermost first, each with
//! the index of its body frame and who drove it). Suspending copies the
//! frames from the body up, with their registers, into the object; resuming
//! copies them back on top of the resumer. A switch therefore costs one copy
//! of the suspended registers (no allocation once the object's buffers have
//! grown to the coroutine's deepest stack), and the dispatch loop keeps one
//! flat register stack, so nothing on the hot path changes (LSB §10 question
//! 7: copy on suspension, not segmented stacks).
//!
//! **Drivers.** What a coroutine produces when it next stops goes to its
//! [`Driver`]: a register of the resumer (`resume`, `resume_throw`,
//! `coro_close`), an iterator (`iter_next`, rule 7), the host (the built-in
//! scheduler), or nobody (a drop-close).
//!
//! **Host frames.** LSB raises `CannotSuspend` when a host frame lies between
//! a `yield` and its coroutine. bvm's host functions never call back into
//! bytecode (they run to completion on the Rust stack without re-entering the
//! VM), so no such frame can exist: every frame between a suspension point
//! and its coroutine's body is a bytecode frame (calls, hook calls) and is
//! suspended with it. `CannotSuspend` is raised exactly when no coroutine is
//! running.
//!
//! **Close on drop** (rule 13 as decided in LSB §10 question 9). bvm's heap
//! is traced, not reference-counted, so the last reference going away is not
//! observable when it happens; a suspended coroutine found unreachable by a
//! collection is resurrected and queued ([`Machine::finalize`]), and the VM
//! closes queued coroutines one at a time, oldest first, at the next frame
//! transition (a call, return, resume, suspension, or handler entry) of a
//! run, or when the host calls [`Vm::run_finalizers`]. The close signal is
//! `nil`; the outcome (return value, error, or a refusal) is discarded.
//!
//! [`Vm::run_finalizers`]: crate::Vm::run_finalizers

use alloc::boxed::Box;
use alloc::vec::Vec;

use bytecode_lang::{CoroState, ErrorKind, Hook, Inst, ValType};

use crate::bind;
use crate::coll::not_a;
use crate::conv;
use crate::dynv;
use crate::exec::{Cx, Step};
use crate::fault::Fault;
use crate::heap::{Callable, CoroObj, FRAME_BYTES, Object};
use crate::machine::{Active, Charge, Cont, Driver, Frame, Machine, Stop, Wake};
use crate::program::Program;
use crate::value::{Obj, Value};

/// What a suspending or finishing coroutine hands its driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Event {
    Yielded {
        payload: u64,
        key: u64,
    },
    Awaiting(u64),
    /// It yielded while being closed (rule 11).
    CloseIgnored,
}

/// How a coroutine was entered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Entered {
    /// It was `created`: its body starts at pc 0.
    Fresh,
    /// It was suspended; the sent value goes to this register of its top
    /// frame, which then continues after the suspending instruction.
    Suspended(u16),
}

impl Machine {
    /// A new coroutine in state `created` whose body is `func` with `args`
    /// (already in the parameters' representations) and running closure
    /// `closure`. (A)
    pub(crate) fn coro_create(
        &mut self,
        prog: &Program,
        func: u32,
        closure: u64,
        args: &[u64],
    ) -> Result<u64, Fault> {
        let nregs = prog.func(func).map_or(0, |f| f.nregs);
        let bytes = nregs
            .saturating_mul(8)
            .saturating_add(core::mem::size_of::<CoroObj>() + FRAME_BYTES);
        if !self.heap.fits(bytes) {
            return Err(Fault::Trap(ErrorKind::OutOfMemory));
        }
        let mut regs = alloc::vec![0u64; nregs];
        if let Some(dst) = regs.get_mut(..args.len()) {
            dst.copy_from_slice(args);
        }
        let seq = self.coro_seq;
        self.coro_seq = self.coro_seq.wrapping_add(1);
        self.heap.alloc(Object::Coro(Box::new(CoroObj {
            state: CoroState::Created,
            closing: false,
            finalized: false,
            task: false,
            seq,
            frames: alloc::vec![Frame {
                func,
                pc: 0,
                base: 0,
                closure,
                cont: Cont::Coro,
            }],
            regs,
            resume_dst: 0,
            payload: dynv::NIL,
            key: dynv::NIL,
            max_key: -1,
            result: dynv::NIL,
            signal: dynv::NIL,
        })))
    }

    /// The state of the coroutine `v` names (`NullReference`/`TypeError` for
    /// anything else).
    pub(crate) fn coro_state(&self, v: u64) -> Result<CoroState, Fault> {
        self.heap
            .coro(v)
            .map(|c| c.state)
            .ok_or_else(|| not_a(&self.heap, v))
    }

    /// Moves a resumable coroutine's frames on top of the active stack and
    /// makes it the running coroutine (rule 1). The caller has recorded the
    /// resumer's pc. Fails, changing nothing, for a non-coroutine, a state
    /// other than `created`/`yielded`/`awaiting` (`InvalidCoroState`), or a
    /// stack the depth or register limits cannot hold (`StackOverflow`).
    pub(crate) fn coro_enter(
        &mut self,
        stack: &mut Vec<u64>,
        coro: u64,
        driver: Driver,
    ) -> Result<Entered, Fault> {
        let Machine {
            heap,
            frames,
            coros,
            max_depth,
            max_stack,
            ..
        } = self;
        let Some(co) = heap.coro_mut(coro) else {
            return Err(not_a(heap, coro));
        };
        if !co.state.is_resumable() || co.frames.is_empty() {
            return Err(Fault::raise(ErrorKind::InvalidCoroState));
        }
        // A coroutine's frames count toward the limits of the chain running
        // them (§5.13 "Limits").
        if frames.len() + co.frames.len() > *max_depth || stack.len() + co.regs.len() > *max_stack {
            return Err(Fault::raise(ErrorKind::StackOverflow));
        }
        let entered = if co.state == CoroState::Created {
            Entered::Fresh
        } else {
            Entered::Suspended(co.resume_dst)
        };
        let offset = stack.len();
        let floor = frames.len();
        stack.extend_from_slice(&co.regs);
        co.regs.clear();
        frames.extend(co.frames.drain(..).map(|f| Frame {
            base: f.base + offset,
            ..f
        }));
        co.state = CoroState::Running;
        coros.push(Active {
            coro,
            floor,
            driver,
        });
        Ok(entered)
    }

    /// After [`coro_enter`](Machine::coro_enter) of a suspended coroutine:
    /// the sent value becomes the suspending instruction's result and its
    /// frame continues after it.
    pub(crate) fn coro_send(&mut self, stack: &mut [u64], dst: u16, value: u64) {
        if let Some(top) = self.frames.last_mut() {
            if let Some(slot) = stack.get_mut(top.base + usize::from(dst)) {
                *slot = value;
            }
            top.pc += 1;
        }
    }

    /// Suspends the running coroutine (rule 2): every frame from its body up
    /// moves into the object; the caller has recorded the top frame's pc (the
    /// suspending instruction). Fails, changing nothing, with
    /// `CannotSuspend` when no coroutine is running, or the `OutOfMemory`
    /// trap when the suspended stack does not fit the memory budget.
    /// `key` is the new key and largest-integer-key for a `yield`.
    pub(crate) fn coro_suspend(
        &mut self,
        stack: &mut Vec<u64>,
        state: CoroState,
        dst: u16,
        payload: u64,
        key: Option<(u64, i64)>,
    ) -> Result<(Driver, Event), Fault> {
        let Some(&act) = self.coros.last() else {
            return Err(Fault::raise(ErrorKind::CannotSuspend));
        };
        let Machine {
            heap,
            frames,
            coros,
            ..
        } = self;
        let Some(body) = frames.get(act.floor) else {
            return Err(Fault::raise(ErrorKind::CannotSuspend));
        };
        let sbase = body.base;
        let nregs = stack.len().saturating_sub(sbase);
        let nframes = frames.len() - act.floor;
        let Some(co) = heap.coro(act.coro) else {
            return Err(Fault::type_error());
        };
        // Charge the suspended stack's growth before moving anything (the
        // buffers keep their capacity, so a coroutine pays once for its
        // deepest stack).
        let grow = nregs.saturating_sub(co.regs.capacity()) * 8
            + nframes.saturating_sub(co.frames.capacity()) * FRAME_BYTES;
        if grow > 0 {
            heap.charge(grow)?;
        }
        let Some(co) = heap.coro_mut(act.coro) else {
            return Err(Fault::type_error());
        };
        co.regs.clear();
        co.regs.reserve_exact(nregs);
        co.regs.extend_from_slice(stack.get(sbase..).unwrap_or(&[]));
        stack.truncate(sbase);
        co.frames.clear();
        co.frames.reserve_exact(nframes);
        co.frames.extend(frames.drain(act.floor..).map(|f| Frame {
            base: f.base.saturating_sub(sbase),
            ..f
        }));
        co.state = state;
        co.resume_dst = dst;
        co.payload = payload;
        if let Some((k, max)) = key {
            co.key = k;
            co.max_key = max;
        }
        let event = if state == CoroState::Awaiting {
            Event::Awaiting(payload)
        } else if co.closing {
            // Rule 11: a coroutine being closed that yields stays suspended
            // at that yield, no longer closing, and its closer is told.
            co.closing = false;
            co.signal = dynv::NIL;
            Event::CloseIgnored
        } else {
            Event::Yielded {
                payload,
                key: co.key,
            }
        };
        let _ = coros.pop();
        Ok((act.driver, event))
    }

    /// Hands a suspension to its driver. The driver's frame (if any) is now
    /// the top frame, at the driving instruction; an error returned here is
    /// raised there.
    pub(crate) fn deliver(
        &mut self,
        stack: &mut [u64],
        driver: Driver,
        event: Event,
    ) -> Result<(), Fault> {
        match driver {
            Driver::Resume(dst) => match event {
                Event::Yielded { payload, .. } | Event::Awaiting(payload) => {
                    self.coro_send(stack, dst, payload);
                    Ok(())
                }
                Event::CloseIgnored => Err(Fault::raise(ErrorKind::CloseIgnored)),
            },
            Driver::Iter { has, val, iter } => match event {
                Event::Yielded { payload, key } => {
                    if let Some(Object::Iter(it)) = self.heap.get_mut(iter) {
                        it.key = Some(key);
                    }
                    if let Some(top) = self.frames.last() {
                        if let Some(slot) = stack.get_mut(top.base + usize::from(val)) {
                            *slot = payload;
                        }
                    }
                    self.coro_send(stack, has, 1);
                    Ok(())
                }
                // Rule 7: an async coroutine cannot be iterated synchronously.
                Event::Awaiting(_) => Err(Fault::type_error()),
                Event::CloseIgnored => Err(Fault::raise(ErrorKind::CloseIgnored)),
            },
            Driver::Host => {
                self.outcome = Some(match event {
                    Event::Yielded { payload, .. } => Stop::Yielded(payload),
                    Event::Awaiting(payload) => Stop::Awaiting(payload),
                    Event::CloseIgnored => Stop::CloseIgnored,
                });
                Ok(())
            }
            Driver::Finalize => {
                self.set_finalizing(false);
                Ok(())
            }
        }
    }

    /// The running coroutine's body returned `value` (already `dyn`); its
    /// frame is popped. It becomes `returned` and its driver gets the value
    /// (rule 3).
    #[cold]
    #[inline(never)]
    pub(crate) fn coro_returned(&mut self, stack: &mut [u64], value: u64) {
        let Some(act) = self.coros.pop() else {
            return;
        };
        if let Some(co) = self.heap.coro_mut(act.coro) {
            finish(co, CoroState::Returned, value);
        }
        self.deliver_return(stack, act.driver, value);
    }

    /// Delivers a return value (never fails).
    fn deliver_return(&mut self, stack: &mut [u64], driver: Driver, value: u64) {
        match driver {
            Driver::Resume(dst) => self.coro_send(stack, dst, value),
            Driver::Iter { has, iter, .. } => {
                if let Some(Object::Iter(it)) = self.heap.get_mut(iter) {
                    it.done = true;
                }
                self.coro_send(stack, has, 0);
            }
            Driver::Host => self.outcome = Some(Stop::Returned(value)),
            Driver::Finalize => self.set_finalizing(false),
        }
    }

    /// The error `value` escaped the running coroutine's body (its frame is
    /// popped). Returns whether unwinding stops here: a close that succeeded
    /// (the error is the close signal, rule 11), or a driver that is not a
    /// frame (host, drop-close). Otherwise the coroutine is `failed` and the
    /// search continues in the driver's frame, at the driving instruction.
    #[cold]
    #[inline(never)]
    pub(crate) fn coro_escaped(&mut self, stack: &mut [u64], value: u64, at: (u32, u32)) -> bool {
        let Some(act) = self.coros.pop() else {
            return false;
        };
        let closed = match self.heap.coro_mut(act.coro) {
            Some(co) => {
                let closed = co.closing && value == co.signal;
                if closed {
                    finish(co, CoroState::Returned, dynv::NIL);
                } else {
                    finish(co, CoroState::Failed, value);
                }
                closed
            }
            None => false,
        };
        if closed {
            self.deliver_return(stack, act.driver, dynv::NIL);
            return true;
        }
        match act.driver {
            Driver::Host => {
                self.outcome = Some(Stop::Failed(value, at.0, at.1));
                true
            }
            Driver::Finalize => {
                // A drop-close's error has no one to go to (CPython reports
                // it as "unraisable"); it is discarded.
                self.set_finalizing(false);
                true
            }
            Driver::Resume(_) | Driver::Iter { .. } => false,
        }
    }

    /// Ends every running coroutine after a run aborted (a trap): their
    /// frames are gone, so they are `failed`.
    pub(crate) fn abort_coros(&mut self) {
        for act in self.coros.drain(..) {
            if let Some(co) = self.heap.coro_mut(act.coro) {
                finish(co, CoroState::Failed, dynv::NIL);
            }
        }
        self.set_finalizing(false);
        self.outcome = None;
    }

    /// The built-in scheduler's `spawn` entry: makes a coroutine a task.
    pub(crate) fn spawn_task(&mut self, arg: Value) -> Result<u64, Fault> {
        let Value::Obj(Obj(w)) = arg else {
            return Err(Fault::type_error());
        };
        let Some(co) = self.heap.coro_mut(w) else {
            return Err(Fault::type_error());
        };
        if !co.task && co.state.is_resumable() {
            co.task = true;
            self.sched.ready.push_back((w, Wake::Send(dynv::NIL)));
        }
        Ok(w)
    }

    /// Enters the oldest dropped coroutine still suspended on top of the
    /// current frames to close it (rule 13); the caller raises the close
    /// signal (`nil`) at its suspension point, and the [`Driver::Finalize`]
    /// delivery ends it. A coroutine the limits cannot hold is skipped (it
    /// stays suspended and is freed when next found unreachable). Returns
    /// whether one was entered.
    #[cold]
    #[inline(never)]
    pub(crate) fn start_finalizer(&mut self, stack: &mut Vec<u64>) -> bool {
        self.unbank();
        while let Some(c) = self.finalize.pop_front() {
            let suspended = self
                .heap
                .coro(c)
                .is_some_and(|co| matches!(co.state, CoroState::Yielded | CoroState::Awaiting));
            if !suspended {
                continue;
            }
            if self.coro_enter(stack, c, Driver::Finalize).is_err() {
                continue;
            }
            if let Some(co) = self.heap.coro_mut(c) {
                co.closing = true;
                co.signal = dynv::NIL;
            }
            self.set_finalizing(true);
            return true;
        }
        self.set_finalizing(false);
        false
    }

    /// Records whether a drop-close is running; when none is and some are
    /// queued, arms the next fuel charge to start one.
    pub(crate) fn set_finalizing(&mut self, running: bool) {
        self.finalizing = running;
        self.close_ready = !running && !self.finalize.is_empty();
        self.arm_close();
    }

    /// While a close is ready, the fuel left is set aside ([`Machine::bank`])
    /// and the counter reads 0, so the next charge takes the cold branch
    /// ([`charge_slow`](Machine::charge_slow)), which starts the close: the
    /// dispatch loop needs no check of its own for pending closes.
    pub(crate) fn arm_close(&mut self) {
        if self.close_ready && self.bank.is_none() {
            self.bank = Some(self.fuel);
            self.fuel = 0;
        }
    }

    /// Takes back the fuel [`arm_close`](Machine::arm_close) set aside.
    pub(crate) fn unbank(&mut self) {
        if let Some(f) = self.bank.take() {
            self.fuel = f;
        }
    }

    /// The cold branch of a fuel charge, taken when the counter reads 0:
    /// either the budget is spent, or a close is armed. With `at` (the
    /// running frame and the charging instruction, which has done nothing
    /// yet) a ready close starts there, and the instruction runs again when it
    /// ends; without it (handler entry, a second charge inside one
    /// instruction) the close stays armed for the next charge.
    #[cold]
    #[inline(never)]
    pub(crate) fn charge_slow(&mut self, stack: &mut Vec<u64>, at: Option<(usize, u32)>) -> Charge {
        self.unbank();
        if self.fuel == 0 {
            return Charge::Spent;
        }
        if let Some((fi, pc)) = at {
            if self.close_ready {
                if let Some(f) = self.frames.get_mut(fi) {
                    f.pc = pc;
                }
                if self.start_finalizer(stack) {
                    return Charge::Close;
                }
            }
        }
        self.fuel -= 1;
        self.arm_close();
        Charge::Done
    }
}

/// Marks a coroutine finished and releases its stack.
fn finish(co: &mut CoroObj, state: CoroState, result: u64) {
    co.state = state;
    co.result = result;
    co.closing = false;
    co.signal = dynv::NIL;
    co.frames = Vec::new();
    co.regs = Vec::new();
}

/// The key of a `yield` (rule 10, PHP's generators): one more than the
/// counter `largest` (which starts at -1 and only grows), with
/// `ArithOverflow` past `i64::MAX`. Returns the key and the new counter.
fn auto_key(m: &mut Machine, largest: i64) -> Result<(u64, i64), Fault> {
    let next = largest
        .checked_add(1)
        .ok_or(Fault::Raise(ErrorKind::ArithOverflow))?;
    Ok((conv::encode_int(&mut m.heap, next)?, next))
}

/// Executes one coroutine instruction (`0xF0`..=`0xFC`). Every one charges
/// one unit of fuel first and is a safepoint (LSB §5.13, §5.14).
#[allow(clippy::too_many_lines)]
pub(crate) fn exec(
    m: &mut Machine,
    stack: &mut Vec<u64>,
    prog: &Program,
    cx: &Cx<'_>,
    pc: usize,
    inst: Inst,
) -> Step {
    let Cx { base, fi, .. } = *cx;
    macro_rules! r {
        ($x:expr) => {
            stack[base + ($x).index()]
        };
    }
    macro_rules! t {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(f) => return Step::Fault(f),
            }
        };
    }
    macro_rules! charge {
        () => {
            if let Some(step) = crate::exec::charge(m, stack, fi, pc) {
                return step;
            }
        };
    }
    macro_rules! gc {
        ($extra:expr) => {
            if m.heap.wants_gc($extra) {
                m.collect(stack, prog);
            }
        };
    }
    charge!();
    match inst {
        Inst::CoroNew { dst, func, argc } => {
            let nregs = prog.func(func.0).map_or(0, |f| f.nregs);
            gc!(nregs * 8);
            let first = base + dst.index() + 1;
            let mut args = core::mem::take(&mut m.scratch);
            args.clear();
            args.extend_from_slice(&stack[first..first + usize::from(argc)]);
            let made = m.coro_create(prog, func.0, dynv::NIL, &args);
            m.scratch = args;
            r!(dst) = t!(made);
            Step::Next
        }
        Inst::CoroNewIndirect { dst, callee, argc } => {
            r!(dst) = t!(indirect(
                m,
                stack,
                prog,
                cx,
                dst.index(),
                callee.index(),
                argc
            ));
            Step::Next
        }
        Inst::Spawn { dst, callee, argc } => {
            // Rule 8: no scheduler, no task (checked before anything is made).
            let Some(hook) = prog.hooks[usize::from(Hook::Spawn.code())] else {
                return Step::Fault(Fault::raise(ErrorKind::NoScheduler));
            };
            let c = t!(indirect(
                m,
                stack,
                prog,
                cx,
                dst.index(),
                callee.index(),
                argc
            ));
            // The hook invocation is charged as every hook invocation is. The
            // coroutine already exists, so no close may start here.
            if m.fuel == 0 {
                if m.charge_slow(stack, None) == Charge::Spent {
                    return Step::Fault(Fault::Trap(ErrorKind::OutOfFuel));
                }
            } else {
                m.fuel -= 1;
            }
            match hook {
                bytecode_lang::Callee::Func(hf) => {
                    let hb = t!(m.push_frame(stack, prog, hf.0, dynv::NIL, Cont::Value(dst.0)));
                    stack[hb] = c;
                    m.frames[fi].pc = pc as u32;
                    Step::Frame
                }
                bytecode_lang::Callee::Import(hi) => {
                    let res = t!(m.call_host_dyn(prog, hi.0, &[c]));
                    t!(m.apply_cont(stack, Cont::Value(dst.0), res, Some(ValType::Dyn)));
                    Step::Next
                }
            }
        }
        Inst::Yield { dst, src } | Inst::YieldKv { dst, src, .. } | Inst::Await { dst, src } => {
            let Some(act) = m.coros.last() else {
                return Step::Fault(Fault::raise(ErrorKind::CannotSuspend));
            };
            let coro = act.coro;
            let payload = r!(src);
            let (state, key) = match inst {
                Inst::Await { .. } => (CoroState::Awaiting, None),
                Inst::YieldKv { key, .. } => {
                    let k = r!(key);
                    let max = m.heap.coro(coro).map_or(-1, |c| c.max_key);
                    // An integer key larger than any before moves the
                    // automatic-key counter (rule 10); a smaller one,
                    // negative ones included, leaves it.
                    let max = match conv::dyn_int(&m.heap, k) {
                        Some(i) => max.max(i),
                        None => max,
                    };
                    (CoroState::Yielded, Some((k, max)))
                }
                _ => {
                    let max = m.heap.coro(coro).map_or(-1, |c| c.max_key);
                    (CoroState::Yielded, Some(t!(auto_key(m, max))))
                }
            };
            m.frames[fi].pc = pc as u32;
            let (driver, event) = t!(m.coro_suspend(stack, state, dst.0, payload, key));
            match m.deliver(stack, driver, event) {
                Ok(()) => Step::Frame,
                Err(f) => Step::Unwind(f),
            }
        }
        Inst::Resume { dst, coro, src } => {
            gc!(0);
            let (c, sent) = (r!(coro), r!(src));
            m.frames[fi].pc = pc as u32;
            match t!(m.coro_enter(stack, c, Driver::Resume(dst.0))) {
                Entered::Fresh => {}
                Entered::Suspended(d) => m.coro_send(stack, d, sent),
            }
            Step::Frame
        }
        Inst::ResumeThrow { dst, coro, src } => {
            gc!(0);
            let (c, err) = (r!(coro), r!(src));
            // Rule 4: on a created coroutine nothing runs; it fails and the
            // error is raised here.
            if t!(m.coro_state(c)) == CoroState::Created {
                if let Some(co) = m.heap.coro_mut(c) {
                    finish(co, CoroState::Failed, err);
                }
                return Step::Fault(Fault::Throw(err));
            }
            m.frames[fi].pc = pc as u32;
            let _ = t!(m.coro_enter(stack, c, Driver::Resume(dst.0)));
            // Raised at the suspension point: the suspended frames' handlers
            // are searched from the suspending pc outward.
            Step::Unwind(Fault::Throw(err))
        }
        Inst::CoroClose { dst, coro, src } => {
            gc!(0);
            let (c, signal) = (r!(coro), r!(src));
            match t!(m.coro_state(c)) {
                CoroState::Created => {
                    if let Some(co) = m.heap.coro_mut(c) {
                        finish(co, CoroState::Returned, dynv::NIL);
                    }
                    r!(dst) = dynv::NIL;
                    Step::Next
                }
                CoroState::Returned | CoroState::Failed => {
                    r!(dst) = dynv::NIL;
                    Step::Next
                }
                CoroState::Yielded | CoroState::Awaiting => {
                    m.frames[fi].pc = pc as u32;
                    let _ = t!(m.coro_enter(stack, c, Driver::Resume(dst.0)));
                    if let Some(co) = m.heap.coro_mut(c) {
                        co.closing = true;
                        co.signal = signal;
                    }
                    Step::Unwind(Fault::Throw(signal))
                }
                // Running (itself or one of its resumers): it cannot be
                // interrupted from inside.
                _ => Step::Fault(Fault::raise(ErrorKind::InvalidCoroState)),
            }
        }
        Inst::CoroStatus { dst, coro } => {
            r!(dst) = u64::from(t!(m.coro_state(r!(coro))).code());
            Step::Next
        }
        Inst::CoroCurrent { dst } => {
            r!(dst) = m.coros.last().map_or(dynv::NIL, |a| a.coro);
            Step::Next
        }
        Inst::CoroKey { dst, coro } => {
            let c = r!(coro);
            match m.heap.coro(c) {
                Some(co) => r!(dst) = co.key,
                None => return Step::Fault(not_a(&m.heap, c)),
            }
            Step::Next
        }
        Inst::CoroResult { dst, coro } => {
            let c = r!(coro);
            match m.heap.coro(c) {
                Some(co) if co.state == CoroState::Returned => r!(dst) = co.result,
                Some(_) => return Step::Fault(Fault::raise(ErrorKind::InvalidCoroState)),
                None => return Step::Fault(not_a(&m.heap, c)),
            }
            Step::Next
        }
        // Only the coroutine group is routed here.
        _ => Step::Next,
    }
}

/// `iter_next` on an iterator over a coroutine (rule 7): resumes it with
/// `nil` and lets the delivery fill `has`/`val`. Charged like a coroutine
/// instruction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn iter_next(
    m: &mut Machine,
    stack: &mut Vec<u64>,
    prog: &Program,
    cx: &Cx<'_>,
    pc: usize,
    (has, iter, val): (u16, u64, u16),
    src: u64,
) -> Step {
    let Cx { base, fi, .. } = *cx;
    if let Some(step) = crate::exec::charge(m, stack, fi, pc) {
        return step;
    }
    let done = matches!(m.heap.get(iter), Some(Object::Iter(i)) if i.done);
    let state = match m.coro_state(src) {
        Ok(s) => s,
        Err(f) => return Step::Fault(f),
    };
    if done || state == CoroState::Returned {
        if let Some(Object::Iter(i)) = m.heap.get_mut(iter) {
            i.done = true;
        }
        stack[base + usize::from(has)] = 0;
        return Step::Next;
    }
    if state == CoroState::Awaiting {
        return Step::Fault(Fault::type_error());
    }
    if m.heap.wants_gc(0) {
        m.collect(stack, prog);
    }
    m.frames[fi].pc = pc as u32;
    match m.coro_enter(stack, src, Driver::Iter { has, val, iter }) {
        Ok(Entered::Fresh) => Step::Frame,
        Ok(Entered::Suspended(d)) => {
            m.coro_send(stack, d, dynv::NIL);
            Step::Frame
        }
        Err(f) => Step::Fault(f),
    }
}

/// `coro_new_indirect` (and `spawn`'s coroutine): a closure or function
/// reference with the window's arguments. With a `dyn` callee register the
/// window binds to the body's parameter list exactly as `dcall` binds it
/// (LSB §5.15: `ArgumentError` when it does not, by-reference parameters
/// receive references, rest parameters their collections, the presence mask
/// last); otherwise the arguments pass as they are (as `call_indirect`,
/// exact arity). `nil` is `NullReference`; any other non-function, an import
/// (a host function cannot be suspended), or a typed arity mismatch is
/// `TypeError`.
fn indirect(
    m: &mut Machine,
    stack: &[u64],
    prog: &Program,
    cx: &Cx<'_>,
    dst: usize,
    callee: usize,
    argc: u8,
) -> Result<u64, Fault> {
    let Cx { info, base, .. } = *cx;
    let cv = stack[base + callee];
    let func = match m.heap.get(cv) {
        Some(Object::Func(f)) => match f.target {
            Callable::Func(f) => f,
            Callable::Import(_) => return Err(Fault::type_error()),
        },
        _ => return Err(not_a(&m.heap, cv)),
    };
    let Some(body) = prog.func(func) else {
        return Err(Fault::type_error());
    };
    let argc = usize::from(argc);
    let dynamic = info.regs.get(callee) == Some(&ValType::Dyn);
    if !dynamic && body.nparams != argc {
        return Err(Fault::type_error());
    }
    // Collect first: nothing below collects, so the references and rest
    // collections the binding allocates live until the coroutine holds them.
    if m.heap.wants_gc(body.nregs * 8) {
        m.collect(stack, prog);
    }
    let first = base + dst + 1;
    let window = &stack[first..first + argc];
    let mut args = core::mem::take(&mut m.scratch);
    let made = if dynamic {
        let target = Callable::Func(func);
        let list = bind::params_of(prog, target);
        let sig = &body.regs[..body.nparams];
        let mut bound = core::mem::take(&mut m.bound);
        let built = bind::bind(prog, list, sig.len(), None, argc, &mut bound).and_then(|mask| {
            bind::build_args(m, prog, list, sig, window, None, &bound, mask, &mut args)
        });
        m.bound = bound;
        built.and_then(|()| m.coro_create(prog, func, cv, &args))
    } else {
        args.clear();
        args.extend_from_slice(window);
        m.coro_create(prog, func, cv, &args)
    };
    m.scratch = args;
    made
}
