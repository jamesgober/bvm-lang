//! The interpreter: a register file and the dispatch loop that drives it.
//!
//! [`Vm`] holds the one piece of mutable execution state — the register file —
//! and reuses it across runs so a long-lived VM executing many chunks does not
//! reallocate. [`Vm::run`] walks a [`Chunk`](crate::Chunk)'s code with a program
//! counter, dispatching each instruction through a single `match`. On a modern
//! compiler that `match` lowers to a jump table, so per-instruction overhead is a
//! table lookup plus the operation itself.
//!
//! The loop is written to be safe against malformed bytecode. Every register,
//! constant, and branch access is checked, so a corrupt or hand-crafted chunk
//! yields a [`VmError`] instead of a panic or undefined behaviour — the crate
//! forbids `unsafe`, and this loop is where that guarantee is enforced.

use crate::{Chunk, Op, VmError, eval};
use alloc::vec::Vec;
use value_lang::Value;

/// A bytecode interpreter.
///
/// A `Vm` owns a register file that is cleared and resized to fit at the start of
/// each [`run`](Vm::run). Reusing one `Vm` across many runs amortises that
/// allocation to zero in steady state. A `Vm` holds no reference to any chunk, so
/// one instance can execute different chunks in sequence, and different instances
/// can execute the same chunk on different threads.
///
/// # Examples
///
/// ```
/// use vm_lang::{Chunk, Op, Value, Vm};
///
/// // Compute 2 + 3 and return it.
/// let mut chunk = Chunk::new();
/// chunk.emit(Op::LoadInt { dst: 0, val: 2 });
/// chunk.emit(Op::LoadInt { dst: 1, val: 3 });
/// chunk.emit(Op::Add { dst: 0, lhs: 0, rhs: 1 });
/// chunk.emit(Op::Return { src: 0 });
///
/// let mut vm = Vm::new();
/// assert_eq!(vm.run(&chunk).unwrap().as_int(), Some(5));
/// ```
#[derive(Debug, Default)]
pub struct Vm {
    registers: Vec<Value>,
}

impl Vm {
    /// Create a VM with an empty register file.
    ///
    /// The file grows to fit the first chunk executed and is reused thereafter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a VM whose register file is pre-allocated for at least `registers`
    /// slots, avoiding a growth reallocation on the first run of a chunk that
    /// size.
    ///
    /// # Examples
    ///
    /// ```
    /// use vm_lang::Vm;
    ///
    /// // A VM primed for chunks using up to 32 registers.
    /// let mut vm = Vm::with_capacity(32);
    /// ```
    #[must_use]
    pub fn with_capacity(registers: u16) -> Self {
        Self {
            registers: Vec::with_capacity(registers as usize),
        }
    }

