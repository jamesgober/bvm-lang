//! Execution failures raised by the interpreter loop.
//!
//! Bytecode handed to [`Vm::run`](crate::Vm::run) is treated as untrusted: a
//! malformed program (an out-of-range register, a jump past the end of the code,
//! a constant index that does not exist) is reported as a [`VmError`] rather than
//! panicking. Runtime faults from otherwise well-formed code — a type mismatch, a
//! division by zero, integer overflow — surface the same way. Every variant
//! carries enough context to point at the specific fault.

use core::fmt;

/// An error raised while executing a [`Chunk`](crate::Chunk).
///
/// Errors fall into two groups. *Structural* faults (`BadRegister`,
/// `BadConstant`, `BadJump`, `NoTerminator`) mean the bytecode itself is
/// malformed and would never be produced by a correct compiler; they are
/// reported instead of trusted so that hand-written or corrupted programs cannot
/// drive the VM into a panic. *Runtime* faults (`TypeMismatch`, `DivideByZero`,
/// `IntegerOverflow`) come from executing well-formed instructions against
/// operands that do not satisfy their contract.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VmError {
    /// An operation received an operand of the wrong kind — for example, adding a
    /// boolean to an integer, or branching on a value that is not a boolean.
    ///
    /// The field names the operation that rejected its operand (`"add"`,
    /// `"jump-if-true"`, …) so a caller can report which instruction faulted.
    TypeMismatch {
        /// The operation that rejected its operand(s).
        op: &'static str,
    },

    /// Integer division or remainder with a zero divisor.
    ///
    /// Only integer operands raise this. Floating-point division by zero follows
    /// IEEE-754 and yields an infinity or NaN rather than an error.
    DivideByZero,

    /// A checked integer operation overflowed its 32-bit range.
    ///
    /// Raised by addition, subtraction, multiplication, negation, and the
    /// `i32::MIN / -1` division/remainder edge case. Integer arithmetic never
    /// wraps silently.
    IntegerOverflow,

    /// A register index addressed a slot outside the chunk's register file.
    ///
    /// Indicates malformed bytecode: a correct compiler only emits register
    /// indices below [`Chunk::registers`](crate::Chunk::registers).
    BadRegister(u16),

    /// A `LoadConst` referenced a constant-pool slot that does not exist.
    BadConstant(u16),

    /// Control flow reached an instruction index outside the code array — either
    /// a branch target past the end, or a `pc` that walked off the end.
    BadJump(u32),

    /// Execution reached the end of the code without a `Return` or `Halt`.
    ///
    /// Every well-formed program terminates explicitly; falling off the end is
    /// treated as a structural fault rather than an implicit return.
    NoTerminator,
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TypeMismatch { op } => {
                write!(
                    f,
                    "type mismatch: `{op}` received an operand of the wrong kind"
                )
            }
            Self::DivideByZero => f.write_str("integer division or remainder by zero"),
            Self::IntegerOverflow => f.write_str("integer arithmetic overflowed the 32-bit range"),
            Self::BadRegister(r) => write!(f, "register index {r} is outside the register file"),
            Self::BadConstant(c) => write!(f, "constant index {c} does not exist"),
            Self::BadJump(t) => write!(f, "branch target {t} is outside the code"),
            Self::NoTerminator => {
                f.write_str("execution reached the end of the code without a return or halt")
            }
        }
    }
}

impl core::error::Error for VmError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn test_display_type_mismatch_names_op() {
        let msg = VmError::TypeMismatch { op: "add" }.to_string();
        assert!(msg.contains("add"), "message should name the op: {msg}");
    }

    #[test]
    fn test_display_all_variants_nonempty() {
        let variants = [
            VmError::TypeMismatch { op: "neg" },
            VmError::DivideByZero,
            VmError::IntegerOverflow,
            VmError::BadRegister(7),
            VmError::BadConstant(3),
            VmError::BadJump(99),
            VmError::NoTerminator,
        ];
        for v in variants {
            assert!(!v.to_string().is_empty());
        }
    }

    #[test]
    fn test_error_is_std_error() {
        fn assert_error<E: core::error::Error>(_: &E) {}
        assert_error(&VmError::DivideByZero);
    }
}
