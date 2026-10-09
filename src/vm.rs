//! The public VM: an instance of a [`Program`] with its own heap, globals,
//! and limits.

use alloc::vec::Vec;

use bytecode_lang::{CoroState, FuncId, GlobalId, Kind, ValType};

use crate::conv;
use crate::dynv;
use crate::error::VmError;
use crate::exec;
use crate::fault::Fault;
use crate::heap::Object;
use crate::host;
use crate::machine::{Machine, Stop, Wake};
use crate::program::Program;
use crate::value::{Obj, Value};

/// The budgets a run executes under.
///
/// Every budget is enforced deterministically, so an untrusted module cannot
/// run forever, exhaust memory, or overflow the native stack:
///
/// - **fuel**: units charged at every `safepoint`, call-family instruction
///   (calls, tail calls, host calls, hook invocations), taken backward
///   branch, handler entry, coroutine instruction, and `iter_next` over a
///   coroutine (the rule of LSB §5.14). Exhausting it is the `OutOfFuel`
///   trap (E0107). Execution between two charges is bounded by the length of
///   one function, so fuel bounds total work.
/// - **memory**: bytes of heap objects (strings, arrays, maps, structs,
///   closures, cells, iterators, error values, boxed ints). Exceeding it is
///   the `OutOfMemory` trap (E0106). Accounting is approximate (headers and
///   shared copy-on-write storage are estimated), never unbounded.
/// - **depth**: frames on the call stack, hook frames and the frames of
///   running coroutines included (a suspended coroutine's frames count again
///   when it is resumed). Exceeding it raises `StackOverflow` (E0105,
///   catchable).
/// - **stack**: register slots across all frames (8 bytes each). Exceeding
///   it also raises `StackOverflow`. Suspended coroutines' registers count
///   against the memory budget instead.
///
/// # Examples
///
/// ```
/// use bvm_lang::Limits;
///
/// let tight = Limits::new().with_fuel(10_000).with_memory(1 << 20).with_depth(64);
/// assert_eq!(tight.fuel(), 10_000);
/// assert_eq!(Limits::default().fuel(), u64::MAX);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    fuel: u64,
    memory: usize,
    depth: usize,
    stack: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::new()
    }
}

impl Limits {
    /// The defaults: unlimited fuel, 1 GiB of heap, 10,000 frames, and 4 Mi
    /// register slots (32 MiB). Set fuel and memory explicitly for untrusted
    /// code.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Limits;
    ///
    /// assert_eq!(Limits::new().depth(), 10_000);
    /// ```
    #[must_use]
    pub const fn new() -> Limits {
        Limits {
            fuel: u64::MAX,
            memory: 1 << 30,
            depth: 10_000,
            stack: 4 << 20,
        }
    }

    /// Sets the fuel budget of each run.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().with_fuel(5).fuel(), 5);
    /// ```
    #[must_use]
    pub const fn with_fuel(self, fuel: u64) -> Limits {
        Limits { fuel, ..self }
    }

    /// Sets the heap budget in bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().with_memory(4096).memory(), 4096);
    /// ```
    #[must_use]
    pub const fn with_memory(self, bytes: usize) -> Limits {
        Limits {
            memory: bytes,
            ..self
        }
    }

    /// Sets the maximum call depth (at least 1, the entry frame).
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().with_depth(0).depth(), 1);
    /// ```
    #[must_use]
    pub const fn with_depth(self, frames: usize) -> Limits {
        Limits {
            depth: if frames == 0 { 1 } else { frames },
            ..self
        }
    }

    /// Sets the maximum number of register slots across all frames.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().with_stack(1024).stack(), 1024);
    /// ```
    #[must_use]
    pub const fn with_stack(self, slots: usize) -> Limits {
        Limits {
            stack: slots,
            ..self
        }
    }

    /// The fuel budget.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().fuel(), u64::MAX);
    /// ```
    #[must_use]
    pub const fn fuel(&self) -> u64 {
        self.fuel
    }

    /// The heap budget in bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().memory(), 1 << 30);
    /// ```
    #[must_use]
    pub const fn memory(&self) -> usize {
        self.memory
    }

    /// The maximum call depth.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().depth(), 10_000);
    /// ```
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// The maximum register slots.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().stack(), 4 << 20);
    /// ```
    #[must_use]
    pub const fn stack(&self) -> usize {
        self.stack
    }
}

