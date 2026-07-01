//! # bvm_lang
//!
//! A small, fast bytecode virtual machine — the execution engine an interpreted
//! language runs on once its source has been compiled down to instructions.
//!
//! `bvm-lang` is a **register machine**. Instructions name the registers they read
//! and write (`Add { dst, lhs, rhs }`) instead of shuffling an operand stack, so a
//! program runs in far fewer dispatch steps than the stack-machine equivalent.
//! Each [`Op`] is a fixed-size, already-decoded value; a program is a slice of
//! them plus a pool of constants, walked by a program counter in a single
//! `match`-based dispatch loop. The whole crate is safe Rust — `unsafe` is
//! forbidden — and treats the bytecode it runs as untrusted: a malformed program
//! produces a [`VmError`], never a panic.
//!
//! The runtime value type is [`Value`] from
//! [`value-lang`](https://docs.rs/value-lang): an eight-byte NaN-boxed handle that
//! represents `nil`, a boolean, a 32-bit integer, a float, or an interned symbol.
//! Every register holds one, and the VM speaks the same value as the rest of the
//! language-construction family.
//!
//! ## The pieces
//!
//! - [`Op`] — the instruction set: data movement, arithmetic, comparison, logic,
//!   branches, and termination.
//! - [`Chunk`] — a built program: instructions, a constant pool, and an
//!   automatically sized register file. Construct one with [`Chunk::emit`] and
//!   [`Chunk::constant`]; fix up forward branches with [`Chunk::patch`].
//! - [`Vm`] — the interpreter. [`Vm::run`] executes a chunk and returns its
//!   result [`Value`].
//! - [`VmError`] — every way a run can fail, from a type mismatch to a corrupt
//!   branch target.
//!
//! ## Example
//!
//! Compile and run `(2 + 3) * 4`:
//!
//! ```
//! use bvm_lang::{Chunk, Op, Vm};
//!
//! let mut chunk = Chunk::new();
//! chunk.emit(Op::LoadInt { dst: 0, val: 2 });
//! chunk.emit(Op::LoadInt { dst: 1, val: 3 });
//! chunk.emit(Op::Add { dst: 0, lhs: 0, rhs: 1 }); // r0 = 2 + 3
//! chunk.emit(Op::LoadInt { dst: 1, val: 4 });
//! chunk.emit(Op::Mul { dst: 0, lhs: 0, rhs: 1 }); // r0 = 5 * 4
//! chunk.emit(Op::Return { src: 0 });
//!
//! let mut vm = Vm::new();
//! let result = vm.run(&chunk).expect("well-formed program");
//! assert_eq!(result.as_int(), Some(20));
//! ```
//!
//! ## Semantics
//!
//! Arithmetic and ordering work over a single numeric tower: integer-with-integer
//! stays an integer and is **overflow-checked** (a fault is reported, never a
//! silent wrap), while any float operand promotes the result to float. Integer
//! division or remainder by zero is a [`VmError::DivideByZero`]; float division by
//! zero follows IEEE-754. Comparisons and [`Op::Not`] produce booleans, and every
//! branch condition must be a boolean. See [`Op`] for per-instruction detail.
//!
//! ## `no_std`
//!
//! The crate is `no_std`-compatible and needs only `alloc` (the code, constant
//! pool, and register file are heap-allocated vectors). The default `std` feature
//! is additive and forwards to `value-lang`. The optional `serde` feature derives
//! `Serialize` / `Deserialize` for [`Op`] and [`Chunk`], so compiled bytecode can
//! be persisted and reloaded.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::unreachable)]
#![forbid(unsafe_code)]

extern crate alloc;

mod chunk;
mod error;
mod eval;
mod op;
mod vm;

pub use chunk::Chunk;
pub use error::VmError;
pub use op::{Addr, Const, Op, Reg};
pub use vm::Vm;

/// The runtime value every register holds, re-exported from
/// [`value-lang`](https://docs.rs/value-lang).
///
/// See [`Value`] for the eight-byte NaN-boxed representation and its
/// constructors and accessors. [`Unpacked`] is the tagged-union view for matching
/// on a value's kind, and [`Symbol`] is the interned-string handle a value can
/// carry.
pub use value_lang::{Symbol, Unpacked, Value};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_public_surface_runs_end_to_end() {
        let mut chunk = Chunk::new();
        let k = chunk.constant(Value::float(1.5)).expect("pool has room");
        let _ = chunk.emit(Op::LoadConst { dst: 0, konst: k });
        let _ = chunk.emit(Op::LoadInt { dst: 1, val: 2 });
        let _ = chunk.emit(Op::Add {
            dst: 0,
            lhs: 0,
            rhs: 1,
        });
        let _ = chunk.emit(Op::Return { src: 0 });

        let mut vm = Vm::new();
        assert_eq!(vm.run(&chunk).map(|v| v.as_float()), Ok(Some(3.5)));
    }
}
