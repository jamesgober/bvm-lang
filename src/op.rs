//! The instruction set executed by the VM.
//!
//! `bvm-lang` is a *register machine*: every instruction names the registers it
//! reads and writes explicitly, rather than pushing and popping an operand stack.
//! A register machine issues far fewer instructions for the same work — a
//! three-address `Add { dst, lhs, rhs }` replaces a push/push/add/pop sequence —
//! which means fewer dispatch iterations through the interpreter loop and better
//! use of the CPU's branch predictor.
//!
//! Each [`Op`] is a fixed-size, already-decoded value: the operands are plain
//! integer fields, so the interpreter never parses a byte stream at runtime. A
//! program is just a `&[Op]` walked by a program counter, plus a pool of
//! [`Value`](crate::Value) constants.

/// A register index.
///
/// Registers are the VM's working storage. A [`Chunk`](crate::Chunk) sizes its
/// register file to the highest index any of its instructions names, so indices
/// are dense and start at zero. The 16-bit width allows up to 65 536 registers
/// per chunk, far more than a single function body needs.
pub type Reg = u16;

/// A constant-pool index.
///
/// Constants are [`Value`](crate::Value)s too large or too varied to encode as an
/// immediate (arbitrary floats, interned symbols). They live in the chunk's
/// constant pool and are addressed by this index via [`Op::LoadConst`].
pub type Const = u16;

/// An instruction address within a chunk's code array.
///
/// Branch targets are absolute indices, not relative offsets: `Jump { target }`
/// sets the program counter directly. Absolute targets keep back-patching during
/// code generation trivial — the target is known once, and never shifts if
/// earlier instructions change size (they never do).
pub type Addr = u32;

/// A single decoded instruction.
///
/// Instructions divide into data movement (`Move`, `Load*`), arithmetic
/// (`Add`…`Neg`), comparison (`Eq`…`Ge`), the logical `Not`, control flow
/// (`Jump`, `JumpIfTrue`, `JumpIfFalse`), and termination (`Return`, `Halt`).
///
/// Arithmetic and comparison operands are numeric: an integer combines with an
/// integer to give an integer (with overflow checked), and any float operand
/// promotes the result to float. Comparisons and `Not` produce booleans. See the
/// individual variants and [the crate root](crate) for the exact semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Op {
    /// Copy the value in `src` into `dst`.
    Move {
        /// Destination register.
        dst: Reg,
        /// Source register.
        src: Reg,
    },
    /// Load the constant at pool position `index` into `dst`.
    LoadConst {
        /// Destination register.
        dst: Reg,
        /// Constant-pool index (see [`Chunk::constant`](crate::Chunk::constant)).
        index: Const,
    },
    /// Load `nil` into `dst`.
    LoadNil {
        /// Destination register.
        dst: Reg,
    },
    /// Load the boolean `val` into `dst`.
    LoadBool {
        /// Destination register.
        dst: Reg,
        /// Immediate boolean.
        val: bool,
    },
    /// Load the 32-bit integer immediate `val` into `dst`.
    ///
    /// Small integers travel in the instruction itself, avoiding a constant-pool
    /// round trip for the common case.
    LoadInt {
        /// Destination register.
        dst: Reg,
        /// Immediate integer.
        val: i32,
    },

    /// `dst = lhs + rhs` (numeric; integer add is overflow-checked).
    Add {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = lhs - rhs` (numeric; integer subtract is overflow-checked).
    Sub {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = lhs * rhs` (numeric; integer multiply is overflow-checked).
    Mul {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = lhs / rhs` (numeric; integer divide-by-zero errors, float follows IEEE-754).
    Div {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = lhs % rhs` (numeric; integer remainder-by-zero errors).
    Rem {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = -src` (numeric; integer negation is overflow-checked).
    Neg {
        /// Destination register.
        dst: Reg,
        /// Source register.
        src: Reg,
    },

    /// `dst = (lhs == rhs)` as a boolean. Numeric operands compare by value across
    /// int/float; other kinds compare structurally. Floats use IEEE-754 equality.
    Eq {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = (lhs != rhs)` — the boolean negation of [`Op::Eq`].
    Ne {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = (lhs < rhs)` (numeric operands only).
    Lt {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = (lhs <= rhs)` (numeric operands only).
    Le {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = (lhs > rhs)` (numeric operands only).
    Gt {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },
    /// `dst = (lhs >= rhs)` (numeric operands only).
    Ge {
        /// Destination register.
        dst: Reg,
        /// Left operand register.
        lhs: Reg,
        /// Right operand register.
        rhs: Reg,
    },

    /// `dst = !src`. `src` must be a boolean.
    Not {
        /// Destination register.
        dst: Reg,
        /// Source register.
        src: Reg,
    },

    /// Unconditionally set the program counter to `target`.
    Jump {
        /// Absolute instruction address to continue from.
        target: Addr,
    },
    /// Branch to `target` when `cond` holds a `true` boolean; otherwise fall
    /// through. `cond` must be a boolean.
    JumpIfTrue {
        /// Condition register (must be a boolean).
        cond: Reg,
        /// Absolute instruction address taken when the condition is `true`.
        target: Addr,
    },
    /// Branch to `target` when `cond` holds a `false` boolean; otherwise fall
    /// through. `cond` must be a boolean.
    JumpIfFalse {
        /// Condition register (must be a boolean).
        cond: Reg,
        /// Absolute instruction address taken when the condition is `false`.
        target: Addr,
    },

    /// Stop execution and yield the value in `src` as the run's result.
    Return {
        /// Register holding the value to return.
        src: Reg,
    },
    /// Stop execution and yield `nil`.
    Halt,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn test_op_is_compact_and_copy() {
        // A decoded instruction must stay small enough to keep the code array
        // cache-dense. Eight bytes is the target on 64-bit targets.
        assert!(size_of::<Op>() <= 8, "Op grew to {} bytes", size_of::<Op>());
        fn assert_copy<T: Copy>(_: T) {}
        assert_copy(Op::Halt);
    }
}