/// An instance of a [`Program`]: its heap, globals, and limits.
///
/// The first run initialises the globals and runs the module's start function
/// (LSB §3); later runs reuse the instance, so globals and heap objects
/// persist between runs. Register stacks and work lists are pooled, so steady
/// state execution does not allocate outside the program's own objects.
///
/// [`Value::Obj`] handles a run returns stay valid until the next run on the
/// same VM (collection happens only while bytecode runs).
///
/// # Examples
///
/// ```
/// use bvm_lang::{Host, Program, Value, Vm};
/// use bytecode_lang::{Inst, IntOp, IntTy, ModuleBuilder, ValType};
///
/// // fn square(x: i64) -> i64 { x * x }
/// let mut m = ModuleBuilder::new();
/// let mut f = m.function("square", &[ValType::I64], &[ValType::I64]);
/// let r = f.reg(ValType::I64);
/// f.emit(Inst::IMul { dst: r, lhs: f.param(0), rhs: f.param(0), op: IntOp::new(IntTy::I64) });
/// f.ret(r);
/// let square = m.add_function(f).unwrap();
///
/// let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
/// let mut vm = Vm::new(&program);
/// assert_eq!(vm.run(square, &[Value::Int(12)]), Ok(Value::Int(144)));
/// ```
#[derive(Debug)]
pub struct Vm<'p> {
    prog: &'p Program,
    m: Machine,
    limits: Limits,
    ready: bool,
    fuel_used: u64,
}

