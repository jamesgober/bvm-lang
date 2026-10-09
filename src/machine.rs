//! The mutable state of one VM instance, and the operations on it that are
//! not instruction dispatch: frames, constant materialisation, caches, host
//! calls, call continuations, unwinding, and garbage collection.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;

use bytecode_lang::{Const, ConstId, ErrorKind, FuncId, ValType};

use crate::coll;
use crate::conv;
use crate::dynv;
use crate::error::VmError;
use crate::fault::Fault;
use crate::hash::Seed;
use crate::heap::{ArrayObj, Callable, ErrorObj, FuncObj, Heap, MapObj, Object};
use crate::host::{HostCtx, HostError, HostFn};
use crate::int;
use crate::program::{Program, TypeInfo};
use crate::value::Value;

/// What happens to a callee's result when its frame returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cont {
    /// The bottom frame of a run: the result leaves the VM.
    Entry,
    /// `call`/`call_indirect`: the result word goes to the caller's register
    /// (a void callee leaves it untouched, LSB §4.2).
    Write(u16),
    /// `dcall`: the result converted by `to_dyn` (`nil` for void).
    Dyn(u16),
    /// A value-returning hook: the `dyn` result as is.
    Value(u16),
    /// A predicate hook (`eq`, `lt`, `le`, `truthy`, `has_prop`): the result
    /// must be a `dyn` bool; `negate` serves `dne` and `dlnot`.
    Bool { dst: u16, negate: bool },
    /// The `len` hook: the result must be a `dyn` int, written as `i64`.
    Len(u16),
    /// The `iter` hook: an array or map is wrapped in a dynamic iterator, an
    /// iterator is used as is.
    Iter(u16),
    /// A void hook (`set_index`, `set_prop`).
    Discard,
    /// The body frame of a coroutine: returning from it (or an error escaping
    /// it) finishes the innermost running coroutine (LSB §5.13 rule 3), and
    /// the result goes to whoever drove it ([`Active::driver`]).
    Coro,
}

/// Who resumed a running coroutine, and so receives what it produces next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Driver {
    /// `resume`, `resume_throw`, or `coro_close`: the payload or result goes
    /// to this register of the resumer's frame.
    Resume(u16),
    /// `iter_next` on an iterator over the coroutine (rule 7).
    Iter {
        /// The `has` register.
        has: u16,
        /// The value register.
        val: u16,
        /// The iterator (its key is set from the coroutine's).
        iter: u64,
    },
    /// The host (the built-in scheduler): the run stops and reports a
    /// [`Stop`].
    Host,
    /// A drop-close started by the VM (rule 13): the outcome is discarded
    /// and the interrupted frame continues where it was.
    Finalize,
}

/// A running coroutine: one link of the resume chain.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Active {
    /// The coroutine object.
    pub(crate) coro: u64,
    /// Index in the frame stack of its body frame.
    pub(crate) floor: usize,
    pub(crate) driver: Driver,
}

/// What a coroutine driven by the host did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Stop {
    Yielded(u64),
    Awaiting(u64),
    Returned(u64),
    /// An error value escaped its body: the value, and the function and pc
    /// where it was raised (for an uncaught non-error value).
    Failed(u64, u32, u32),
    /// It yielded while being closed (rule 11): it stays suspended.
    CloseIgnored,
}

/// How the scheduler resumes a task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wake {
    Send(u64),
    Throw(u64),
}

/// The built-in scheduler's state (see [`Vm::run_async`]): tasks ready to
/// run in FIFO order, and tasks waiting for another task to finish.
///
/// [`Vm::run_async`]: crate::Vm::run_async
#[derive(Debug, Default)]
pub(crate) struct Sched {
    pub(crate) ready: VecDeque<(u64, Wake)>,
    /// Per awaited task, its waiters in the order they started waiting.
    pub(crate) waiters: BTreeMap<u64, Vec<u64>>,
}

impl Sched {
    /// Every word the scheduler holds (GC roots).
    fn words(&self) -> impl Iterator<Item = u64> + '_ {
        let ready = self.ready.iter().flat_map(|&(t, w)| {
            let v = match w {
                Wake::Send(v) | Wake::Throw(v) => v,
            };
            [t, v]
        });
        let waiting = self
            .waiters
            .iter()
            .flat_map(|(&t, ws)| core::iter::once(t).chain(ws.iter().copied()));
        ready.chain(waiting)
    }

    pub(crate) fn clear(&mut self) {
        self.ready.clear();
        self.waiters.clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.ready.len() + self.waiters.values().map(Vec::len).sum::<usize>()
    }
}

