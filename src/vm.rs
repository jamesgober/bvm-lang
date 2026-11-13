//! The public VM: an instance of a [`Program`] with its own heap, globals,
//! and limits.

use alloc::vec::Vec;

use bytecode_lang::{CoroState, FuncId, GlobalId, Kind, ValType};

use crate::conv;
use crate::dynv;
use crate::error::VmError;
use crate::exec;
use crate::fault::Fault;
use crate::hash::Seed;
use crate::heap::{NO_DTOR, Object};
use crate::host;
use crate::machine::{Machine, Stop, Wake};
use crate::program::Program;
use crate::value::{Obj, Value};

/// How a VM manages memory: its *memory profile* (DECISIONS D22, the
/// language's `[runtime] memory` key).
///
/// LSB semantics that depend on reference counts follow the profile: when a
/// dropped suspended coroutine is closed (LSB §5.13 rule 13), when an
/// object's destructor runs ([`Vm::set_destructor`]), and whether copying an
/// array unbinds a PHP reference nothing else holds (LSB §5.17 "the one
/// divergence"). Everything else (results, errors, fuel, globals) is the same
/// under both: the differential tests run the same programs under both.
///
/// | | `Tracing` (default) | `Rc` |
/// |---|---|---|
/// | freeing | mark and sweep, when enough was allocated | at the last reference, before the next instruction |
/// | garbage cycles | traced | trial-deletion cycle collector (PHP's), fuel-charged |
/// | drop-close, destructors | at the next fuel charge point after the collection that found the object | at the last reference, before the next instruction (PHP's timing) |
/// | copying an array or map holding a reference only it holds | the copy shares the reference (LSB's rule) | the copy holds the value (PHP's rule) |
///
/// # Examples
///
/// ```
/// use bvm_lang::{Host, Memory, Program, Vm};
/// use bytecode_lang::ModuleBuilder;
///
/// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
/// assert_eq!(Vm::new(&p).memory(), Memory::Tracing);
/// assert_eq!(Vm::with_memory(&p, Memory::Rc).memory(), Memory::Rc);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Memory {
    /// A tracing collector (`[runtime] memory = "gc"`): Mercury and other
    /// languages without deterministic finalization.
    #[default]
    Tracing,
    /// Reference counting plus a cycle collector (`[runtime] memory =
    /// "rc"`): PHP's model, for Mox and other PHP-compatible languages.
    Rc,
}

/// What a VM's memory manager has done ([`Vm::memory_stats`]).
///
/// # Examples
///
/// ```
/// use bvm_lang::{Host, Memory, Program, Vm};
/// use bytecode_lang::ModuleBuilder;
///
/// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
/// let mut vm = Vm::with_memory(&p, Memory::Rc);
/// vm.new_str(b"kept until the next run").unwrap();
/// let stats = vm.memory_stats();
/// assert_eq!((stats.live, stats.freed), (1, 0));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoryStats {
    /// Collections run: tracing collections, or cycle collections in the
    /// RC profile.
    pub collections: u64,
    /// Objects freed, by every means.
    pub freed: u64,
    /// Of those, freed by the RC cycle collector (garbage cycles).
    pub cycle_freed: u64,
    /// Finalizations started: destructors run and dropped coroutines closed.
    pub finalized: u64,
    /// RC: references unbound by a copy (PHP's rule that a copied array
    /// drops a reference only it holds).
    pub unbound: u64,
    /// Live objects (exact in the RC profile; in the tracing profile, as of
    /// the last collection plus what was allocated since).
    pub live: usize,
    /// Bytes charged against the memory budget.
    pub bytes: usize,
}

