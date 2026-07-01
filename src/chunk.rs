//! A unit of executable bytecode: its instructions, its constants, and the size
//! of the register file they operate on.
//!
//! A [`Chunk`] is what you hand to [`Vm::run`](crate::Vm::run). It is built up
//! instruction by instruction with [`emit`](Chunk::emit); large or non-integer
//! literals go into the constant pool with [`constant`](Chunk::constant), which
//! returns the index a [`LoadConst`](crate::Op::LoadConst) uses to read them back.
//!
//! The register-file size is tracked automatically: every emitted instruction
//! widens the file to cover the highest register it names, so callers never size
//! it by hand and can never under-provision it. Forward branches — the jump out
//! of an `if`, the back-edge of a loop — are written with a placeholder target
//! and later fixed up with [`patch`](Chunk::patch) once the destination is known.

use crate::Op;
use alloc::vec::Vec;
use value_lang::Value;

/// An assembled sequence of instructions with its constant pool.
///
/// Cloning a chunk is a deep copy of both vectors; it is cheap relative to
/// execution and lets a compiled program be run many times. A chunk carries no
/// interpreter state — that lives in the [`Vm`](crate::Vm) — so the same chunk can
/// be executed concurrently by separate VMs.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Chunk {
    code: Vec<Op>,
    constants: Vec<Value>,
    registers: u16,
}

impl Chunk {
    /// Create an empty chunk with no instructions, no constants, and a
    /// zero-width register file.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Chunk;
    ///
    /// let chunk = Chunk::new();
    /// assert!(chunk.is_empty());
    /// assert_eq!(chunk.registers(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an instruction and return its address.
    ///
    /// The returned address is the index the instruction occupies in the code
    /// array — the same value a branch uses as its target, and the argument
    /// [`patch`](Chunk::patch) takes to rewrite this slot later. Emitting widens
    /// the register file to include every register the instruction names.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Chunk, Op};
    ///
    /// let mut chunk = Chunk::new();
    /// let first = chunk.emit(Op::LoadInt { dst: 0, val: 41 });
    /// assert_eq!(first, 0);
    /// // Naming register 0 sized the file to one slot.
    /// assert_eq!(chunk.registers(), 1);
    /// ```
    pub fn emit(&mut self, op: Op) -> usize {
        if let Some(top) = max_register(op) {
            // `top` is the highest index named; the file must hold `top + 1`
            // slots. Saturating keeps the arithmetic total even at `u16::MAX`.
            self.registers = self.registers.max(top.saturating_add(1));
        }
        let addr = self.code.len();
        self.code.push(op);
        addr
    }

    /// Add `value` to the constant pool and return its index for
    /// [`LoadConst`](crate::Op::LoadConst).
    ///
    /// Constants are not deduplicated; adding the same value twice yields two
    /// slots. This keeps the builder allocation-light and predictable — a
    /// compiler that wants interning can layer it on top.
    ///
    /// # Errors
    ///
    /// Returns [`None`] if the pool already holds `u16::MAX + 1` constants, the
    /// most a [`Const`](crate::Const) index can address. A single function body
    /// reaching 65 536 distinct constants is pathological, so callers typically
    /// treat this as unreachable rather than a runtime path.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Chunk, Op, Value};
    ///
    /// let mut chunk = Chunk::new();
    /// let k = chunk.constant(Value::float(3.5)).expect("pool has room");
    /// chunk.emit(Op::LoadConst { dst: 0, konst: k });
    /// ```
    #[must_use = "the returned index is how the constant is later loaded"]
    pub fn constant(&mut self, value: Value) -> Option<u16> {
        let index = u16::try_from(self.constants.len()).ok()?;
        self.constants.push(value);
        Some(index)
    }

    /// Overwrite the instruction at `addr`, typically to fill in a branch target
    /// that was not known when the instruction was first emitted.
    ///
    /// Returns `true` if `addr` was a valid slot and the rewrite happened, and
    /// `false` if it was out of range. Rewriting can change which registers the
    /// chunk touches, so the register file is re-derived from scratch to stay
    /// exact.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Chunk, Op};
    ///
    /// let mut chunk = Chunk::new();
    /// // Emit a forward branch whose target is not yet known.
    /// let jump = chunk.emit(Op::Jump { target: 0 });
    /// chunk.emit(Op::LoadInt { dst: 0, val: 1 });
    /// let landing = chunk.len() as u32;
    /// // Now patch the branch to skip the load.
    /// assert!(chunk.patch(jump, Op::Jump { target: landing }));
    /// ```
    pub fn patch(&mut self, addr: usize, op: Op) -> bool {
        let Some(slot) = self.code.get_mut(addr) else {
            return false;
        };
        *slot = op;
        self.recompute_registers();
        true
    }