impl<'p> Vm<'p> {
    /// An instance of `program` with the default [`Limits`].
    ///
    /// # Examples
    ///
    /// See [`Vm`].
    #[must_use]
    pub fn new(program: &'p Program) -> Vm<'p> {
        Vm::with_limits(program, Limits::new())
    }

    /// An instance of `program` with `limits` for every run.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Limits, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// let vm = Vm::with_limits(&p, Limits::new().with_fuel(1_000));
    /// assert_eq!(vm.limits().fuel(), 1_000);
    /// ```
    #[must_use]
    pub fn with_limits(program: &'p Program, limits: Limits) -> Vm<'p> {
        Vm {
            prog: program,
            m: Machine::new(program, limits.memory),
            limits,
            ready: false,
            fuel_used: 0,
        }
    }

    /// The limits runs use by default.
    ///
    /// # Examples
    ///
    /// See [`with_limits`](Vm::with_limits).
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Replaces the default limits.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Limits, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// vm.set_limits(Limits::new().with_depth(16));
    /// assert_eq!(vm.limits().depth(), 16);
    /// ```
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// The program this VM runs.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// assert!(core::ptr::eq(Vm::new(&p).program(), &p));
    /// ```
    #[must_use]
    pub fn program(&self) -> &'p Program {
        self.prog
    }

    /// Runs `func` with `args` under the VM's limits and returns its result
    /// (`Value::Nil` for a void function).
    ///
    /// # Errors
    ///
    /// A [`VmError`]: an uncaught error or throw, a trap, an unsupported
    /// instruction, or an entry problem (unknown function, wrong arguments).
    ///
    /// # Examples
    ///
    /// See [`Vm`].
    pub fn run(&mut self, func: FuncId, args: &[Value]) -> Result<Value, VmError> {
        self.run_with(func, args, self.limits)
    }

    /// Runs the function exported under `name`.
    ///
    /// # Errors
    ///
    /// [`VmError::NoSuchExport`], or as [`run`](Vm::run).
    ///
    /// # Examples
    ///
    /// See [`Program`].
    pub fn run_export(&mut self, name: &str, args: &[Value]) -> Result<Value, VmError> {
        let func = self.prog.export(name).ok_or(VmError::NoSuchExport)?;
        self.run(func, args)
    }

    /// Runs `func` under `limits` for this run only (fuel restarts at the
    /// budget; the memory budget applies to the whole heap).
    ///
    /// # Errors
    ///
    /// As [`run`](Vm::run).
    ///
    /// # Examples
    ///
    /// An infinite loop stops when its fuel is spent:
    ///
    /// ```
    /// use bvm_lang::{Host, Limits, Program, Vm, VmError};
    /// use bytecode_lang::{ErrorKind, Inst, ModuleBuilder, Target};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let mut f = m.function("spin", &[], &[]);
    /// f.emit(Inst::Jmp { target: Target(0) });
    /// let spin = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    ///
    /// let err = Vm::new(&p).run_with(spin, &[], Limits::new().with_fuel(1_000)).unwrap_err();
    /// assert_eq!(err, VmError::Trap { kind: ErrorKind::OutOfFuel, func: spin, pc: 0 });
    /// ```
    pub fn run_with(
        &mut self,
        func: FuncId,
        args: &[Value],
        limits: Limits,
    ) -> Result<Value, VmError> {
        self.m.heap.limit = limits.memory;
        self.m.max_depth = limits.depth;
        self.m.max_stack = limits.stack;
        let mut fuel = limits.fuel;
        let result = self.run_inner(func, args, &mut fuel);
        self.fuel_used = limits.fuel - fuel;
        result
    }

    fn run_inner(
        &mut self,
        func: FuncId,
        args: &[Value],
        fuel: &mut u64,
    ) -> Result<Value, VmError> {
        self.prepare(fuel)?;
        let words = self.entry_words(func, args)?;
        let prog = self.prog;
        let result = prog.func(func.0).and_then(|i| i.result);
        let out = exec::execute(&mut self.m, prog, func.0, &words, fuel)?;
        Ok(match (out, result) {
            (Some(w), Some(ty)) => conv::value_of(&self.m.heap, ty, w),
            _ => Value::Nil,
        })
    }

    /// The first run's set-up: globals from their initialisers, then the
    /// module's start function.
    fn prepare(&mut self, fuel: &mut u64) -> Result<(), VmError> {
        if self.ready {
            return Ok(());
        }
        let prog = self.prog;
        self.m
            .init_globals(prog)
            .map_err(|(g, f)| VmError::GlobalInit {
                global: GlobalId(g),
                kind: fault_kind(f),
            })?;
        if let Some(start) = prog.module.start() {
            let _ = exec::execute(&mut self.m, prog, start.0, &[], fuel)?;
        }
        self.ready = true;
        Ok(())
    }

    /// Checks an entry function and converts its arguments.
    fn entry_words(&mut self, func: FuncId, args: &[Value]) -> Result<Vec<u64>, VmError> {
        let prog = self.prog;
        let info = prog.func(func.0).ok_or(VmError::NoSuchFunction(func))?;
        if !info.captures.is_empty() {
            return Err(VmError::NeedsClosure(func));
        }
        if args.len() != info.nparams {
            return Err(VmError::ArgumentCount {
                expected: info.nparams,
                found: args.len(),
            });
        }
        let mut words = Vec::with_capacity(args.len());
        for (index, (&v, &ty)) in args.iter().zip(info.regs.iter()).enumerate() {
            let w = conv::slot_of(&mut self.m.heap, prog, ty, v)
                .map_err(|_| VmError::ArgumentType { index })?;
            words.push(w);
        }
        Ok(words)
    }

    /// Runs `func` as the main task of the VM's built-in scheduler and
    /// returns its result (Python's `asyncio.run`, a Mox request).
    ///
    /// `func` becomes a coroutine (so it may `await`); the scheduler resumes
    /// ready tasks one at a time in FIFO order until the main task finishes.
    /// The rules, all deterministic:
    ///
    /// - `spawn` (with the module's `spawn` hook bound to the import
    ///   registered by [`Host::register_scheduler`]) makes the new coroutine a
    ///   task at the back of the queue and returns it as the task handle.
    /// - `await` of a task (or of any coroutine, which then becomes a task)
    ///   waits until it finishes, then resumes with its return value, or
    ///   raises its error at the `await` (`resume_throw`). Awaiting a
    ///   finished coroutine resumes at once, at the back of the queue.
    /// - `await` of any other value resumes, at the back of the queue, with
    ///   that same value (a cooperative "let the others run").
    /// - A task that `yield`s is resumed with `nil` at the back of the queue.
    /// - A task that fails wakes its waiters with its error; the main task
    ///   failing ends the run with that error.
    ///
    /// Tasks still unfinished when the main task finishes are dropped (and,
    /// once collected, closed like any dropped coroutine). Tasks spawned by
    /// a plain [`run`](Vm::run) wait in the queue for the next `run_async`.
    /// Fuel is one budget for the whole run, every task included.
    ///
    /// # Errors
    ///
    /// As [`run`](Vm::run), plus [`VmError::Deadlock`] when the main task
    /// waits and no task can run.
    ///
    /// # Examples
    ///
    /// `main` may `await` (here a plain value, which comes straight back);
    /// [`Host::register_scheduler`] shows tasks made with `spawn`.
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm, VmError};
    /// use bytecode_lang::{ErrorKind, Inst, ModuleBuilder, ValType};
    ///
    /// let d = ValType::Dyn;
    /// let mut m = ModuleBuilder::new();
    /// let mut f = m.function("main", &[d], &[d]);
    /// let r = f.reg(d);
    /// f.emit(Inst::Await { dst: r, src: f.param(0) });
    /// f.ret(r);
    /// let main = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// assert_eq!(vm.run_async(main, &[Value::Int(7)]), Ok(Value::Int(7)));
    /// // A plain run has no coroutine to suspend.
    /// assert_eq!(
    ///     vm.run(main, &[Value::Int(7)]),
    ///     Err(VmError::Raised { kind: ErrorKind::CannotSuspend, payload: Value::Nil, func: main, pc: 0 })
    /// );
    /// ```
    ///
    /// [`Host::register_scheduler`]: crate::Host::register_scheduler
    pub fn run_async(&mut self, func: FuncId, args: &[Value]) -> Result<Value, VmError> {
        let limits = self.limits;
        self.m.heap.limit = limits.memory;
        self.m.max_depth = limits.depth;
        self.m.max_stack = limits.stack;
        let mut fuel = limits.fuel;
        let result = self.async_inner(func, args, &mut fuel);
        self.m.sched.clear();
        self.fuel_used = limits.fuel - fuel;
        result
    }

