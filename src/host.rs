//! The host interface: functions the embedding application provides to a
//! module through its imports (and, through imports, its hooks).
//!
//! The seam is deliberately small: a registry of named functions, a context
//! that can read and create strings, and an error type. A host function runs
//! to completion without re-entering the VM and without collection, so the
//! values it receives stay valid for its whole call. Because no host function
//! ever calls back into bytecode, no host frame can lie between a coroutine
//! and its `yield` (LSB §5.13 rule 2): every frame a suspension captures is a
//! bytecode frame.
//!
//! The registry also offers the VM's built-in scheduler as an import
//! ([`Host::register_scheduler`]), which [`Vm::run_async`](crate::Vm::run_async)
//! drives.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use bytecode_lang::{ErrorKind, Kind};

use crate::coll;
use crate::conv;
use crate::fault::Fault;
use crate::hash::Seed;
use crate::heap::{Heap, Object};
use crate::value::{Obj, Value};

/// The signature of a host function.
type HostFnDyn = dyn Fn(&mut HostCtx<'_>, &[Value]) -> Result<Value, HostError> + Send + Sync;

/// A registered host function (cheap to clone; shared by every program that
/// binds it).
#[derive(Clone)]
pub(crate) enum HostFn {
    /// A function the embedder registered.
    User(Arc<HostFnDyn>),
    /// The VM's built-in scheduler entry ([`Host::register_scheduler`]):
    /// makes its coroutine argument a task and returns it as the handle.
    Scheduler,
}

impl fmt::Debug for HostFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostFn::User(_) => f.write_str("HostFn"),
            HostFn::Scheduler => f.write_str("HostFn::Scheduler"),
        }
    }
}

/// A registry of host functions, keyed by import module and name.
///
/// [`Program::load`](crate::Program::load) binds each of a module's imports
/// to the function registered under the import's `(module, name)`; an import
/// with no registration is a load error.
///
/// A host function receives its arguments converted from the import's
/// declared parameter types and returns a [`Value`] that the VM converts to
/// the declared result type (a void import's result is ignored). Returning
/// [`HostError`] raises an error at the calling instruction, catchable like
/// any other (LSB §5.9: "errors from the host propagate as error values").
///
/// # Examples
///
/// ```
/// use bvm_lang::{Host, HostError, Value};
///
/// let mut host = Host::new();
/// host.register("env", "double", |_ctx, args| match args {
///     [Value::Int(i)] => Ok(Value::Int(i.wrapping_mul(2))),
///     _ => Err(HostError::Raise(bytecode_lang::ErrorKind::TypeError)),
/// });
/// assert_eq!(host.len(), 1);
/// ```
#[derive(Clone, Default)]
pub struct Host {
    funcs: BTreeMap<(String, String), HostFn>,
}