/// The budgets a run executes under.
///
/// Every budget is enforced deterministically, so with finite limits an
/// untrusted module cannot run forever, exhaust memory, or overflow the
/// native stack:
///
/// - **fuel**: one unit at every `safepoint`, call-family instruction
///   (calls, tail calls, host calls, hook invocations), taken backward
///   branch, handler entry, coroutine instruction, and `iter_next` over a
///   coroutine (the rule of LSB §5.14), plus one unit per 256 bytes of data
///   an instruction builds, copies, hashes, or compares, rounded down per
///   instruction (building or slicing a string, a new array, separating
///   shared array or map storage, hashing or comparing a string key; work
///   inside host functions is not charged). Exhausting it is the
///   `OutOfFuel` trap (E0107). Execution
///   between two charges is bounded by the length of one function, and the
///   data work is paid for when the instruction that did it finishes, so
///   fuel bounds total work.
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
/// Three presets:
///
/// | preset | fuel | memory | depth | register slots |
/// |---|---|---|---|---|
/// | [`untrusted`](Limits::untrusted) | 10,000,000 | 64 MiB | 1,000 | 1 Mi (8 MiB) |
/// | [`new`](Limits::new) (the default) | 100,000,000 | 256 MiB | 10,000 | 4 Mi (32 MiB) |
/// | [`trusted`](Limits::trusted) | unlimited | 1 GiB | 10,000 | 4 Mi (32 MiB) |
///
/// # Examples
///
/// ```
/// use bvm_lang::Limits;
///
/// let tight = Limits::untrusted().with_fuel(10_000).with_memory(1 << 20).with_depth(64);
/// assert_eq!(tight.fuel(), 10_000);
/// assert_eq!(Limits::default().fuel(), 100_000_000);
/// assert_eq!(Limits::trusted().fuel(), u64::MAX);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    fuel: u64,
    memory: usize,
    depth: usize,
    stack: usize,
    data_fuel: bool,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::new()
    }
}

impl Limits {
    /// The defaults: 100,000,000 units of fuel, 256 MiB of heap, 10,000
    /// frames, and 4 Mi register slots (32 MiB). Every budget is finite, so
    /// a module that loops forever ends with `OutOfFuel`. For code you do
    /// not control, start from [`untrusted`](Limits::untrusted) and size the
    /// fuel to the time you can spend.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Limits;
    ///
    /// assert_eq!(Limits::new().depth(), 10_000);
    /// assert_eq!(Limits::new().memory(), 256 << 20);
    /// ```
    #[must_use]
    pub const fn new() -> Limits {
        Limits {
            fuel: 100_000_000,
            memory: 256 << 20,
            depth: 10_000,
            stack: 4 << 20,
            data_fuel: true,
        }
    }

    /// Tight budgets for code you do not control: 10,000,000 units of fuel,
    /// 64 MiB of heap, 1,000 frames, and 1 Mi register slots (8 MiB).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Limits;
    ///
    /// let l = Limits::untrusted();
    /// assert_eq!((l.fuel(), l.memory(), l.depth()), (10_000_000, 64 << 20, 1_000));
    /// ```
    #[must_use]
    pub const fn untrusted() -> Limits {
        Limits {
            fuel: 10_000_000,
            memory: 64 << 20,
            depth: 1_000,
            stack: 1 << 20,
            data_fuel: true,
        }
    }

    /// No fuel limit and 1 GiB of heap, with the default depth and register
    /// slots: for code you trust to finish. A module that loops forever runs
    /// forever under these limits.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Limits;
    ///
    /// assert_eq!(Limits::trusted().fuel(), u64::MAX);
    /// assert_eq!(Limits::trusted().memory(), 1 << 30);
    /// ```
    #[must_use]
    pub const fn trusted() -> Limits {
        Limits {
            fuel: u64::MAX,
            memory: 1 << 30,
            depth: 10_000,
            stack: 4 << 20,
            data_fuel: true,
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

    /// Sets whether data work costs fuel (on in every preset): one unit
    /// per 256 bytes an instruction builds, copies, hashes, or compares.
    /// Off, fuel is charged at LSB §5.14's points only, the count every
    /// tier agrees on, and the work one unit pays for is bounded only by
    /// the memory budget, so turn it off only for code you trust.
    ///
    /// # Examples
    ///
    /// ```
    /// let lsb = bvm_lang::Limits::new().with_data_fuel(false);
    /// assert!(!lsb.data_fuel());
    /// assert!(bvm_lang::Limits::untrusted().data_fuel());
    /// ```
    #[must_use]
    pub const fn with_data_fuel(self, on: bool) -> Limits {
        Limits {
            data_fuel: on,
            ..self
        }
    }

    /// Whether data work costs fuel.
    ///
    /// # Examples
    ///
    /// See [`with_data_fuel`](Limits::with_data_fuel).
    #[must_use]
    pub const fn data_fuel(&self) -> bool {
        self.data_fuel
    }

    /// The fuel budget.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Limits::new().fuel(), 100_000_000);
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
    /// assert_eq!(bvm_lang::Limits::new().memory(), 256 << 20);
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
    /// An instance of `program` with the default [`Limits`]: finite fuel
    /// and memory ([`Limits::new`]). Use [`with_limits`](Vm::with_limits)
    /// and [`Limits::untrusted`] for code you do not control.
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
            m: Machine::new(program, limits.memory, false),
            limits,
            ready: false,
            fuel_used: 0,
        }
    }