    fn async_inner(
        &mut self,
        func: FuncId,
        args: &[Value],
        fuel: &mut u64,
    ) -> Result<Value, VmError> {
        self.prepare(fuel)?;
        let words = self.entry_words(func, args)?;
        let prog = self.prog;
        let main = self
            .m
            .coro_create(prog, func.0, dynv::NIL, &words)
            .map_err(|f| VmError::Trap {
                kind: fault_kind(f),
                func,
                pc: 0,
            })?;
        if let Some(c) = self.m.heap.coro_mut(main) {
            c.task = true;
        }
        self.m.sched.ready.push_back((main, Wake::Send(dynv::NIL)));
        loop {
            let Some((task, wake)) = self.m.sched.ready.pop_front() else {
                // Nothing is ready, so every task left (the main one
                // included) is waiting for another.
                return Err(VmError::Deadlock {
                    waiting: self.m.sched.len(),
                });
            };
            let stop = match self.m.heap.coro(task).map(|c| (c.state, c.result)) {
                Some((CoroState::Returned, v)) => Stop::Returned(v),
                Some((CoroState::Failed, e)) => Stop::Failed(e, func.0, 0),
                // A task some bytecode is running cannot be resumed from
                // here; it is not one the scheduler still owns.
                Some((CoroState::Running, _)) | None => continue,
                Some((CoroState::Created, _)) if matches!(wake, Wake::Throw(_)) => {
                    let Wake::Throw(e) = wake else { continue };
                    if let Some(c) = self.m.heap.coro_mut(task) {
                        c.state = CoroState::Failed;
                        c.result = e;
                        c.frames = Vec::new();
                        c.regs = Vec::new();
                    }
                    Stop::Failed(e, func.0, 0)
                }
                Some(_) => exec::drive(&mut self.m, prog, task, wake, fuel)?,
            };
            let sched = &mut self.m.sched;
            match stop {
                Stop::Yielded(_) | Stop::CloseIgnored => {
                    sched.ready.push_back((task, Wake::Send(dynv::NIL)));
                }
                Stop::Awaiting(target) => match self.m.heap.coro_mut(target) {
                    Some(t) => match t.state {
                        CoroState::Returned => sched.ready.push_back((task, Wake::Send(t.result))),
                        CoroState::Failed => sched.ready.push_back((task, Wake::Throw(t.result))),
                        _ => {
                            if !t.task && t.state.is_resumable() {
                                t.task = true;
                                sched.ready.push_back((target, Wake::Send(dynv::NIL)));
                            }
                            sched.waiters.entry(target).or_default().push(task);
                        }
                    },
                    None => sched.ready.push_back((task, Wake::Send(target))),
                },
                Stop::Returned(v) => {
                    if task == main {
                        return Ok(conv::dyn_value(&self.m.heap, v));
                    }
                    for w in sched.waiters.remove(&task).unwrap_or_default() {
                        sched.ready.push_back((w, Wake::Send(v)));
                    }
                }
                Stop::Failed(e, at_func, at_pc) => {
                    if task == main {
                        return Err(self.m.uncaught(e, at_func, at_pc));
                    }
                    for w in sched.waiters.remove(&task).unwrap_or_default() {
                        sched.ready.push_back((w, Wake::Throw(e)));
                    }
                }
            }
        }
    }