impl Host {
    /// An empty registry.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(bvm_lang::Host::new().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Host {
        Host::default()
    }

    /// Registers `f` as the import `module`.`name`, replacing any earlier
    /// registration of that name.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Value};
    ///
    /// let mut host = Host::new();
    /// host.register("env", "answer", |_, _| Ok(Value::Int(42)))
    ///     .register("env", "nothing", |_, _| Ok(Value::Nil));
    /// assert_eq!(host.len(), 2);
    /// ```
    pub fn register<F>(&mut self, module: &str, name: &str, f: F) -> &mut Host
    where
        F: Fn(&mut HostCtx<'_>, &[Value]) -> Result<Value, HostError> + Send + Sync + 'static,
    {
        let f: Arc<HostFnDyn> = Arc::new(f);
        let _previous = self
            .funcs
            .insert((module.to_string(), name.to_string()), HostFn::User(f));
        self
    }

    /// Registers the VM's built-in scheduler as the import `module`.`name`,
    /// whose signature must be `(dyn) -> dyn`. Bind it as the module's
    /// `spawn` hook (`Hook::Spawn`, LSB §5.8) and `spawn` hands each new
    /// coroutine to it; [`Vm::run_async`](crate::Vm::run_async) then drives
    /// the tasks.
    ///
    /// The scheduler is deterministic and single-threaded: it makes its
    /// argument (a coroutine) a task, queues it, and returns it as the task
    /// handle; `await` of a task waits for it to finish. See
    /// [`Vm::run_async`](crate::Vm::run_async) for the rules.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program, Value, Vm};
    /// use bytecode_lang::{Callee, Hook, Inst, ModuleBuilder, Policy, Reg, ValType};
    ///
    /// let d = ValType::Dyn;
    /// let mut host = Host::new();
    /// host.register_scheduler("ls.async", "spawn");
    ///
    /// let mut m = ModuleBuilder::new();
    /// let sig = m.func_type(&[d], &[d]);
    /// let spawn = m.import("ls.async", "spawn", sig);
    /// m.hook(Hook::Spawn, Callee::Import(spawn));
    /// // async fn double(x) { x + x }
    /// let mut task = m.function("double", &[d], &[d]);
    /// let r = task.reg(d);
    /// task.emit(Inst::DAdd { dst: r, lhs: task.param(0), rhs: task.param(0), pol: Policy::new() });
    /// task.ret(r);
    /// let double = m.add_function(task).unwrap();
    /// // async fn main() { await spawn double(21) }
    /// let mut main = m.function("main", &[], &[d]);
    /// let (f, out) = (main.reg(d), main.reg(d));
    /// let w = main.regs(&[d, d]); // spawn's window: result, then the argument
    /// main.emit(Inst::MakeClosure { dst: f, func: double });
    /// main.emit(Inst::DLoadInt { dst: Reg(w.0 + 1), val: 21 });
    /// main.emit(Inst::Spawn { dst: w, callee: f, argc: 1 });
    /// main.emit(Inst::Await { dst: out, src: w });
    /// main.ret(out);
    /// let main = m.add_function(main).unwrap();
    ///
    /// let p = Program::load(m.finish().unwrap(), &host).unwrap();
    /// assert_eq!(Vm::new(&p).run_async(main, &[]), Ok(Value::Int(42)));
    /// ```
    pub fn register_scheduler(&mut self, module: &str, name: &str) -> &mut Host {
        let _previous = self
            .funcs
            .insert((module.to_string(), name.to_string()), HostFn::Scheduler);
        self
    }

    /// The number of registered functions.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(bvm_lang::Host::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.funcs.len()
    }

    /// Whether nothing is registered.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(bvm_lang::Host::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.funcs.is_empty()
    }

    pub(crate) fn lookup(&self, module: &str, name: &str) -> Option<&HostFn> {
        // BTreeMap<(String, String)> cannot be queried by (&str, &str)
        // without allocating; imports are bound once per load, so that is
        // fine.
        self.funcs.get(&(module.to_string(), name.to_string()))
    }
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<_> = self.funcs.keys().collect();
        f.debug_struct("Host").field("funcs", &names).finish()
    }
}

/// What a host function can do with the VM while it runs: read and create
/// strings and inspect values.
///
/// # Examples
///
/// ```
/// use bvm_lang::{Host, Value};
///
/// let mut host = Host::new();
/// host.register("env", "greet", |ctx, args| {
///     let name = args.first().and_then(|v| ctx.str_bytes(*v)).unwrap_or(b"world");
///     let mut text = b"hello, ".to_vec();
///     text.extend_from_slice(name);
///     ctx.new_str(&text)
/// });
/// ```
pub struct HostCtx<'a> {
    pub(crate) heap: &'a mut Heap,
    pub(crate) seed: Seed,
}

