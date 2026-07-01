//! Compute a Fibonacci number with a counted loop.
//!
//! Demonstrates control flow: a comparison drives a `JumpIfFalse` that exits the
//! loop, and an unconditional `Jump` forms the back-edge. The forward exit branch
//! is emitted with a placeholder target and back-patched once the landing address
//! is known — the pattern a real code generator uses for every forward jump.
//!
//! Run with `cargo run --example fibonacci`.

use bvm_lang::{Chunk, Op, Vm};

/// Assemble a chunk that returns the `n`th Fibonacci number (0-indexed).
fn fibonacci(n: i32) -> Chunk {
    let mut chunk = Chunk::new();

    // r0 = a (prev), r1 = b (curr), r2 = i (counter), r3 = n, r4 = one, r5 = tmp/cond
    chunk.emit(Op::LoadInt { dst: 0, val: 0 });
    chunk.emit(Op::LoadInt { dst: 1, val: 1 });
    chunk.emit(Op::LoadInt { dst: 2, val: 0 });
    chunk.emit(Op::LoadInt { dst: 3, val: n });
    chunk.emit(Op::LoadInt { dst: 4, val: 1 });

    // loop: while i < n { tmp = a + b; a = b; b = tmp; i += 1 }
    let loop_top = chunk.emit(Op::Lt {
        dst: 5,
        lhs: 2,
        rhs: 3,
    });
    let exit_branch = chunk.emit(Op::JumpIfFalse { cond: 5, target: 0 });
    chunk.emit(Op::Add {
        dst: 5,
        lhs: 0,
        rhs: 1,
    }); // tmp = a + b
    chunk.emit(Op::Move { dst: 0, src: 1 }); // a = b
    chunk.emit(Op::Move { dst: 1, src: 5 }); // b = tmp
    chunk.emit(Op::Add {
        dst: 2,
        lhs: 2,
        rhs: 4,
    }); // i += 1
    chunk.emit(Op::Jump { target: loop_top });

    let exit = chunk.emit(Op::Return { src: 0 });
    // Now that the landing address is known, fix up the forward exit branch.
    let patched = chunk.patch(
        exit_branch,
        Op::JumpIfFalse {
            cond: 5,
            target: exit,
        },
    );
    assert!(patched, "exit branch address is valid");

    chunk
}

fn main() {
    let mut vm = Vm::new();
    for n in 0..=10 {
        let chunk = fibonacci(n);
        match vm.run(&chunk) {
            Ok(value) => println!("fib({n:>2}) = {:?}", value.as_int().unwrap_or_default()),
            Err(err) => eprintln!("fib({n}) failed: {err}"),
        }
    }
}
