//! # bvm_lang
//!
//! The virtual machine that executes **LSB**, the LexerSketch bytecode
//! defined by [`bytecode-lang`](https://docs.rs/bytecode-lang): the T1
//! interpreter of the `-lang` family, for static languages (typed registers,
//! unboxed integers and floats) and dynamic ones (`dyn` registers, PHP-style
//! ordered maps, hooks) alike.
//!
//! ## The model
//!
//! - A [`Program`] is a loaded module: [`Program::load`] takes a
//!   `bytecode_lang::Module` (or [`Program::decode`] takes its bytes), checks
//!   every index the interpreter will use, and binds the module's imports to
//!   functions registered in a [`Host`]. Loading is linear in the module's
//!   size; after it the dispatch loop never meets an out-of-range index.
//! - A [`Vm`] is an instance of a program: a heap with a tracing collector,
//!   the globals, and the [`Limits`] its runs execute under (fuel, memory,
//!   call depth, register stack). [`Vm::run`] calls a function with
//!   [`Value`] arguments and returns its result.
//! - A [`VmError`] is how a run ends without a result: an uncaught error
//!   with its OPS/LSB code and the function and pc that raised it, an
//!   uncaught throw, a trap (`OutOfFuel`, `OutOfMemory`, `Unreachable`), or
//!   an instruction this release does not execute.
//!
//! ## Example
//!
//! A loop that sums `0..n`, with a safepoint on its back edge (where fuel is
//! charged):
//!
//! ```
//! use bvm_lang::{Host, Program, Value, Vm};
//! use bytecode_lang::{Inst, IntOp, IntTy, ModuleBuilder, ValType};
//!
//! let mut m = ModuleBuilder::new();
//! let mut f = m.function("sum", &[ValType::I64], &[ValType::I64]);
//! let n = f.param(0);
//! let (acc, i, one, more) = (f.reg(ValType::I64), f.reg(ValType::I64), f.reg(ValType::I64), f.reg(ValType::Bool));
//! let op = IntOp::new(IntTy::I64);
//! f.emit(Inst::LoadInt { dst: one, val: 1, ty: IntTy::I64 });
//! let (top, done) = (f.label(), f.label());
//! f.bind(top);
//! f.emit(Inst::ILt { dst: more, lhs: i, rhs: n, ty: IntTy::I64 });
//! f.jmp_if_not(more, done);
//! f.emit(Inst::IAdd { dst: acc, lhs: acc, rhs: i, op });
//! f.emit(Inst::IAdd { dst: i, lhs: i, rhs: one, op });
//! f.emit(Inst::Safepoint {});
//! f.jmp(top);
//! f.bind(done);
//! f.ret(acc);
//! let sum = m.add_function(f).unwrap();
//!
//! let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
//! let mut vm = Vm::new(&program);
//! assert_eq!(vm.run(sum, &[Value::Int(100)]), Ok(Value::Int(4950)));
//! ```
//!
//! ## Guarantees
//!
//! - **Exact OPS semantics.** Every integer instruction applies the policies
//!   in its own modifier (`error`, `wrap`, `trap`, and `promote` on the
//!   dynamic instructions) bit for bit as OPS defines them; float results are
//!   IEEE 754 with correctly rounded `sqrt` and `fma`, CPython's float floor
//!   division, and the IEEE `remainder`.
//! - **No panics, no unbounded work on untrusted modules.** Loading rejects
//!   any module whose indices the interpreter could not trust; at run time
//!   fuel bounds work, the memory budget bounds the heap, and depth and
//!   stack limits bound frames. The crate is `#![forbid(unsafe_code)]`.
//! - **Precise errors.** A failing instruction raises at its own pc without
//!   writing its destination, so handlers see the registers as they were.
//!
//! ## Not in this release
//!
//! The coroutine instructions (`0xF0`..=`0xFC`) load but do not execute: they
//! end the run with [`VmError::Unsupported`]. They arrive in 2.0.0-alpha.2.
//!
//! ## `no_std`
//!
//! Without the default `std` feature the crate needs only `alloc`; float
//! routines are then computed in software (bit-identical to the hardware
//! ones) and the map hashing key is fixed rather than random.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
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

extern crate alloc;
// Unit tests compare against `std` (float routines, SipHash) even in
// `no_std` builds.
#[cfg(test)]
extern crate std;

mod coll;
mod conv;
mod dynops;
mod dynv;
mod error;
mod exec;
mod fault;
mod fmath;
mod hash;
mod heap;
mod host;
mod int;
mod machine;
mod map;
mod program;
mod value;
mod vm;

pub use error::VmError;
pub use host::{Host, HostCtx, HostError};
pub use program::{
    LoadError, LoadErrorKind, Location, MAX_CONST_DEPTH, MAX_INHERITANCE_DEPTH, Program,
};
pub use value::{Obj, Value};
pub use vm::{Limits, Vm};

/// Compiles and runs the `rust` code blocks in `README.md` and `docs/API.md`
/// as part of `cargo test`, so the published examples cannot drift from the
/// API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
#[doc = include_str!("../docs/API.md")]
pub struct MarkdownDocTests;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}

    #[test]
    fn test_programs_are_shareable_and_vms_movable() {
        assert_send_sync::<Program>();
        assert_send_sync::<Host>();
        assert_send_sync::<Value>();
        assert_send_sync::<VmError>();
        assert_send_sync::<LoadError>();
        assert_send::<Vm<'static>>();
    }
}