    /// Closes the dropped suspended coroutines a collection has queued
    /// (LSB §5.13 rule 13) now, under the VM's limits, instead of at the
    /// next safepoint of a later run; returns how many were closed. Call it
    /// after [`collect_garbage`](Vm::collect_garbage) to run the pending
    /// `finally` blocks of everything dropped (at program exit, say).
    ///
    /// # Errors
    ///
    /// A trap inside a `finally` block (`OutOfFuel`, `OutOfMemory`,
    /// `Unreachable`) stops the closing; the remaining coroutines stay queued.
    /// Errors raised by a `finally` block are discarded, as CPython discards
    /// an exception raised while a dropped generator closes.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// vm.collect_garbage();
    /// assert_eq!(vm.pending_finalizers(), 0);
    /// assert_eq!(vm.run_finalizers(), Ok(0));
    /// ```
    pub fn run_finalizers(&mut self) -> Result<usize, VmError> {
        let limits = self.limits;
        self.m.heap.limit = limits.memory;
        self.m.max_depth = limits.depth;
        self.m.max_stack = limits.stack;
        let mut fuel = limits.fuel;
        let result = exec::finalize_all(&mut self.m, self.prog, &mut fuel);
        self.fuel_used = limits.fuel - fuel;
        result
    }

    /// Dropped suspended coroutines waiting to be closed (see
    /// [`run_finalizers`](Vm::run_finalizers)).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// assert_eq!(Vm::new(&p).pending_finalizers(), 0);
    /// ```
    #[must_use]
    pub fn pending_finalizers(&self) -> usize {
        self.m.finalize.len()
    }

    /// The state of a coroutine value (`None` for anything else).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{CoroState, Inst, ModuleBuilder, ValType};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let mut body = m.function("body", &[], &[]);
    /// body.ret_void();
    /// let body = m.add_function(body).unwrap();
    /// let mut f = m.function("f", &[], &[ValType::Dyn]);
    /// let c = f.reg(ValType::Dyn);
    /// f.emit(Inst::CoroNew { dst: c, func: body, argc: 0 });
    /// f.ret(c);
    /// let f = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// let c = vm.run(f, &[]).unwrap();
    /// assert_eq!(vm.coro_state(c), Some(CoroState::Created));
    /// assert_eq!(vm.coro_state(Value::Int(1)), None);
    /// ```
    #[must_use]
    pub fn coro_state(&self, v: Value) -> Option<CoroState> {
        let Value::Obj(Obj(w)) = v else { return None };
        self.m.heap.coro(w).map(|c| c.state)
    }