    /// Execute `chunk` from its first instruction and return the value it yields.
    ///
    /// A [`Return`](Op::Return) yields the value in its register; a
    /// [`Halt`](Op::Halt), or a chunk with no reachable terminator that instead
    /// stops via `Halt`, yields `nil`. The register file is reset to `nil` and
    /// sized to the chunk before execution begins, so a run never observes
    /// residue from a previous one.
    ///
    /// # Errors
    ///
    /// Returns a [`VmError`] on a runtime fault (type mismatch, division by zero,
    /// integer overflow) or a structural fault in the bytecode (an out-of-range
    /// register, constant, or branch target, or running off the end of the code
    /// without terminating). See [`VmError`] for the full set.
    ///
    /// # Examples
    ///
    /// A division by zero surfaces as an error rather than a panic:
    ///
    /// ```
    /// use vm_lang::{Chunk, Op, VmError, Vm};
    ///
    /// let mut chunk = Chunk::new();
    /// chunk.emit(Op::LoadInt { dst: 0, val: 1 });
    /// chunk.emit(Op::LoadInt { dst: 1, val: 0 });
    /// chunk.emit(Op::Div { dst: 0, lhs: 0, rhs: 1 });
    /// chunk.emit(Op::Return { src: 0 });
    ///
    /// let mut vm = Vm::new();
    /// assert_eq!(vm.run(&chunk), Err(VmError::DivideByZero));
    /// ```
    pub fn run(&mut self, chunk: &Chunk) -> Result<Value, VmError> {
        self.registers.clear();
        self.registers
            .resize(chunk.registers() as usize, Value::nil());

        let code = chunk.code();
        let constants = chunk.constants();
        let end = code.len();
        let mut pc: usize = 0;

        loop {
            // Fetch. A `pc` at or past the end means control fell through the
            // last instruction without a terminator; branches validate their own
            // targets before landing here, so this only ever signals that fault.
            let op = *code.get(pc).ok_or(VmError::NoTerminator)?;

            match op {
                Op::Return { src } => return self.get(src),
                Op::Halt => return Ok(Value::nil()),

                Op::Jump { target } => {
                    pc = jump_target(target, end)?;
                    continue;
                }
                Op::JumpIfTrue { cond, target } => {
                    if eval::cond(self.get(cond)?, "jump-if-true")? {
                        pc = jump_target(target, end)?;
                        continue;
                    }
                }
                Op::JumpIfFalse { cond, target } => {
                    if !eval::cond(self.get(cond)?, "jump-if-false")? {
                        pc = jump_target(target, end)?;
                        continue;
                    }
                }

                Op::Move { dst, src } => {
                    let v = self.get(src)?;
                    self.set(dst, v)?;
                }
                Op::LoadConst { dst, konst } => {
                    let v = *constants
                        .get(konst as usize)
                        .ok_or(VmError::BadConstant(konst))?;
                    self.set(dst, v)?;
                }
                Op::LoadNil { dst } => self.set(dst, Value::nil())?,
                Op::LoadBool { dst, val } => self.set(dst, Value::bool(val))?,
                Op::LoadInt { dst, val } => self.set(dst, Value::int(val))?,

                Op::Add { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::add)?,
                Op::Sub { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::sub)?,
                Op::Mul { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::mul)?,
                Op::Div { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::div)?,
                Op::Rem { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::rem)?,
                Op::Neg { dst, src } => {
                    let v = eval::neg(self.get(src)?)?;
                    self.set(dst, v)?;
                }

                Op::Eq { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::eq_op)?,
                Op::Ne { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::ne_op)?,
                Op::Lt { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::lt)?,
                Op::Le { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::le)?,
                Op::Gt { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::gt)?,
                Op::Ge { dst, lhs, rhs } => self.binop(dst, lhs, rhs, eval::ge)?,

                Op::Not { dst, src } => {
                    let v = eval::not(self.get(src)?)?;
                    self.set(dst, v)?;
                }
            }

            pc += 1;
        }
    }

    /// Read register `r`, or fault if it is outside the register file.
    #[inline]
    fn get(&self, r: u16) -> Result<Value, VmError> {
        self.registers
            .get(r as usize)
            .copied()
            .ok_or(VmError::BadRegister(r))
    }

    /// Write `value` into register `r`, or fault if it is outside the file.
    #[inline]
    fn set(&mut self, r: u16, value: Value) -> Result<(), VmError> {
        let slot = self
            .registers
            .get_mut(r as usize)
            .ok_or(VmError::BadRegister(r))?;
        *slot = value;
        Ok(())
    }

    /// Evaluate a three-address binary instruction: read both operands, apply
    /// `f`, store into `dst`. Shared by every arithmetic and comparison opcode.
    #[inline]
    fn binop(
        &mut self,
        dst: u16,
        lhs: u16,
        rhs: u16,
        f: fn(Value, Value) -> Result<Value, VmError>,
    ) -> Result<(), VmError> {
        let a = self.get(lhs)?;
        let b = self.get(rhs)?;
        let v = f(a, b)?;
        self.set(dst, v)
    }
}