/// One activation record. Registers live in the shared stack at
/// `base..base + nregs`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub(crate) func: u32,
    /// The pc to resume at; for a frame below the top, its call instruction.
    pub(crate) pc: u32,
    pub(crate) base: usize,
    /// The running closure (for `get_upval`), or nil.
    pub(crate) closure: u64,
    pub(crate) cont: Cont,
}

/// The state of one VM instance.
#[derive(Debug)]
pub(crate) struct Machine {
    pub(crate) stack: Vec<u64>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) heap: Heap,
    pub(crate) globals: Vec<u64>,
    pub(crate) seed: Seed,
    /// Per constant: its string object (strings are immutable and shared).
    pub(crate) str_consts: Vec<u64>,
    /// Per constant: its `dyn` template (aggregates) or boxed int.
    pub(crate) dyn_consts: Vec<u64>,
    /// Per (constant, array/map type): its typed template.
    pub(crate) typed_consts: BTreeMap<(u32, u32), u64>,
    /// Per string id: its string object (property names passed to hooks and
    /// used as map keys).
    pub(crate) names: Vec<u64>,
    /// Per function: its function value without captures (method values).
    pub(crate) fn_refs: Vec<u64>,
    /// Per import: its function value.
    pub(crate) imp_refs: Vec<u64>,
    /// Pooled argument buffers.
    pub(crate) scratch: Vec<u64>,
    pub(crate) host_args: Vec<Value>,
    pub(crate) max_depth: usize,
    pub(crate) max_stack: usize,
    /// The resume chain: running coroutines, outermost first.
    pub(crate) coros: Vec<Active>,
    /// Dropped suspended coroutines waiting to be closed (rule 13), oldest
    /// first.
    pub(crate) finalize: VecDeque<u64>,
    /// A drop-close is running (they run one at a time, in order).
    pub(crate) finalizing: bool,
    /// `finalize` is non-empty and no drop-close is running: the one flag the
    /// dispatch loop tests at each frame transition.
    pub(crate) close_ready: bool,
    /// The fuel left in the current run (LSB §5.14).
    pub(crate) fuel: u64,
    /// Fuel set aside while a drop-close is armed ([`Machine::arm_close`]).
    pub(crate) bank: Option<u64>,
    /// What the coroutine driven by the host did (set by its delivery).
    pub(crate) outcome: Option<Stop>,
    pub(crate) sched: Sched,
    /// The next coroutine's creation number.
    pub(crate) coro_seq: u64,
    /// Pooled list of coroutines a collection found dropped.
    pub(crate) dropped: Vec<u64>,
}

/// The outcome of the cold branch of a fuel charge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Charge {
    /// Charged; continue.
    Done,
    /// The budget is spent: the `OutOfFuel` trap.
    Spent,
    /// A dropped coroutine's close was started on top of the frames, before
    /// the charging instruction: raise its close signal there.
    Close,
}

/// What a returning frame leads to.
pub(crate) enum Next {
    /// The run is over with this result.
    Exit(Option<u64>),
    /// Continue with the (new) top frame.
    Resume,
}