impl HostCtx<'_> {
    /// The bytes of a string value.
    ///
    /// # Examples
    ///
    /// See [`HostCtx`].
    #[must_use]
    pub fn str_bytes(&self, v: Value) -> Option<&[u8]> {
        match v {
            Value::Obj(Obj(bits)) => self.heap.str(bits),
            _ => None,
        }
    }

    /// Allocates a string.
    ///
    /// # Errors
    ///
    /// `HostError::Raise(ErrorKind::OutOfMemory)` when the VM's memory budget
    /// is spent; the VM turns it into the `OutOfMemory` trap.
    ///
    /// # Examples
    ///
    /// See [`HostCtx`].
    pub fn new_str(&mut self, bytes: &[u8]) -> Result<Value, HostError> {
        self.heap
            .alloc_str(bytes)
            .map(|bits| Value::Obj(Obj(bits)))
            .map_err(|_| HostError::Raise(ErrorKind::OutOfMemory))
    }

    /// The dynamic kind of a value (LSB §2.2).
    ///
    /// # Examples
    ///
    /// ```
    /// use bytecode_lang::Kind;
    /// use bvm_lang::{Host, Value};
    ///
    /// let mut host = Host::new();
    /// host.register("env", "is_int", |ctx, args| {
    ///     Ok(Value::Bool(args.first().map(|v| ctx.kind(*v)) == Some(Kind::Int)))
    /// });
    /// ```
    #[must_use]
    pub fn kind(&self, v: Value) -> Kind {
        value_kind(self.heap, v)
    }

    /// The error code of a runtime error value (`1` for E0001), if `v` is
    /// one.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Value};
    ///
    /// let mut host = Host::new();
    /// host.register("env", "code", |ctx, args| {
    ///     Ok(Value::UInt(args.first().and_then(|v| ctx.error_code(*v)).unwrap_or(0).into()))
    /// });
    /// ```
    #[must_use]
    pub fn error_code(&self, v: Value) -> Option<u32> {
        match v {
            Value::Obj(Obj(bits)) => match self.heap.get(bits)? {
                Object::Error(e) => Some(e.kind.code()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The value a PHP reference holds (LSB §5.17), if `v` is one. A host
    /// function receives a reference for every `by_ref` parameter of its
    /// import's parameter list.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Value};
    ///
    /// let mut host = Host::new();
    /// // PHP: function inc(&$x) { $x = $x + 1; }
    /// host.register("env", "inc", |ctx, args| {
    ///     let r = args[0];
    ///     let n = ctx.ref_get(r).and_then(|v| v.as_int()).unwrap_or(0);
    ///     ctx.ref_set(r, Value::Int(n + 1))?;
    ///     Ok(Value::Nil)
    /// });
    /// ```
    #[must_use]
    pub fn ref_get(&self, v: Value) -> Option<Value> {
        match v {
            Value::Obj(Obj(bits)) => self
                .heap
                .box_value(bits)
                .map(|w| conv::dyn_value(self.heap, w)),
            _ => None,
        }
    }

    /// Stores `value` into the PHP reference `r` (a reference given as the
    /// value stores its value: a reference never holds a reference).
    ///
    /// # Errors
    ///
    /// [`HostError::Raise`] with `TypeError` when `r` is not a reference,
    /// `ArithOverflow` for a `UInt` above `i64::MAX`, `OutOfMemory` when the
    /// value needs memory the budget does not have.
    ///
    /// # Examples
    ///
    /// See [`ref_get`](HostCtx::ref_get).
    pub fn ref_set(&mut self, r: Value, value: Value) -> Result<(), HostError> {
        let Value::Obj(Obj(bits)) = r else {
            return Err(HostError::Raise(ErrorKind::TypeError));
        };
        if self.heap.box_value(bits).is_none() {
            return Err(HostError::Raise(ErrorKind::TypeError));
        }
        let w = conv::dyn_of(self.heap, value).map_err(fault_error)?;
        let w = self.heap.deref(w);
        let _ = self.heap.set_box(bits, w);
        Ok(())
    }

    /// The elements of an array value, in order (a reference slot as its
    /// value).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Value};
    ///
    /// let mut host = Host::new();
    /// // A rest parameter arrives as an array: count its elements.
    /// host.register("env", "count", |ctx, args| {
    ///     let n = args.first().and_then(|v| ctx.elements(*v)).map_or(0, |e| e.len());
    ///     Ok(Value::Int(n as i64))
    /// });
    /// ```
    #[must_use]
    pub fn elements(&self, v: Value) -> Option<Vec<Value>> {
        let Value::Obj(Obj(bits)) = v else {
            return None;
        };
        match self.heap.get(bits)? {
            Object::Array(a) => Some(
                a.items
                    .iter()
                    .map(|&w| {
                        let w = if a.elem == bytecode_lang::ValType::Dyn {
                            self.heap.deref(w)
                        } else {
                            w
                        };
                        conv::value_of(self.heap, a.elem, w)
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The entries of a map value, in insertion order (a reference slot as
    /// its value).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Value};
    ///
    /// let mut host = Host::new();
    /// // A named rest parameter (`**kwargs`) arrives as a map.
    /// host.register("env", "nkw", |ctx, args| {
    ///     let n = args.first().and_then(|v| ctx.entries(*v)).map_or(0, |e| e.len());
    ///     Ok(Value::Int(n as i64))
    /// });
    /// ```
    #[must_use]
    pub fn entries(&self, v: Value) -> Option<Vec<(Value, Value)>> {
        let Value::Obj(Obj(bits)) = v else {
            return None;
        };
        match self.heap.get(bits)? {
            Object::Map(m) => Some(
                m.store
                    .entries()
                    .iter()
                    .filter(|e| e.live)
                    .map(|e| {
                        let w = if m.value == bytecode_lang::ValType::Dyn {
                            self.heap.deref(e.value)
                        } else {
                            e.value
                        };
                        (
                            conv::value_of(self.heap, m.key, e.key),
                            conv::value_of(self.heap, m.value, w),
                        )
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// A new `dyn` array of `items` (references are stored as their values).
    ///
    /// # Errors
    ///
    /// As [`ref_set`](HostCtx::ref_set), without the `TypeError`.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Value};
    ///
    /// let mut host = Host::new();
    /// host.register("env", "pair", |ctx, args| ctx.new_array(&[args[0], args[1]]));
    /// ```
    pub fn new_array(&mut self, items: &[Value]) -> Result<Value, HostError> {
        let mut words = Vec::with_capacity(items.len());
        for &v in items {
            let w = conv::dyn_of(self.heap, v).map_err(fault_error)?;
            words.push(self.heap.deref(w));
        }
        coll::new_dyn_array(self.heap, words)
            .map(|w| Value::Obj(Obj(w)))
            .map_err(fault_error)
    }

    /// A new `dyn` map of `entries`, in order (a repeated key keeps its first
    /// position and its last value, as `map_set` does).
    ///
    /// # Errors
    ///
    /// As [`new_array`](HostCtx::new_array).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Value};
    ///
    /// let mut host = Host::new();
    /// host.register("env", "one", |ctx, _| ctx.new_map(&[(Value::Int(1), Value::Bool(true))]));
    /// ```
    pub fn new_map(&mut self, entries: &[(Value, Value)]) -> Result<Value, HostError> {
        let map = coll::new_map(
            self.heap,
            bytecode_lang::ValType::Dyn,
            bytecode_lang::ValType::Dyn,
        )
        .map_err(fault_error)?;
        for &(k, v) in entries {
            let k = conv::dyn_of(self.heap, k).map_err(fault_error)?;
            let v = conv::dyn_of(self.heap, v).map_err(fault_error)?;
            coll::map_set(self.heap, self.seed, map, k, v).map_err(fault_error)?;
        }
        Ok(Value::Obj(Obj(map)))
    }
}

/// A fault inside a host-context helper as the host's error.
fn fault_error(f: Fault) -> HostError {
    match f {
        Fault::Raise(k) | Fault::RaiseWith(k, _) | Fault::Trap(k) => HostError::Raise(k),
        Fault::Throw(_) => HostError::Raise(ErrorKind::TypeError),
    }
}

impl fmt::Debug for HostCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostCtx")
    }
}

/// The kind of a host-side value.
pub(crate) fn value_kind(heap: &Heap, v: Value) -> Kind {
    match v {
        Value::Nil => Kind::Nil,
        Value::Bool(_) => Kind::Bool,
        Value::Int(_) | Value::UInt(_) => Kind::Int,
        Value::F32(_) | Value::Float(_) => Kind::Float,
        Value::Char(_) => Kind::Char,
        Value::Obj(Obj(bits)) => heap.kind(bits),
    }
}

/// An error a host function reports.
///
/// # Examples
///
/// ```
/// use bvm_lang::{HostError, Value};
/// use bytecode_lang::ErrorKind;
///
/// let e = HostError::Raise(ErrorKind::KeyNotFound);
/// assert_eq!(e.to_string(), "host raised E0103 KeyNotFound");
/// let t = HostError::Throw(Value::Int(7));
/// assert_eq!(t.to_string(), "host threw 7");
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum HostError {
    /// Raise a runtime error of this kind at the calling instruction. A
    /// non-catchable kind (`OutOfMemory`, `OutOfFuel`, `Unreachable`) traps.
    Raise(ErrorKind),
    /// Throw this value at the calling instruction, as `throw` would.
    Throw(Value),
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::Raise(kind) => write!(f, "host raised {kind}"),
            HostError::Throw(v) => write!(f, "host threw {v}"),
        }
    }
}

impl core::error::Error for HostError {}