    /// The chunk's instructions, in address order.
    #[must_use]
    pub fn code(&self) -> &[Op] {
        &self.code
    }

    /// The chunk's constant pool, indexed by [`LoadConst`](crate::Op::LoadConst).
    #[must_use]
    pub fn constants(&self) -> &[Value] {
        &self.constants
    }

    /// The number of registers the chunk uses — one more than the highest
    /// register index any instruction names, or zero for a chunk that names none.
    #[must_use]
    pub fn registers(&self) -> u16 {
        self.registers
    }

    /// The number of instructions in the chunk.
    #[must_use]
    pub fn len(&self) -> usize {
        self.code.len()
    }

    /// Whether the chunk contains no instructions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// Re-derive the register-file size from the current code. Used after
    /// [`patch`](Chunk::patch), where an edit may have removed the instruction
    /// that justified the previous width.
    fn recompute_registers(&mut self) {
        self.registers = self
            .code
            .iter()
            .filter_map(|op| max_register(*op))
            .map(|top| top.saturating_add(1))
            .max()
            .unwrap_or(0);
    }
}

/// The highest register index an instruction names, or `None` if it names none
/// (`Jump`, `Halt`).
#[inline]
fn max_register(op: Op) -> Option<u16> {
    match op {
        Op::Move { dst, src } | Op::Neg { dst, src } | Op::Not { dst, src } => Some(dst.max(src)),
        Op::LoadConst { dst, .. }
        | Op::LoadNil { dst }
        | Op::LoadBool { dst, .. }
        | Op::LoadInt { dst, .. } => Some(dst),
        Op::Add { dst, lhs, rhs }
        | Op::Sub { dst, lhs, rhs }
        | Op::Mul { dst, lhs, rhs }
        | Op::Div { dst, lhs, rhs }
        | Op::Rem { dst, lhs, rhs }
        | Op::Eq { dst, lhs, rhs }
        | Op::Ne { dst, lhs, rhs }
        | Op::Lt { dst, lhs, rhs }
        | Op::Le { dst, lhs, rhs }
        | Op::Gt { dst, lhs, rhs }
        | Op::Ge { dst, lhs, rhs } => Some(dst.max(lhs).max(rhs)),
        Op::JumpIfTrue { cond, .. } | Op::JumpIfFalse { cond, .. } | Op::Return { src: cond } => {
            Some(cond)
        }
        Op::Jump { .. } | Op::Halt => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_emit_returns_sequential_addresses() {
        let mut c = Chunk::new();
        assert_eq!(c.emit(Op::Halt), 0);
        assert_eq!(c.emit(Op::Halt), 1);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn test_register_file_tracks_highest_index() {
        let mut c = Chunk::new();
        let _ = c.emit(Op::Add {
            dst: 5,
            lhs: 2,
            rhs: 9,
        });
        assert_eq!(c.registers(), 10);
    }

    #[test]
    fn test_constant_returns_incrementing_indices() {
        let mut c = Chunk::new();
        assert_eq!(c.constant(Value::int(1)), Some(0));
        assert_eq!(c.constant(Value::int(2)), Some(1));
        assert_eq!(c.constants().len(), 2);
    }

    #[test]
    fn test_patch_replaces_instruction_and_reports_success() {
        let mut c = Chunk::new();
        let at = c.emit(Op::Jump { target: 0 });
        assert!(c.patch(at, Op::Jump { target: 7 }));
        assert_eq!(c.code()[at], Op::Jump { target: 7 });
    }

    #[test]
    fn test_patch_out_of_range_returns_false() {
        let mut c = Chunk::new();
        assert!(!c.patch(0, Op::Halt));
    }

    #[test]
    fn test_patch_recomputes_register_file() {
        let mut c = Chunk::new();
        let at = c.emit(Op::LoadInt { dst: 40, val: 1 });
        assert_eq!(c.registers(), 41);
        // Replace the wide instruction; the file should shrink to fit.
        assert!(c.patch(at, Op::Halt));
        assert_eq!(c.registers(), 0);
    }

    #[test]
    fn test_new_chunk_is_empty() {
        let c = Chunk::new();
        assert!(c.is_empty());
        assert_eq!(c.registers(), 0);
    }
}
