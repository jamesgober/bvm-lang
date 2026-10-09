//! The internal outcome of an instruction that does not complete.

use bytecode_lang::{ErrorKind, Opcode};

/// Why an instruction stopped. A `Raise` or `Throw` unwinds to the nearest
/// handler (LSB §4.3); a `Trap` or `Unsupported` ends the run at once.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Fault {
    /// A catchable runtime error with its OPS/LSB kind. The VM allocates the
    /// `error` value when it starts unwinding.
    Raise(ErrorKind),
    /// A catchable error carrying an arbitrary `dyn` value (`throw`, or a host
    /// function's thrown value).
    Throw(u64),
    /// A non-catchable abort: `OutOfFuel`, `OutOfMemory`, `Unreachable`, or an
    /// OPS kind under policy `trap`.
    Trap(ErrorKind),
    /// An instruction this release does not execute.
    Unsupported(Opcode),
}

impl Fault {
    /// The fault for an OPS error kind under a policy that traps or not.
    #[cold]
    #[inline(never)]
    pub(crate) fn ops(kind: ErrorKind, trap: bool) -> Fault {
        if trap {
            Fault::Trap(kind)
        } else {
            Fault::Raise(kind)
        }
    }

    /// A catchable `TypeError`.
    #[cold]
    #[inline(never)]
    pub(crate) fn type_error() -> Fault {
        Fault::Raise(ErrorKind::TypeError)
    }

    /// A catchable error of `kind`.
    #[cold]
    #[inline(never)]
    pub(crate) fn raise(kind: ErrorKind) -> Fault {
        Fault::Raise(kind)
    }
}