    /// An instance of `program` under a memory profile, with the default
    /// [`Limits`]. The profile is fixed for the VM's life.
    ///
    /// # Examples
    ///
    /// Under [`Memory::Rc`] an object dies at its last reference: a string
    /// the run made and dropped is gone when the run returns, without any
    /// collection.
    ///
    /// ```
    /// use bvm_lang::{Host, Memory, Program, Vm};
    /// use bytecode_lang::{Const, Inst, ModuleBuilder, ValType};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let k = m.constant(Const::Bytes(b"temporary".to_vec()));
    /// let mut f = m.function("f", &[], &[]);
    /// let (s, t) = (f.reg(ValType::Dyn), f.reg(ValType::Dyn));
    /// f.emit(Inst::DLoadConst { dst: s, k });
    /// f.emit(Inst::DConcat { dst: t, lhs: s, rhs: s });
    /// f.emit(Inst::LoadNil { dst: t });
    /// f.ret_void();
    /// let f = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    ///
    /// let mut vm = Vm::with_memory(&p, Memory::Rc);
    /// vm.run(f, &[]).unwrap();
    /// // The constant's string stays (its cache holds it); the concatenation
    /// // died when `t` was overwritten.
    /// assert_eq!(vm.heap_objects(), 1);
    /// assert_eq!(vm.collections(), 0);
    /// ```
    #[must_use]
    pub fn with_memory(program: &'p Program, memory: Memory) -> Vm<'p> {
        let limits = Limits::new();
        Vm {
            prog: program,
            m: Machine::new(program, limits.memory, memory == Memory::Rc),
            limits,
            ready: false,
            fuel_used: 0,
        }
    }

    /// Sets the 128-bit key that map keys are hashed with, for a host that
    /// has its own entropy. With the `std` feature every VM already gets a
    /// random key; without it the key is a fixed constant, and a module that
    /// knows it can choose string keys that collide, which makes building a
    /// map quadratic. A `no_std` host that runs untrusted modules should
    /// call this with a random value before the first run.
    ///
    /// Returns `false`, changing nothing, once the heap holds a map (its
    /// entries are placed under the old key).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Vm};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// let mut vm = Vm::new(&p);
    /// assert!(vm.set_hash_seed(0x0123_4567_89ab_cdef_0f1e_2d3c_4b5a_6978));
    /// ```
    pub fn set_hash_seed(&mut self, seed: u128) -> bool {
        if self.m.heap.holds_map() {
            return false;
        }
        self.m.seed = Seed::from_u128(seed);
        true
    }

    /// The memory profile.
    ///
    /// # Examples
    ///
    /// See [`Memory`].
    #[must_use]
    pub fn memory(&self) -> Memory {
        if self.m.heap.rc {
            Memory::Rc
        } else {
            Memory::Tracing
        }
    }

    /// Names the destructor method of struct types (PHP's `__destruct`,
    /// Python's `__del__`): every struct type whose methods, its own or
    /// inherited (as `get_prop` finds them), include one of this name has it
    /// called with the instance as its one argument when the instance dies.
    /// Returns how many struct types got a destructor; a name no method has
    /// (the empty name, say) clears every destructor.
    ///
    /// When "dies" means follows the memory profile ([`Memory`]): under
    /// [`Memory::Rc`] at the last reference, before the next instruction
    /// (PHP's timing; an object in a garbage cycle when the cycle collector
    /// finds it); under [`Memory::Tracing`] the collection that finds it
    /// unreachable queues it, and it runs at the next fuel charge point (as
    /// a dropped coroutine is closed), or on
    /// [`run_finalizers`](Vm::run_finalizers). Either way a destructor runs
    /// at most once per object; its result and any error escaping it are
    /// discarded; it is charged fuel like any code; one the depth or
    /// register limits cannot hold is skipped. An object a destructor stores
    /// somewhere lives on (and is not destructed again). Objects still
    /// reachable when the VM is dropped are not destructed (PHP's shutdown
    /// pass is the host's call: drop its references, collect, and run the
    /// finalizers).
    ///
    /// The format has no destructor flag (LSB 2); this names the method by
    /// convention, as a language's runtime profile does.
    ///
    /// # Errors
    ///
    /// [`VmError::BadDestructor`] when a method found cannot take the
    /// instance as its one argument (nothing is changed).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Memory, Program, Value, Vm};
    /// use bytecode_lang::{Inst, Method, ModuleBuilder, StructDef, TypeDef, ValType};
    ///
    /// let mut m = ModuleBuilder::new();
    /// let (name, dtor) = (m.string("Res"), m.string("__destruct"));
    /// let res = m.reserve_type();
    /// let log = m.global("log", ValType::I64, true, None);
    /// // __destruct($this) { log = log + 1 }
    /// let mut d = m.function("Res::__destruct", &[ValType::Dyn], &[]);
    /// let (v, one) = (d.reg(ValType::I64), d.reg(ValType::I64));
    /// d.emit(Inst::GetGlobal { dst: v, global: log });
    /// d.emit(Inst::LoadInt { dst: one, val: 1, ty: bytecode_lang::IntTy::I64 });
    /// d.emit(Inst::IAdd { dst: v, lhs: v, rhs: one, op: bytecode_lang::IntOp::new(bytecode_lang::IntTy::I64) });
    /// d.emit(Inst::SetGlobal { global: log, src: v });
    /// d.ret_void();
    /// let d = m.add_function(d).unwrap();
    /// m.define_type(res, TypeDef::Struct(StructDef {
    ///     name,
    ///     methods: vec![Method { name: dtor, func: d }],
    ///     ..Default::default()
    /// }));
    /// // main() { o = new Res; o = nil; return log }  (the destructor runs at `o = nil`)
    /// let mut f = m.function("main", &[], &[ValType::I64]);
    /// let (o, r) = (f.reg(ValType::Dyn), f.reg(ValType::I64));
    /// let ty = f.type_ref(res);
    /// f.emit(Inst::NewStruct { dst: o, ty });
    /// f.emit(Inst::LoadNil { dst: o });
    /// f.emit(Inst::GetGlobal { dst: r, global: log });
    /// f.ret(r);
    /// let main = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    ///
    /// let mut vm = Vm::with_memory(&p, Memory::Rc);
    /// assert_eq!(vm.set_destructor("__destruct"), Ok(1));
    /// assert_eq!(vm.run(main, &[]), Ok(Value::Int(1)));
    /// ```
    pub fn set_destructor(&mut self, name: &str) -> Result<usize, VmError> {
        let prog = self.prog;
        let Some(id) = prog.module.strings().position(|s| s == name) else {
            self.m.heap.dtors = Vec::new();
            return Ok(0);
        };
        let id = u32::try_from(id).unwrap_or(u32::MAX);
        let mut dtors = alloc::vec![NO_DTOR; prog.types.len()];
        let mut found = 0;
        for (t, slot) in dtors.iter_mut().enumerate() {
            let t = u32::try_from(t).unwrap_or(u32::MAX);
            if prog.struct_info(t).is_none() {
                continue;
            }
            let Some(f) = exec::find_method(prog, t, id) else {
                continue;
            };
            let fits = prog.func(f).is_some_and(|i| {
                i.nparams == 1
                    && i.captures.is_empty()
                    && match i.regs.first() {
                        Some(ValType::Dyn) => true,
                        Some(ValType::Ref(a)) => prog.descends(t, a.0),
                        _ => false,
                    }
            });
            if !fits {
                return Err(VmError::BadDestructor(FuncId(f)));
            }
            *slot = f;
            found += 1;
        }
        self.m.heap.dtors = if found > 0 { dtors } else { Vec::new() };
        Ok(found)
    }

    /// What the memory manager has done so far.
    ///
    /// # Examples
    ///
    /// See [`MemoryStats`].
    #[must_use]
    pub fn memory_stats(&self) -> MemoryStats {
        let s = self.m.heap.stats;
        MemoryStats {
            collections: s.collections,
            freed: s.freed,
            cycle_freed: s.cycle_freed,
            finalized: s.finalized,
            unbound: s.unbound,
            live: self.m.heap.len(),
            bytes: self.m.heap.used(),
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
        self.m.data_fuel = limits.data_fuel;
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
        let out = exec::execute(&mut self.m, prog, func.0, &words, fuel, true)?;
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
            let _ = exec::execute(&mut self.m, prog, start.0, &[], fuel, false)?;
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
        self.m.data_fuel = limits.data_fuel;
        let mut fuel = limits.fuel;
        let result = self.async_inner(func, args, &mut fuel);
        // Unfinished tasks are dropped (closed when they die: at once in
        // the RC profile, at a later collection when tracing).
        self.m.sched_clear();
        exec::settle_quiet(&mut self.m, self.prog);
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
        self.m.sched_push(main, Wake::Send(dynv::NIL));
        // The main task holds its arguments now (RC).
        self.m.release_pins();
        loop {
            let Some((task, wake)) = self.m.sched.ready.pop_front() else {
                // Nothing is ready, so every task left (the main one
                // included) is waiting for another.
                return Err(VmError::Deadlock {
                    waiting: self.m.sched.len(),
                });
            };
            self.m.waking = Some((task, wake));
            let stop = self.wake(func, task, wake, fuel);
            // The popped entry's hold goes once the task has run (RC).
            self.m.waking = None;
            self.m.sched_release(task, wake);
            let stop = match stop? {
                Some(s) => s,
                None => continue,
            };
            match stop {
                Stop::Yielded(_) | Stop::CloseIgnored => {
                    self.m.sched_push(task, Wake::Send(dynv::NIL));
                }
                Stop::Awaiting(target) => match self.m.heap.coro_mut(target) {
                    Some(t) => match t.state {
                        CoroState::Returned => {
                            let r = t.result;
                            self.m.sched_push(task, Wake::Send(r));
                        }
                        CoroState::Failed => {
                            let r = t.result;
                            self.m.sched_push(task, Wake::Throw(r));
                        }
                        _ => {
                            let start = !t.task && t.state.is_resumable();
                            if start {
                                t.task = true;
                                self.m.sched_push(target, Wake::Send(dynv::NIL));
                            }
                            self.m.sched_wait(target, task);
                        }
                    },
                    None => self.m.sched_push(task, Wake::Send(target)),
                },
                Stop::Returned(v) => {
                    if task == main {
                        self.m.pin(v);
                        return Ok(conv::dyn_value(&self.m.heap, v));
                    }
                    self.m.sched_wake(task, Wake::Send(v));
                }
                Stop::Failed(e, at_func, at_pc) => {
                    if task == main {
                        return Err(self.m.uncaught(e, at_func, at_pc));
                    }
                    self.m.sched_wake(task, Wake::Throw(e));
                }
            }
        }
    }

    /// Runs one ready task; `None` when the scheduler no longer owns it.
    fn wake(
        &mut self,
        func: FuncId,
        task: u64,
        wake: Wake,
        fuel: &mut u64,
    ) -> Result<Option<Stop>, VmError> {
        let prog = self.prog;
        Ok(Some(
            match self.m.heap.coro(task).map(|c| (c.state, c.result)) {
                Some((CoroState::Returned, v)) => Stop::Returned(v),
                Some((CoroState::Failed, e)) => Stop::Failed(e, func.0, 0),
                // A task some bytecode is running cannot be resumed from
                // here; it is not one the scheduler still owns.
                Some((CoroState::Running, _)) | None => return Ok(None),
                Some((CoroState::Created, _)) if matches!(wake, Wake::Throw(_)) => {
                    let Wake::Throw(e) = wake else {
                        return Ok(None);
                    };
                    self.m.end_coro(prog, task, CoroState::Failed, e);
                    Stop::Failed(e, func.0, 0)
                }
                Some(_) => exec::drive(&mut self.m, prog, task, wake, fuel)?,
            },
        ))
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
    ///
    /// In the RC profile the queue holds what died outside a run (or when a
    /// run ended abnormally) and the finalizable garbage a cycle collection
    /// found; it is also drained at the next run's first instruction.
    pub fn run_finalizers(&mut self) -> Result<usize, VmError> {
        let limits = self.limits;
        self.m.heap.limit = limits.memory;
        self.m.max_depth = limits.depth;
        self.m.max_stack = limits.stack;
        self.m.data_fuel = limits.data_fuel;
        let mut fuel = limits.fuel;
        let result = exec::finalize_all(&mut self.m, self.prog, &mut fuel);
        self.fuel_used = limits.fuel - fuel;
        result
    }

    /// Dropped suspended coroutines waiting to be closed and objects
    /// waiting for their destructor (see
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
    /// queued to be closed (LSB §5.13 rule 13), and objects with a
    /// destructor, which are queued for it: at the next run's first fuel
    /// charge (RC: first instruction), or now with
    /// [`run_finalizers`](Vm::run_finalizers).
    ///
    /// In the RC profile everything else is already freed at its last
    /// reference: this lets go of the values earlier runs handed out and
    /// runs the cycle collector over every possible root.
    ///
    /// # Examples
    ///
    /// See [`heap_objects`](Vm::heap_objects).
    pub fn collect_garbage(&mut self) {
        if self.m.heap.rc {
            self.m.release_pins();
            exec::settle_quiet(&mut self.m, self.prog);
            self.m.collect_cycles(self.prog, true);
            exec::settle_quiet(&mut self.m, self.prog);
            #[cfg(debug_assertions)]
            if self.m.heap.slots.len() <= 50_000 {
                let checked = self.m.check_reachable(self.prog);
                debug_assert!(checked.is_ok(), "{checked:?}");
            }
            return;
        }
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
        let w = self.m.heap.alloc_str(bytes).map_err(|f| VmError::Trap {
            kind: fault_kind(f),
            func: FuncId(u32::MAX),
            pc: 0,
        })?;
        // Held for the host until the next run (RC), like a run's result.
        self.m.pin(w);
        Ok(Value::Obj(Obj(w)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytecode_lang::{ErrorKind, Inst, IntTy, ModuleBuilder, TypeDef};

    /// A program holding one array `n` times in another, then returning.
    fn holders(n: usize) -> (Program, FuncId) {
        let mut m = ModuleBuilder::new();
        let at = m.add_type(TypeDef::Array(ValType::Dyn));
        let mut f = m.function("main", &[], &[]);
        let ty = f.type_ref(at);
        let (len, a, x) = (
            f.reg(ValType::I64),
            f.reg(ValType::Dyn),
            f.reg(ValType::Dyn),
        );
        let _ = f.emit(Inst::LoadInt {
            dst: len,
            val: 0,
            ty: IntTy::I64,
        });
        let _ = f.emit(Inst::NewArray { dst: a, len, ty });
        let _ = f.emit(Inst::NewArray { dst: x, len, ty });
        for _ in 0..n {
            let _ = f.emit(Inst::ArrayPush { arr: a, src: x });
        }
        let _ = f.ret_void();
        let main = m.add_function(f).unwrap_or(FuncId(0));
        let p = Program::load(m.finish().unwrap_or_default(), &crate::Host::new());
        (p.unwrap_or_else(|e| panic!("{e}")), main)
    }

    /// A count that would pass its maximum sticks there: the object is never
    /// freed again (its count is no longer exact), and the run ends with the
    /// `OutOfMemory` trap at the next instruction boundary, never with
    /// undefined behaviour or a panic. The VM stays usable.
    #[test]
    fn test_a_saturated_count_traps_and_its_object_is_never_freed() {
        let (p, main) = holders(10);
        let mut vm = Vm::with_memory(&p, Memory::Rc);
        vm.m.heap.rc_max = 4;
        // The register holds it once, the first three pushes take it to 4,
        // the fourth push saturates: the trap comes before the fifth.
        assert_eq!(
            vm.run(main, &[]),
            Err(VmError::Trap {
                kind: ErrorKind::OutOfMemory,
                func: main,
                pc: 7,
            })
        );
        let live = vm.heap_objects();
        vm.collect_garbage();
        assert_eq!(
            vm.heap_objects(),
            1,
            "only the stuck array is left ({live} before)"
        );
        // Under the ceiling nothing traps, and everything goes.
        let (p, main) = holders(2);
        let mut vm = Vm::with_memory(&p, Memory::Rc);
        vm.m.heap.rc_max = 4;
        assert_eq!(vm.run(main, &[]), Ok(Value::Nil));
        assert_eq!(vm.heap_objects(), 0);
        assert_eq!(vm.run(main, &[]), Ok(Value::Nil), "the VM stays usable");
    }

    /// The tracing profile keeps no counts at all.
    #[test]
    fn test_tracing_keeps_no_counts() {
        let (p, main) = holders(10);
        let mut vm = Vm::new(&p);
        vm.m.heap.rc_max = 1;
        assert_eq!(vm.run(main, &[]), Ok(Value::Nil));
        assert!(vm.m.heap.slots.iter().all(|s| s.rc == 0 && s.flags == 0));
    }
}
