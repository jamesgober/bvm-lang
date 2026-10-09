//! The host interface: functions the embedding application provides to a
//! module through its imports (and, through imports, its hooks).
//!
//! The seam is deliberately small: a registry of named functions, a context
//! that can read and create strings, and an error type. A host function runs
//! to completion without re-entering the VM and without collection, so the
//! values it receives stay valid for its whole call.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use bytecode_lang::{ErrorKind, Kind};

use crate::heap::{Heap, Object};
use crate::value::{Obj, Value};

/// The signature of a host function.
type HostFnDyn = dyn Fn(&mut HostCtx<'_>, &[Value]) -> Result<Value, HostError> + Send + Sync;

/// A registered host function (cheap to clone; shared by every program that
/// binds it).
#[derive(Clone)]
pub(crate) struct HostFn(Arc<HostFnDyn>);

impl HostFn {
    pub(crate) fn call(&self, ctx: &mut HostCtx<'_>, args: &[Value]) -> Result<Value, HostError> {
        (self.0)(ctx, args)
    }
}

impl fmt::Debug for HostFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostFn")
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
            .insert((module.to_string(), name.to_string()), HostFn(f));
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
