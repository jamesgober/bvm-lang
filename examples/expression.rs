//! Compile and evaluate a fixed arithmetic expression.
//!
//! Shows the shortest end-to-end path: assemble a [`Chunk`] of instructions,
//! hand it to a [`Vm`], and read the result. The expression is
//! `(2 + 3) * 4 - 10 / 2`, which evaluates to `15`.
//!
//! Run with `cargo run --example expression`.

use bvm_lang::{Chunk, Op, Vm};

fn main() {
    let mut chunk = Chunk::new();

    // r0 = 2 + 3
    chunk.emit(Op::LoadInt { dst: 0, val: 2 });
    chunk.emit(Op::LoadInt { dst: 1, val: 3 });
    chunk.emit(Op::Add {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });

    // r0 = r0 * 4
    chunk.emit(Op::LoadInt { dst: 1, val: 4 });
    chunk.emit(Op::Mul {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });

    // r1 = 10 / 2
    chunk.emit(Op::LoadInt { dst: 1, val: 10 });
    chunk.emit(Op::LoadInt { dst: 2, val: 2 });
    chunk.emit(Op::Div {
        dst: 1,
        lhs: 1,
        rhs: 2,
    });

    // r0 = r0 - r1
    chunk.emit(Op::Sub {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });
    chunk.emit(Op::Return { src: 0 });

    let mut vm = Vm::new();
    match vm.run(&chunk) {
        Ok(value) => println!("(2 + 3) * 4 - 10 / 2 = {:?}", value.as_int()),
        Err(err) => eprintln!("execution failed: {err}"),
    }
}