impl Machine {
    pub(crate) fn new(prog: &Program, memory: usize) -> Machine {
        let m = &prog.module;
        Machine {
            stack: Vec::new(),
            frames: Vec::new(),
            heap: Heap::new(memory),
            globals: alloc::vec![0; m.globals().len()],
            seed: Seed::new(),
            str_consts: alloc::vec![0; m.consts().len()],
            dyn_consts: alloc::vec![0; m.consts().len()],
            typed_consts: BTreeMap::new(),
            names: alloc::vec![0; m.string_count()],
            fn_refs: alloc::vec![0; m.functions().len()],
            imp_refs: alloc::vec![0; m.imports().len()],
            scratch: Vec::new(),
            host_args: Vec::new(),
            max_depth: 0,
            max_stack: 0,
            coros: Vec::new(),
            finalize: VecDeque::new(),
            finalizing: false,
            close_ready: false,
            fuel: 0,
            bank: None,
            outcome: None,
            sched: Sched::default(),
            coro_seq: 0,
            dropped: Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Frames
    // -----------------------------------------------------------------------

    /// Pushes a frame for `func` with every register at its default (zero)
    /// and returns its base; the caller copies the arguments in.
    #[inline]
    pub(crate) fn push_frame(
        &mut self,
        stack: &mut Vec<u64>,
        prog: &Program,
        func: u32,
        closure: u64,
        cont: Cont,
    ) -> Result<usize, Fault> {
        let nregs = prog.func(func).map_or(0, |f| f.nregs);
        let base = stack.len();
        if self.frames.len() >= self.max_depth || base + nregs > self.max_stack {
            return Err(Fault::raise(ErrorKind::StackOverflow));
        }
        stack.resize(base + nregs, 0);
        self.frames.push(Frame {
            func,
            pc: 0,
            base,
            closure,
            cont,
        });
        Ok(base)
    }

    /// Pops the top frame after `ret`/`ret_void` and delivers the result.
    ///
    /// A coroutine body's result is converted to `dyn` before its frame is
    /// given up: on a conversion error (a `u64` above `i64::MAX`) the frame is
    /// back on top and the caller raises the error there, at the `ret`.
    pub(crate) fn finish(
        &mut self,
        stack: &mut Vec<u64>,
        result: Option<u64>,
        ty: Option<ValType>,
    ) -> Result<Next, Fault> {
        let Some(frame) = self.frames.pop() else {
            return Ok(Next::Exit(result));
        };
        match frame.cont {
            Cont::Entry => {
                stack.truncate(frame.base);
                return Ok(Next::Exit(result));
            }
            Cont::Coro => return self.finish_coro(stack, frame, result, ty),
            _ => {}
        }
        stack.truncate(frame.base);
        self.apply_cont(stack, frame.cont, result, ty)?;
        if let Some(top) = self.frames.last_mut() {
            top.pc += 1;
        }
        Ok(Next::Resume)
    }

    /// [`finish`](Machine::finish) of a coroutine's body frame (rule 3).
    #[cold]
    #[inline(never)]
    fn finish_coro(
        &mut self,
        stack: &mut Vec<u64>,
        frame: Frame,
        result: Option<u64>,
        ty: Option<ValType>,
    ) -> Result<Next, Fault> {
        let value = match (result, ty) {
            (Some(v), Some(t)) => match conv::to_dyn(&mut self.heap, t, v) {
                Ok(d) => d,
                Err(f) => {
                    // Back on top; the caller records the `ret`'s pc.
                    self.frames.push(frame);
                    return Err(f);
                }
            },
            _ => dynv::NIL,
        };
        stack.truncate(frame.base);
        self.coro_returned(stack, value);
        Ok(Next::Resume)
    }

    /// Delivers a call or hook result into the top frame.
    pub(crate) fn apply_cont(
        &mut self,
        stack: &mut [u64],
        cont: Cont,
        result: Option<u64>,
        ty: Option<ValType>,
    ) -> Result<(), Fault> {
        let base = self.frames.last().map_or(0, |f| f.base);
        let (dst, v) = match cont {
            Cont::Entry | Cont::Discard | Cont::Coro => return Ok(()),
            Cont::Write(dst) => match result {
                Some(v) => (dst, v),
                None => return Ok(()),
            },
            Cont::Dyn(dst) => (
                dst,
                match (result, ty) {
                    (Some(v), Some(t)) => conv::to_dyn(&mut self.heap, t, v)?,
                    _ => dynv::NIL,
                },
            ),
            Cont::Value(dst) => (dst, result.unwrap_or(dynv::NIL)),
            Cont::Bool { dst, negate } => {
                let b = result
                    .and_then(conv::dyn_bool)
                    .ok_or_else(Fault::type_error)?;
                (dst, u64::from(b != negate))
            }
            Cont::Len(dst) => {
                let i = result
                    .and_then(|v| conv::dyn_int(&self.heap, v))
                    .ok_or_else(Fault::type_error)?;
                (dst, i as u64)
            }
            Cont::Iter(dst) => {
                let v = result.unwrap_or(dynv::NIL);
                let it = if matches!(self.heap.get(v), Some(Object::Iter(_))) {
                    v
                } else {
                    match coll::new_iter(&mut self.heap, v, true) {
                        Some(r) => r?,
                        None => return Err(Fault::type_error()),
                    }
                };
                (dst, it)
            }
        };
        if let Some(slot) = stack.get_mut(base + usize::from(dst)) {
            *slot = v;
        }
        Ok(())
    }

    /// Unwinds to the nearest handler covering the faulting pc (LSB §4.3),
    /// or ends the run.
    pub(crate) fn unwind(
        &mut self,
        stack: &mut Vec<u64>,
        prog: &Program,
        fault: Fault,
    ) -> Result<(), VmError> {
        let (func, pc) = self.frames.last().map_or((0, 0), |f| (f.func, f.pc));
        let value = match fault {
            Fault::Raise(kind) => match self.heap.alloc(Object::Error(ErrorObj { kind, func, pc }))
            {
                Ok(v) => v,
                Err(_) => {
                    return Err(VmError::Trap {
                        kind: ErrorKind::OutOfMemory,
                        func: FuncId(func),
                        pc,
                    });
                }
            },
            Fault::Throw(v) => v,
            Fault::Trap(kind) => {
                return Err(VmError::Trap {
                    kind,
                    func: FuncId(func),
                    pc,
                });
            }
        };
        loop {
            let Some(frame) = self.frames.last_mut() else {
                return Err(self.uncaught(value, func, pc));
            };
            let handlers = prog
                .module
                .function(FuncId(frame.func))
                .map_or(&[][..], bytecode_lang::Function::handlers);
            let at = frame.pc;
            let found = match prog.func(frame.func).and_then(|f| f.catch_table.as_deref()) {
                Some(table) => table
                    .get(at as usize)
                    .and_then(|&i| handlers.get(i as usize)),
                None => handlers.iter().find(|h| h.start <= at && at < h.end),
            };
            if let Some(h) = found {
                // Entering a handler is a control transfer that may go
                // backward, so it costs fuel like a back edge (a pending
                // close stays armed for the next charge).
                let (target, catch, func) = (h.target.0, h.catch, frame.func);
                if self.fuel == 0 {
                    if self.charge_slow(stack, None) == Charge::Spent {
                        return Err(VmError::Trap {
                            kind: ErrorKind::OutOfFuel,
                            func: FuncId(func),
                            pc: at,
                        });
                    }
                } else {
                    self.fuel -= 1;
                }
                let Some(frame) = self.frames.last_mut() else {
                    return Ok(());
                };
                frame.pc = target;
                let slot = frame.base + catch.index();
                if let Some(s) = stack.get_mut(slot) {
                    *s = value;
                }
                return Ok(());
            }
            let cont = frame.cont;
            let base = frame.base;
            let _ = self.frames.pop();
            stack.truncate(base);
            match cont {
                Cont::Entry => return Err(self.uncaught(value, func, pc)),
                // The error escapes a coroutine's body (rule 3): it fails,
                // unless it is being closed and the error is the close signal
                // itself (rule 11), and the search continues at whoever drove
                // it (the resumer's `resume`/`resume_throw`/`coro_close`/
                // `iter_next`, still at its pc), or stops for a driver that is
                // not a frame (the host scheduler, a drop-close).
                Cont::Coro if self.coro_escaped(stack, value, (func, pc)) => return Ok(()),
                _ => {}
            }
        }
    }

    /// The error for a value no handler caught.
    pub(crate) fn uncaught(&self, value: u64, func: u32, pc: u32) -> VmError {
        match self.heap.get(value) {
            Some(Object::Error(e)) => VmError::Raised {
                kind: e.kind,
                func: FuncId(e.func),
                pc: e.pc,
            },
            _ => VmError::Thrown {
                value: conv::dyn_value(&self.heap, value),
                func: FuncId(func),
                pc,
            },
        }
    }

    // -----------------------------------------------------------------------
    // Host calls
    // -----------------------------------------------------------------------

    /// Calls import `imp` with the arguments in `host_args`; returns the
    /// result as a word of the import's result type.
    pub(crate) fn call_host(&mut self, prog: &Program, imp: u32) -> Result<Option<u64>, Fault> {
        let Some(info) = prog.imports.get(imp as usize) else {
            return Err(Fault::type_error());
        };
        let res = match &info.func {
            HostFn::User(f) => {
                let Machine {
                    heap, host_args, ..
                } = self;
                let res = f(&mut HostCtx { heap }, host_args);
                host_args.clear();
                res
            }
            HostFn::Scheduler => {
                let arg = self.host_args.first().copied().unwrap_or(Value::Nil);
                self.host_args.clear();
                return self.spawn_task(arg).map(Some);
            }
        };
        match res {
            Ok(v) => match info.result {
                Some(t) => conv::slot_of(&mut self.heap, prog, t, v).map(Some),
                None => Ok(None),
            },
            Err(HostError::Raise(kind)) => Err(if kind.is_catchable() {
                Fault::Raise(kind)
            } else {
                Fault::Trap(kind)
            }),
            Err(HostError::Throw(v)) => Err(Fault::Throw(conv::dyn_of(&mut self.heap, v)?)),
        }
    }

    /// Calls a hook bound to an import with `dyn` operands.
    pub(crate) fn call_host_dyn(
        &mut self,
        prog: &Program,
        imp: u32,
        args: &[u64],
    ) -> Result<Option<u64>, Fault> {
        self.host_args.clear();
        for &a in args {
            let v = conv::dyn_value(&self.heap, a);
            self.host_args.push(v);
        }
        self.call_host(prog, imp)
    }

    // -----------------------------------------------------------------------
    // Cached objects
    // -----------------------------------------------------------------------

    /// The string object of a (canonical) string id.
    pub(crate) fn name_str(&mut self, prog: &Program, id: u32) -> Result<u64, Fault> {
        if let Some(&v) = self.names.get(id as usize) {
            if v != 0 {
                return Ok(v);
            }
        }
        let v = self.heap.alloc_str(prog.string(id).as_bytes())?;
        if let Some(slot) = self.names.get_mut(id as usize) {
            *slot = v;
        }
        Ok(v)
    }

    /// The function value of a function (no captures).
    pub(crate) fn fn_ref(&mut self, func: u32) -> Result<u64, Fault> {
        if let Some(&v) = self.fn_refs.get(func as usize) {
            if v != 0 {
                return Ok(v);
            }
        }
        let v = self.heap.alloc(Object::Func(FuncObj {
            target: Callable::Func(func),
            captures: alloc::boxed::Box::new([]),
        }))?;
        if let Some(slot) = self.fn_refs.get_mut(func as usize) {
            *slot = v;
        }
        Ok(v)
    }

    /// The function value of an import.
    pub(crate) fn import_ref(&mut self, imp: u32) -> Result<u64, Fault> {
        if let Some(&v) = self.imp_refs.get(imp as usize) {
            if v != 0 {
                return Ok(v);
            }
        }
        let v = self.heap.alloc(Object::Func(FuncObj {
            target: Callable::Import(imp),
            captures: alloc::boxed::Box::new([]),
        }))?;
        if let Some(slot) = self.imp_refs.get_mut(imp as usize) {
            *slot = v;
        }
        Ok(v)
    }

    // -----------------------------------------------------------------------
    // Constants (LSB §3.2)
    // -----------------------------------------------------------------------

    /// The shared string object of a `str` or `bytes` constant.
    fn const_str(&mut self, prog: &Program, k: u32) -> Result<u64, Fault> {
        if let Some(&v) = self.str_consts.get(k as usize) {
            if v != 0 {
                return Ok(v);
            }
        }
        let v = match prog.module.constant(ConstId(k)) {
            Some(Const::Str(s)) => {
                let text = prog.module.string(*s).unwrap_or("");
                self.heap.alloc_str(text.as_bytes())?
            }
            Some(Const::Bytes(b)) => self.heap.alloc_str(b)?,
            _ => return Err(Fault::type_error()),
        };
        if let Some(slot) = self.str_consts.get_mut(k as usize) {
            *slot = v;
        }
        Ok(v)
    }

    /// `load_const` into a register of declared type `ty`.
    pub(crate) fn load_const(&mut self, prog: &Program, k: u32, ty: ValType) -> Result<u64, Fault> {
        match prog.module.constant(ConstId(k)) {
            Some(Const::Array(_) | Const::Map(_)) => match ty {
                ValType::Ref(t) => {
                    let template = self.template(prog, k, Some(t.0))?;
                    self.fresh(template)
                }
                _ => Err(Fault::type_error()),
            },
            _ => self.scalar_const(prog, k, ty),
        }
    }

    /// `dload_const`.
    pub(crate) fn dload_const(&mut self, prog: &Program, k: u32) -> Result<u64, Fault> {
        match prog.module.constant(ConstId(k)) {
            Some(Const::Array(_) | Const::Map(_)) => {
                let template = self.template(prog, k, None)?;
                self.fresh(template)
            }
            _ => self.dyn_scalar_const(prog, k),
        }
    }

    /// A non-aggregate constant in the typed representation of `ty`.
    fn scalar_const(&mut self, prog: &Program, k: u32, ty: ValType) -> Result<u64, Fault> {
        let norm = |bits: u64| match ty.as_int() {
            Some(it) => int::normalize(it, bits),
            None => bits,
        };
        Ok(match prog.module.constant(ConstId(k)) {
            Some(Const::Bool(b)) => u64::from(*b),
            Some(Const::Int(i)) => norm(*i as u64),
            Some(Const::UInt(u)) => norm(*u),
            Some(Const::F32(bits)) => u64::from(*bits),
            Some(Const::F64(bits)) => *bits,
            Some(Const::Char(c)) => u64::from(*c as u32),
            Some(Const::Str(_) | Const::Bytes(_)) => self.const_str(prog, k)?,
            _ => return Err(Fault::type_error()),
        })
    }

    /// A non-aggregate constant as a `dyn` word.
    fn dyn_scalar_const(&mut self, prog: &Program, k: u32) -> Result<u64, Fault> {
        Ok(match prog.module.constant(ConstId(k)) {
            Some(Const::Bool(b)) => dynv::from_bool(*b),
            Some(Const::Int(i)) => self.const_int(k, *i)?,
            Some(Const::UInt(u)) => match i64::try_from(*u) {
                Ok(i) => self.const_int(k, i)?,
                Err(_) => return Err(Fault::raise(ErrorKind::ArithOverflow)),
            },
            Some(Const::F32(bits)) => dynv::from_f64(f64::from(f32::from_bits(*bits))),
            Some(Const::F64(bits)) => dynv::from_f64(f64::from_bits(*bits)),
            Some(Const::Char(c)) => dynv::from_char(*c as u32),
            Some(Const::Str(_) | Const::Bytes(_)) => self.const_str(prog, k)?,
            _ => return Err(Fault::type_error()),
        })
    }

    /// A `dyn` int constant, its box cached.
    fn const_int(&mut self, k: u32, i: i64) -> Result<u64, Fault> {
        if let Some(v) = dynv::inline_int(i) {
            return Ok(v);
        }
        if let Some(&v) = self.dyn_consts.get(k as usize) {
            if v != 0 {
                return Ok(v);
            }
        }
        let v = self.heap.alloc(Object::Int(i))?;
        if let Some(slot) = self.dyn_consts.get_mut(k as usize) {
            *slot = v;
        }
        Ok(v)
    }

    /// The frozen template object of an aggregate constant, materialised once
    /// per representation (`target` is the array/map type, `None` for
    /// `dyn`). Nested aggregates are templates too, so a DAG of constants
    /// costs one object per (constant, type), never an exponential copy.
    /// Recursion is bounded by the loader's constant-depth check.
    fn template(&mut self, prog: &Program, k: u32, target: Option<u32>) -> Result<u64, Fault> {
        let cached = match target {
            None => self.dyn_consts.get(k as usize).copied().unwrap_or(0),
            Some(t) => self.typed_consts.get(&(k, t)).copied().unwrap_or(0),
        };
        if cached != 0 {
            return Ok(cached);
        }
        let shape = target.map(|t| prog.types.get(t as usize));
        let v = match prog.module.constant(ConstId(k)) {
            Some(Const::Array(items)) => {
                let elem = match shape {
                    None => ValType::Dyn,
                    Some(Some(TypeInfo::Array(e))) => *e,
                    Some(_) => return Err(Fault::type_error()),
                };
                if !self.heap.fits(items.len().saturating_mul(8)) {
                    return Err(Fault::Trap(ErrorKind::OutOfMemory));
                }
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.element(prog, item.0, elem)?);
                }
                self.heap.alloc(Object::Array(ArrayObj {
                    elem,
                    frozen: true,
                    items: Arc::new(values),
                }))?
            }
            Some(Const::Map(entries)) => {
                let (kty, vty) = match shape {
                    None => (ValType::Dyn, ValType::Dyn),
                    Some(Some(TypeInfo::Map(kt, vt))) => (*kt, *vt),
                    Some(_) => return Err(Fault::type_error()),
                };
                let map = coll::new_map(&mut self.heap, kty, vty)?;
                for (ck, cv) in entries {
                    let key = self.element(prog, ck.0, kty)?;
                    let value = self.element(prog, cv.0, vty)?;
                    coll::map_set(&mut self.heap, self.seed, map, key, value)?;
                }
                if let Some(Object::Map(m)) = self.heap.get_mut(map) {
                    m.frozen = true;
                }
                map
            }
            _ => return Err(Fault::type_error()),
        };
        match target {
            None => {
                if let Some(slot) = self.dyn_consts.get_mut(k as usize) {
                    *slot = v;
                }
            }
            Some(t) => {
                let _previous = self.typed_consts.insert((k, t), v);
            }
        }
        Ok(v)
    }