    /// Fuel the last run consumed.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Vm};
    /// use bytecode_lang::{Inst, ModuleBuilder};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let mut f = m.function("f", &[], &[]);
    /// f.emit(Inst::Safepoint {});
    /// f.emit(Inst::Safepoint {});
    /// f.ret_void();
    /// let id = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// vm.run(id, &[]).unwrap();
    /// assert_eq!(vm.fuel_used(), 2);
    /// ```
    #[must_use]
    pub fn fuel_used(&self) -> u64 {
        self.fuel_used
    }

    /// Bytes of heap currently charged against the memory budget.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// let before = vm.heap_bytes();
    /// vm.new_str(&[0; 100]).unwrap();
    /// assert!(vm.heap_bytes() >= before + 100);
    /// ```
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.m.heap.used()
    }

    /// Live heap objects (exact after a collection).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// vm.new_str(b"x").unwrap();
    /// assert_eq!(vm.heap_objects(), 1);
    /// vm.collect_garbage();
    /// assert_eq!(vm.heap_objects(), 0); // nothing referenced it
    /// ```
    #[must_use]
    pub fn heap_objects(&self) -> usize {
        self.m.heap.len()
    }

    /// Collections run so far.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// vm.collect_garbage();
    /// assert_eq!(vm.collections(), 1);
    /// ```
    #[must_use]
    pub fn collections(&self) -> u64 {
        self.m.heap.stats.collections
    }

    /// Collects now, keeping what globals and constant caches reach. Values
    /// from earlier runs that nothing else references are freed (their
    /// handles then read as `nil`), except suspended coroutines, which are
    /// queued to be closed (LSB §5.13 rule 13): at the next run's first fuel
    /// charge, or now with [`run_finalizers`](Vm::run_finalizers).
    ///
    /// # Examples
    ///
    /// See [`heap_objects`](Vm::heap_objects).
    pub fn collect_garbage(&mut self) {
        let stack = core::mem::take(&mut self.m.stack);
        self.m.collect(&stack, self.prog);
        self.m.stack = stack;
    }

    /// The current value of a global.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{Const, ModuleBuilder, ValType};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let k = m.constant(Const::Int(7));
    /// let g = m.global("seven", ValType::I64, false, Some(k));
    /// let mut f = m.function("f", &[], &[]);
    /// f.ret_void();
    /// let id = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// vm.run(id, &[]).unwrap(); // the first run initialises globals
    /// assert_eq!(vm.global(g), Some(Value::Int(7)));
    /// ```
    #[must_use]
    pub fn global(&self, id: GlobalId) -> Option<Value> {
        let ty = *self.prog.global_types.get(id.index())?;
        let w = *self.m.globals.get(id.index())?;
        Some(conv::value_of(&self.m.heap, ty, w))
    }

    /// The dynamic kind of a value (LSB §2.2).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{Kind, ModuleBuilder};
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// let s = vm.new_str(b"abc").unwrap();
    /// assert_eq!(vm.kind(s), Kind::Str);
    /// assert_eq!(vm.kind(Value::Float(1.0)), Kind::Float);
    /// ```
    #[must_use]
    pub fn kind(&self, v: Value) -> Kind {
        host::value_kind(&self.m.heap, v)
    }

    /// The bytes of a string value.
    ///
    /// # Examples
    ///
    /// See [`kind`](Vm::kind).
    #[must_use]
    pub fn str_bytes(&self, v: Value) -> Option<&[u8]> {
        match v {
            Value::Obj(Obj(w)) => self.m.heap.str(w),
            _ => None,
        }
    }

    /// Allocates a string (to pass to a run).
    ///
    /// # Errors
    ///
    /// The `OutOfMemory` trap as [`VmError::Trap`] when the budget is spent.
    ///
    /// # Examples
    ///
    /// See [`kind`](Vm::kind).
    pub fn new_str(&mut self, bytes: &[u8]) -> Result<Value, VmError> {
        self.m.heap.limit = self.limits.memory;
        self.m
            .heap
            .alloc_str(bytes)
            .map(|w| Value::Obj(Obj(w)))
            .map_err(|f| VmError::Trap {
                kind: fault_kind(f),
                func: FuncId(u32::MAX),
                pc: 0,
            })
    }

    /// The elements of an array value, as values (a reference slot as its
    /// reference's value, LSB §5.17).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{Const, Inst, ModuleBuilder, ValType};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let one = m.constant(Const::Int(1));
    /// let two = m.constant(Const::Int(2));
    /// let arr = m.constant(Const::Array(vec![one, two]));
    /// let mut f = m.function("f", &[], &[ValType::Dyn]);
    /// let r = f.reg(ValType::Dyn);
    /// f.emit(Inst::DLoadConst { dst: r, k: arr });
    /// f.ret(r);
    /// let id = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// let v = vm.run(id, &[]).unwrap();
    /// assert_eq!(vm.elements(v), Some(vec![Value::Int(1), Value::Int(2)]));
    /// ```
    #[must_use]
    pub fn elements(&self, v: Value) -> Option<Vec<Value>> {
        let Value::Obj(Obj(w)) = v else { return None };
        match self.m.heap.get(w)? {
            Object::Array(a) => Some(
                a.items
                    .iter()
                    .map(|&x| conv::value_of(&self.m.heap, a.elem, self.slot(a.elem, x)))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The entries of a map value, in insertion order (reference slots as
    /// their values).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{Const, Inst, ModuleBuilder, ValType};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let k = m.constant(Const::Int(10));
    /// let v = m.constant(Const::Bool(true));
    /// let map = m.constant(Const::Map(vec![(k, v)]));
    /// let mut f = m.function("f", &[], &[ValType::Dyn]);
    /// let r = f.reg(ValType::Dyn);
    /// f.emit(Inst::DLoadConst { dst: r, k: map });
    /// f.ret(r);
    /// let id = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// let out = vm.run(id, &[]).unwrap();
    /// assert_eq!(vm.entries(out), Some(vec![(Value::Int(10), Value::Bool(true))]));
    /// ```
    #[must_use]
    pub fn entries(&self, v: Value) -> Option<Vec<(Value, Value)>> {
        let Value::Obj(Obj(w)) = v else { return None };
        match self.m.heap.get(w)? {
            Object::Map(m) => Some(
                m.store
                    .entries()
                    .iter()
                    .filter(|e| e.live)
                    .map(|e| {
                        (
                            conv::value_of(&self.m.heap, m.key, e.key),
                            conv::value_of(&self.m.heap, m.value, self.slot(m.value, e.value)),
                        )
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Field `index` of a struct value (a reference slot as its value).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{Field, FieldIdx, Inst, ModuleBuilder, StructDef, TypeDef, ValType};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let name = m.string("x");
    /// let point = m.add_type(TypeDef::Struct(StructDef {
    ///     name,
    ///     fields: vec![Field { name, ty: ValType::I64 }],
    ///     ..Default::default()
    /// }));
    /// let mut f = m.function("f", &[], &[ValType::Ref(point)]);
    /// let (obj, val) = (f.reg(ValType::Ref(point)), f.reg(ValType::I64));
    /// let ty = f.type_ref(point);
    /// f.emit(Inst::NewStruct { dst: obj, ty });
    /// f.emit(Inst::LoadInt { dst: val, val: 5, ty: bytecode_lang::IntTy::I64 });
    /// f.emit(Inst::SetField { obj, field: FieldIdx(0), src: val });
    /// f.ret(obj);
    /// let id = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// let out = vm.run(id, &[]).unwrap();
    /// assert_eq!(vm.field(out, 0), Some(Value::Int(5)));
    /// ```
    #[must_use]
    pub fn field(&self, v: Value, index: usize) -> Option<Value> {
        let Value::Obj(Obj(w)) = v else { return None };
        match self.m.heap.get(w)? {
            Object::Struct(s) => {
                let ty = *self.prog.struct_info(s.ty)?.fields.get(index)?;
                let w = self.slot(ty, *s.fields.get(index)?);
                Some(conv::value_of(&self.m.heap, ty, w))
            }
            _ => None,
        }
    }

    /// The error code of a runtime error value (`1` for E0001).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// assert_eq!(Vm::new(&p).error_code(Value::Int(1)), None);
    /// ```
    #[must_use]
    pub fn error_code(&self, v: Value) -> Option<u32> {
        let Value::Obj(Obj(w)) = v else { return None };
        match self.m.heap.get(w)? {
            Object::Error(e) => Some(e.kind.code()),
            _ => None,
        }
    }

    /// The payload of a runtime error value (LSB §6): the operand of the
    /// `raise` that raised it, `nil` for an error an instruction raised by
    /// itself; `None` for anything but an error value.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{ErrorKind, Inst, ModuleBuilder, ValType};
    ///
    /// // match (7) {} with no arm: raise NoMatch (E0200) with the scrutinee,
    /// // catch it, and return the error value.
    /// let mut m = ModuleBuilder::new();
    /// let mut f = m.function("f", &[], &[ValType::Dyn]);
    /// let (x, e) = (f.reg(ValType::Dyn), f.reg(ValType::Dyn));
    /// f.emit(Inst::DLoadInt { dst: x, val: 7 });
    /// let (start, end) = (f.label(), f.label());
    /// f.bind(start);
    /// f.emit(Inst::Raise { src: x, kind: ErrorKind::NoMatch });
    /// f.bind(end);
    /// f.ret(e);
    /// f.try_region(start, end, end, e);
    /// let id = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// let err = vm.run(id, &[]).unwrap();
    /// assert_eq!(vm.error_code(err), Some(200));
    /// assert_eq!(vm.error_payload(err), Some(Value::Int(7)));
    /// ```
    #[must_use]
    pub fn error_payload(&self, v: Value) -> Option<Value> {
        let Value::Obj(Obj(w)) = v else { return None };
        match self.m.heap.get(w)? {
            Object::Error(e) => Some(conv::dyn_value(&self.m.heap, e.payload)),
            _ => None,
        }
    }

    /// The value a PHP reference (LSB §5.17, kind `reference`) holds, or
    /// `None` when `v` is not a reference.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{Inst, ModuleBuilder, ValType};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let mut f = m.function("f", &[], &[ValType::Dyn]);
    /// let (x, r) = (f.reg(ValType::Dyn), f.reg(ValType::Dyn));
    /// f.emit(Inst::DLoadInt { dst: x, val: 5 });
    /// f.emit(Inst::NewRef { dst: r, src: x });
    /// f.ret(r);
    /// let id = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// let r = vm.run(id, &[]).unwrap();
    /// assert_eq!(vm.kind(r), bytecode_lang::Kind::Reference);
    /// assert_eq!(vm.ref_value(r), Some(Value::Int(5)));
    /// ```
    #[must_use]
    pub fn ref_value(&self, v: Value) -> Option<Value> {
        let Value::Obj(Obj(w)) = v else { return None };
        self.m
            .heap
            .box_value(w)
            .map(|x| conv::dyn_value(&self.m.heap, x))
    }

    /// A slot word as a value: a reference slot (a `dyn` slot holding a
    /// reference) reads as the reference's value.
    fn slot(&self, ty: ValType, w: u64) -> u64 {
        if ty == ValType::Dyn {
            self.m.heap.deref(w)
        } else {
            w
        }
    }
}

/// The error kind of a fault outside a run.
fn fault_kind(f: Fault) -> bytecode_lang::ErrorKind {
    match f {
        Fault::Raise(k) | Fault::RaiseWith(k, _) | Fault::Trap(k) => k,
        Fault::Throw(_) => bytecode_lang::ErrorKind::TypeError,
    }
}
