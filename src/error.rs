//! The errors a run can end with.

use core::fmt;

use bytecode_lang::{ErrorKind, FuncId, GlobalId};

use crate::value::Value;

/// Why a run ended without a result.
///
/// Errors that unwind (LSB §4.3) reach the host only when no handler caught
/// them: [`Raised`](VmError::Raised) for a runtime error (with the OPS/LSB
/// [`ErrorKind`], its payload, and the function and pc that raised it) and
/// [`Thrown`](VmError::Thrown) for a `throw` of any other value. Traps
/// ([`Trap`](VmError::Trap)) abort at once and carry the instruction that
/// trapped. Every runtime variant names the function and pc (ISSUES M62).
/// [`Deadlock`](VmError::Deadlock) is the built-in scheduler's own outcome.
///
/// # Examples
///
/// ```
/// use bvm_lang::{Host, Program, Value, Vm, VmError};
/// use bytecode_lang::{ErrorKind, Inst, IntOp, IntTy, ModuleBuilder, ValType};
///
/// let mut m = ModuleBuilder::new();
/// let mut f = m.function("div", &[ValType::I64, ValType::I64], &[ValType::I64]);
/// let q = f.reg(ValType::I64);
/// f.emit(Inst::IDiv { dst: q, lhs: f.param(0), rhs: f.param(1), op: IntOp::new(IntTy::I64) });
/// f.ret(q);
/// let div = m.add_function(f).unwrap();
/// let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
///
/// let err = Vm::new(&program).run(div, &[Value::Int(1), Value::Int(0)]).unwrap_err();
/// assert_eq!(
///     err,
///     VmError::Raised { kind: ErrorKind::DivByZero, payload: Value::Nil, func: div, pc: 0 }
/// );
/// assert_eq!(err.code(), Some(2));
/// assert_eq!(err.to_string(), "uncaught E0002 DivByZero at f0 @0");
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum VmError {
    /// A runtime error no handler caught.
    Raised {
        /// The OPS/LSB error kind.
        kind: ErrorKind,
        /// Its payload: the operand of the `raise` that raised it (a
        /// `NoMatch` carries the unmatched value), `nil` for an error an
        /// instruction raised by itself (LSB §6).
        payload: Value,
        /// The function that raised it.
        func: FuncId,
        /// The instruction that raised it.
        pc: u32,
    },
    /// A `throw` (or host-thrown value) that is not a runtime error value
    /// reached the host uncaught.
    Thrown {
        /// The thrown value.
        value: Value,
        /// The function where it was thrown.
        func: FuncId,
        /// The throwing instruction.
        pc: u32,
    },
    /// A trap: `OutOfFuel`, `OutOfMemory`, `Unreachable`, or an OPS error
    /// under policy `trap`. Handlers never see traps.
    Trap {
        /// The trap's kind (its code is the error code).
        kind: ErrorKind,
        /// The function that trapped.
        func: FuncId,
        /// The instruction that trapped.
        pc: u32,
    },
    /// [`Vm::run_async`](crate::Vm::run_async): every remaining task waits
    /// for a task that cannot finish (the main task awaits something no task
    /// will complete), so the scheduler has nothing left to run.
    Deadlock {
        /// Tasks still waiting, the main task included.
        waiting: usize,
    },
    /// A global's initialiser could not be materialised into the global's
    /// type (or exhausted memory).
    GlobalInit {
        /// The global.
        global: GlobalId,
        /// The error or trap kind.
        kind: ErrorKind,
    },
    /// The entry function does not exist.
    NoSuchFunction(FuncId),
    /// No function is exported under the name.
    NoSuchExport,
    /// The entry function has captures, so it can only run as a closure.
    NeedsClosure(FuncId),
    /// The number of arguments does not match the entry function.
    ArgumentCount {
        /// The function's parameter count.
        expected: usize,
        /// The number of arguments given.
        found: usize,
    },
    /// An argument does not convert to its parameter's type.
    ArgumentType {
        /// The argument's position.
        index: usize,
    },
}

impl VmError {
    /// The error kind, for the variants that have one.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::VmError;
    /// use bytecode_lang::{ErrorKind, FuncId};
    ///
    /// let e = VmError::Trap { kind: ErrorKind::OutOfFuel, func: FuncId(0), pc: 3 };
    /// assert_eq!(e.kind(), Some(ErrorKind::OutOfFuel));
    /// assert_eq!(VmError::NoSuchExport.kind(), None);
    /// ```
    #[must_use]
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            VmError::Raised { kind, .. }
            | VmError::Trap { kind, .. }
            | VmError::GlobalInit { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// The numeric error code (`107` for E0107 `OutOfFuel`), for the
    /// variants that have a kind.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::VmError;
    /// use bytecode_lang::{ErrorKind, FuncId};
    ///
    /// let e = VmError::Trap { kind: ErrorKind::OutOfFuel, func: FuncId(0), pc: 3 };
    /// assert_eq!(e.code(), Some(107));
    /// ```
    #[must_use]
    pub fn code(&self) -> Option<u32> {
        self.kind().map(ErrorKind::code)
    }

    /// The function and pc where the run stopped, for runtime variants.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::VmError;
    /// use bytecode_lang::{ErrorKind, FuncId};
    ///
    /// let e = VmError::Trap { kind: ErrorKind::Unreachable, func: FuncId(2), pc: 9 };
    /// assert_eq!(e.location(), Some((FuncId(2), 9)));
    /// ```
    #[must_use]
    pub fn location(&self) -> Option<(FuncId, u32)> {
        match self {
            VmError::Raised { func, pc, .. }
            | VmError::Thrown { func, pc, .. }
            | VmError::Trap { func, pc, .. } => Some((*func, *pc)),
            _ => None,
        }
    }
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::Raised {
                kind,
                payload: Value::Nil,
                func,
                pc,
            } => write!(f, "uncaught {kind} at {func} @{pc}"),
            VmError::Raised {
                kind,
                payload,
                func,
                pc,
            } => write!(f, "uncaught {kind} ({payload}) at {func} @{pc}"),
            VmError::Thrown { value, func, pc } => {
                write!(f, "uncaught throw of {value} at {func} @{pc}")
            }
            VmError::Trap { kind, func, pc } => write!(f, "trap {kind} at {func} @{pc}"),
            VmError::Deadlock { waiting } => {
                write!(f, "deadlock: {waiting} tasks wait and none can run")
            }
            VmError::GlobalInit { global, kind } => {
                write!(f, "cannot initialise {global}: {kind}")
            }
            VmError::NoSuchFunction(id) => write!(f, "no function {id}"),
            VmError::NoSuchExport => f.write_str("no function exported under that name"),
            VmError::NeedsClosure(id) => {
                write!(f, "{id} has captures and can only run as a closure")
            }
            VmError::ArgumentCount { expected, found } => {
                write!(f, "expected {expected} arguments, got {found}")
            }
            VmError::ArgumentType { index } => {
                write!(f, "argument {index} does not convert to its parameter type")
            }
        }
    }
}

impl core::error::Error for VmError {}