    /// One element of an aggregate constant, in the representation `ty`.
    fn element(&mut self, prog: &Program, k: u32, ty: ValType) -> Result<u64, Fault> {
        let aggregate = matches!(
            prog.module.constant(ConstId(k)),
            Some(Const::Array(_) | Const::Map(_))
        );
        match (ty, aggregate) {
            (ValType::Dyn, true) => self.template(prog, k, None),
            (ValType::Dyn, false) => self.dyn_scalar_const(prog, k),
            (ValType::Ref(t), true) => self.template(prog, k, Some(t.0)),
            (_, true) => Err(Fault::type_error()),
            (_, false) => self.scalar_const(prog, k, ty),
        }
    }

    /// A new, mutable object sharing a template's storage.
    fn fresh(&mut self, template: u64) -> Result<u64, Fault> {
        let obj = match self.heap.get(template) {
            Some(Object::Array(a)) => Object::Array(ArrayObj {
                elem: a.elem,
                frozen: false,
                items: Arc::clone(&a.items),
            }),
            Some(Object::Map(m)) => Object::Map(MapObj {
                key: m.key,
                value: m.value,
                frozen: false,
                store: Arc::clone(&m.store),
            }),
            _ => return Err(Fault::type_error()),
        };
        self.heap.alloc(obj)
    }

