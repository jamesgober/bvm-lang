//! Criterion benchmarks for the interpreter loop.
//!
//! Each benchmark assembles a chunk once and then measures repeated execution
//! through a single reused [`Vm`], which is the steady-state pattern: the
//! register file is allocated on the first run and reused thereafter, so these
//! numbers reflect dispatch and arithmetic cost, not allocation.
//!
//! Run with `cargo bench`.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use vm_lang::{Chunk, Op, Vm};

/// A straight-line arithmetic expression: `((7 * 6) + 100 - 8) / 2`.
fn expression_chunk() -> Chunk {
    let mut c = Chunk::new();
    c.emit(Op::LoadInt { dst: 0, val: 7 });
    c.emit(Op::LoadInt { dst: 1, val: 6 });
    c.emit(Op::Mul {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });
    c.emit(Op::LoadInt { dst: 1, val: 100 });
    c.emit(Op::Add {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });
    c.emit(Op::LoadInt { dst: 1, val: 8 });
    c.emit(Op::Sub {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });
    c.emit(Op::LoadInt { dst: 1, val: 2 });
    c.emit(Op::Div {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });
    c.emit(Op::Return { src: 0 });
    c
}

/// A counted loop summing `1..=limit`, dominated by branch dispatch.
fn loop_sum_chunk(limit: i32) -> Chunk {
    let mut c = Chunk::new();
    c.emit(Op::LoadInt { dst: 0, val: 0 }); // sum
    c.emit(Op::LoadInt { dst: 1, val: 1 }); // i
    c.emit(Op::LoadInt { dst: 2, val: limit });
    c.emit(Op::LoadInt { dst: 3, val: 1 }); // step
    let top = c.emit(Op::Le {
        dst: 4,
        lhs: 1,
        rhs: 2,
    });
    let exit_branch = c.emit(Op::JumpIfFalse { cond: 4, target: 0 });
    c.emit(Op::Add {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });
    c.emit(Op::Add {
        dst: 1,
        lhs: 1,
        rhs: 3,
    });
    c.emit(Op::Jump { target: top as u32 });
    let exit = c.emit(Op::Return { src: 0 });
    let _ = c.patch(
        exit_branch,
        Op::JumpIfFalse {
            cond: 4,
            target: exit as u32,
        },
    );
    c
}

fn bench_vm(criterion: &mut Criterion) {
    let mut vm = Vm::new();

    let expr = expression_chunk();
    let _ = criterion.bench_function("expression", |b| {
        b.iter(|| vm.run(black_box(&expr)));
    });

    let loop_1k = loop_sum_chunk(1_000);
    let _ = criterion.bench_function("loop_sum/1000", |b| {
        b.iter(|| vm.run(black_box(&loop_1k)));
    });

    let loop_100k = loop_sum_chunk(100_000);
    let _ = criterion.bench_function("loop_sum/100000", |b| {
        b.iter(|| vm.run(black_box(&loop_100k)));
    });
}

criterion_group!(benches, bench_vm);
criterion_main!(benches);
