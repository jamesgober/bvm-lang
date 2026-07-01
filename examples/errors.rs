//! How runtime and structural faults surface.
//!
//! Every way a run can go wrong — a division by zero, a checked-overflow, a type
//! mismatch, a program that never terminates — comes back as a typed
//! [`VmError`](vm_lang::VmError) from [`Vm::run`], never as a panic. Bytecode is
//! treated as untrusted input.
//!
//! Run with `cargo run --example errors`.

use vm_lang::{Chunk, Op, Value, Vm, VmError};

/// Assemble a two-operand integer program: `<op> over (x, y)`, then return.
fn binary(x: i32, y: i32, op: Op) -> Chunk {
    let mut chunk = Chunk::new();
    chunk.emit(Op::LoadInt { dst: 0, val: x });
    chunk.emit(Op::LoadInt { dst: 1, val: y });
    chunk.emit(op);
    chunk.emit(Op::Return { src: 0 });
    chunk
}

fn report(label: &str, result: Result<Value, VmError>) {
    match result {
        Ok(v) => println!("{label:<18} => ok: {v:?}"),
        Err(e) => println!("{label:<18} => error: {e}"),
    }
}

fn main() {
    let mut vm = Vm::new();

    report(
        "divide by zero",
        vm.run(&binary(
            1,
            0,
            Op::Div {
                dst: 0,
                lhs: 0,
                rhs: 1,
            },
        )),
    );
    report(
        "integer overflow",
        vm.run(&binary(
            i32::MAX,
            1,
            Op::Add {
                dst: 0,
                lhs: 0,
                rhs: 1,
            },
        )),
    );

    // Type mismatch: add a boolean to an integer.
    let mut mismatch = Chunk::new();
    mismatch.emit(Op::LoadBool { dst: 0, val: true });
    mismatch.emit(Op::LoadInt { dst: 1, val: 1 });
    mismatch.emit(Op::Add {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });
    mismatch.emit(Op::Return { src: 0 });
    report("type mismatch", vm.run(&mismatch));

    // No terminator: the code runs off the end.
    let mut runaway = Chunk::new();
    runaway.emit(Op::LoadInt { dst: 0, val: 1 });
    report("no terminator", vm.run(&runaway));
}