    /// Initialises the globals from their initialisers (LSB §2.3: absent
    /// initialisers leave the default).
    pub(crate) fn init_globals(&mut self, prog: &Program) -> Result<(), (u32, Fault)> {
        for (i, g) in prog.module.globals().iter().enumerate() {
            let gi = u32::try_from(i).unwrap_or(u32::MAX);
            let ty = prog.global_types.get(i).copied().unwrap_or(ValType::Dyn);
            let v = match g.init {
                None => 0,
                Some(k) => {
                    let r = if ty == ValType::Dyn {
                        self.dload_const(prog, k.0)
                    } else {
                        self.load_const(prog, k.0, ty)
                    };
                    r.map_err(|f| (gi, f))?
                }
            };
            if let Some(slot) = self.globals.get_mut(i) {
                *slot = v;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Garbage collection
    // -----------------------------------------------------------------------

    /// Collects with every root the machine holds; `stack` is the register
    /// stack the frames index (the interpreter holds it while running).
    pub(crate) fn collect(&mut self, stack: &[u64], prog: &Program) {
        let Machine {
            frames,
            heap,
            globals,
            str_consts,
            dyn_consts,
            typed_consts,
            names,
            fn_refs,
            imp_refs,
            coros,
            finalize,
            sched,
            dropped,
            ..
        } = self;
        let frame_roots = frames.iter().flat_map(|f| {
            let regs = prog.func(f.func).map_or(&[][..], |i| &i.ref_regs[..]);
            let base = f.base;
            regs.iter()
                .map(move |&r| stack.get(base + usize::from(r)).copied().unwrap_or(0))
                .chain(core::iter::once(f.closure))
        });
        let global_roots = prog
            .global_refs
            .iter()
            .map(|&g| globals.get(g as usize).copied().unwrap_or(0));
        let caches = str_consts
            .iter()
            .chain(dyn_consts.iter())
            .chain(typed_consts.values())
            .chain(names.iter())
            .chain(fn_refs.iter())
            .chain(imp_refs.iter())
            .copied();
        // Running coroutines (and the iterators driving them), coroutines
        // waiting to be closed, and the scheduler's tasks.
        let coroutines = coros
            .iter()
            .flat_map(|a| {
                let iter = match a.driver {
                    Driver::Iter { iter, .. } => iter,
                    _ => dynv::NIL,
                };
                [a.coro, iter]
            })
            .chain(finalize.iter().copied())
            .chain(sched.words());
        dropped.clear();
        heap.collect(
            frame_roots
                .chain(global_roots)
                .chain(caches)
                .chain(coroutines),
            prog,
            dropped,
        );
        finalize.extend(dropped.iter().copied());
        self.close_ready = !self.finalizing && !self.finalize.is_empty();
        self.arm_close();
    }
}