/// Validate a branch target against the code length.
///
/// A target equal to or past the end can never be a real instruction, so it is a
/// structural fault rather than a silent halt.
#[inline]
fn jump_target(target: u32, end: usize) -> Result<usize, VmError> {
    let t = target as usize;
    if t >= end {
        Err(VmError::BadJump(target))
    } else {
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Build, run, and unwrap a small chunk in one shot.
    fn run(ops: &[Op], constants: &[Value]) -> Result<Value, VmError> {
        let mut chunk = Chunk::new();
        for &v in constants {
            let _ = chunk.constant(v);
        }
        for &op in ops {
            let _ = chunk.emit(op);
        }
        Vm::new().run(&chunk)
    }

    #[test]
    fn test_return_yields_register_value() {
        let out = run(
            &[Op::LoadInt { dst: 0, val: 42 }, Op::Return { src: 0 }],
            &[],
        )
        .unwrap();
        assert_eq!(out.as_int(), Some(42));
    }

    #[test]
    fn test_halt_yields_nil() {
        let out = run(&[Op::Halt], &[]).unwrap();
        assert!(out.is_nil());
    }

    #[test]
    fn test_load_const_reads_pool() {
        let out = run(
            &[Op::LoadConst { dst: 0, konst: 0 }, Op::Return { src: 0 }],
            &[Value::float(2.5)],
        )
        .unwrap();
        assert_eq!(out.as_float(), Some(2.5));
    }

    #[test]
    fn test_arithmetic_chain() {
        // (4 * 5) - 2 = 18
        let out = run(
            &[
                Op::LoadInt { dst: 0, val: 4 },
                Op::LoadInt { dst: 1, val: 5 },
                Op::Mul {
                    dst: 0,
                    lhs: 0,
                    rhs: 1,
                },
                Op::LoadInt { dst: 1, val: 2 },
                Op::Sub {
                    dst: 0,
                    lhs: 0,
                    rhs: 1,
                },
                Op::Return { src: 0 },
            ],
            &[],
        )
        .unwrap();
        assert_eq!(out.as_int(), Some(18));
    }

    #[test]
    fn test_conditional_branch_taken() {
        // if 1 < 2 { return 10 } else { return 20 }
        let out = run(
            &[
                Op::LoadInt { dst: 0, val: 1 },
                Op::LoadInt { dst: 1, val: 2 },
                Op::Lt {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                Op::JumpIfFalse { cond: 2, target: 6 },
                Op::LoadInt { dst: 0, val: 10 },
                Op::Return { src: 0 },
                Op::LoadInt { dst: 0, val: 20 },
                Op::Return { src: 0 },
            ],
            &[],
        )
        .unwrap();
        assert_eq!(out.as_int(), Some(10));
    }

    #[test]
    fn test_loop_sums_with_back_edge() {
        // sum = 0; i = 1; while i <= 5 { sum += i; i += 1 } return sum  => 15
        let mut chunk = Chunk::new();
        // r0 = sum, r1 = i, r2 = limit, r3 = one, r4 = cond
        let _ = chunk.emit(Op::LoadInt { dst: 0, val: 0 });
        let _ = chunk.emit(Op::LoadInt { dst: 1, val: 1 });
        let _ = chunk.emit(Op::LoadInt { dst: 2, val: 5 });
        let _ = chunk.emit(Op::LoadInt { dst: 3, val: 1 });
        let cond_at = chunk.emit(Op::Le {
            dst: 4,
            lhs: 1,
            rhs: 2,
        });
        let exit_branch = chunk.emit(Op::JumpIfFalse { cond: 4, target: 0 });
        let _ = chunk.emit(Op::Add {
            dst: 0,
            lhs: 0,
            rhs: 1,
        });
        let _ = chunk.emit(Op::Add {
            dst: 1,
            lhs: 1,
            rhs: 3,
        });
        let _ = chunk.emit(Op::Jump {
            target: cond_at as u32,
        });
        let exit = chunk.emit(Op::Return { src: 0 });
        assert!(chunk.patch(
            exit_branch,
            Op::JumpIfFalse {
                cond: 4,
                target: exit as u32,
            },
        ));

        assert_eq!(Vm::new().run(&chunk).unwrap().as_int(), Some(15));
    }

    #[test]
    fn test_no_terminator_errors() {
        assert_eq!(
            run(&[Op::LoadInt { dst: 0, val: 1 }], &[]),
            Err(VmError::NoTerminator)
        );
    }

    #[test]
    fn test_empty_chunk_errors() {
        assert_eq!(Vm::new().run(&Chunk::new()), Err(VmError::NoTerminator));
    }

    #[test]
    fn test_bad_constant_index_errors() {
        assert_eq!(
            run(
                &[Op::LoadConst { dst: 0, konst: 3 }, Op::Return { src: 0 }],
                &[]
            ),
            Err(VmError::BadConstant(3))
        );
    }

    #[test]
    fn test_out_of_range_jump_errors() {
        assert_eq!(
            run(&[Op::Jump { target: 99 }], &[]),
            Err(VmError::BadJump(99))
        );
    }

    #[test]
    fn test_branch_on_non_bool_type_mismatch() {
        assert_eq!(
            run(
                &[
                    Op::LoadInt { dst: 0, val: 1 },
                    Op::JumpIfTrue { cond: 0, target: 0 },
                    Op::Halt,
                ],
                &[],
            ),
            Err(VmError::TypeMismatch { op: "jump-if-true" })
        );
    }

    #[test]
    fn test_vm_reuse_resets_registers() {
        let mut vm = Vm::new();
        let first = {
            let mut c = Chunk::new();
            let _ = c.emit(Op::LoadInt { dst: 0, val: 7 });
            let _ = c.emit(Op::Return { src: 0 });
            vm.run(&c).unwrap()
        };
        assert_eq!(first.as_int(), Some(7));
        // A second chunk that returns an untouched register must see `nil`,
        // proving the file was reset rather than carrying `7` forward.
        let mut c = Chunk::new();
        let _ = c.emit(Op::LoadInt { dst: 5, val: 1 });
        let _ = c.emit(Op::Return { src: 0 });
        assert!(vm.run(&c).unwrap().is_nil());
    }
}
