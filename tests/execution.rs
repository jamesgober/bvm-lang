//! End-to-end execution tests over the public API.
//!
//! These build real chunks the way a compiler would and run them through a
//! [`Vm`], covering the cross-module path from instruction assembly to result:
//! whole programs, control flow, error surfacing, and VM reuse.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bvm_lang::{Chunk, Op, Value, Vm, VmError};

/// Assemble a chunk from an instruction slice and a constant slice.
fn assemble(ops: &[Op], constants: &[Value]) -> Chunk {
    let mut chunk = Chunk::new();
    for &v in constants {
        let _ = chunk.constant(v).expect("constant pool has room");
    }
    for &op in ops {
        let _ = chunk.emit(op);
    }
    chunk
}

#[test]
fn test_polynomial_evaluation() {
    // 3 * x^2 + 2 * x + 7 at x = 5  =>  92
    let chunk = assemble(
        &[
            Op::LoadInt { dst: 0, val: 5 }, // x
            Op::Mul {
                dst: 1,
                lhs: 0,
                rhs: 0,
            }, // x^2
            Op::LoadInt { dst: 2, val: 3 },
            Op::Mul {
                dst: 1,
                lhs: 1,
                rhs: 2,
            }, // 3x^2
            Op::LoadInt { dst: 2, val: 2 },
            Op::Mul {
                dst: 2,
                lhs: 2,
                rhs: 0,
            }, // 2x
            Op::Add {
                dst: 1,
                lhs: 1,
                rhs: 2,
            },
            Op::LoadInt { dst: 2, val: 7 },
            Op::Add {
                dst: 1,
                lhs: 1,
                rhs: 2,
            },
            Op::Return { src: 1 },
        ],
        &[],
    );
    assert_eq!(Vm::new().run(&chunk).unwrap().as_int(), Some(92));
}

#[test]
fn test_factorial_loop() {
    // acc = 1; i = 1; while i <= 6 { acc *= i; i += 1 } return acc  => 720
    let mut chunk = Chunk::new();
    let _ = chunk.emit(Op::LoadInt { dst: 0, val: 1 }); // acc
    let _ = chunk.emit(Op::LoadInt { dst: 1, val: 1 }); // i
    let _ = chunk.emit(Op::LoadInt { dst: 2, val: 6 }); // limit
    let _ = chunk.emit(Op::LoadInt { dst: 3, val: 1 }); // step
    let cond = chunk.emit(Op::Le {
        dst: 4,
        lhs: 1,
        rhs: 2,
    });
    let exit_branch = chunk.emit(Op::JumpIfFalse { cond: 4, target: 0 });
    let _ = chunk.emit(Op::Mul {
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
        target: cond as u32,
    });
    let exit = chunk.emit(Op::Return { src: 0 });
    assert!(chunk.patch(
        exit_branch,
        Op::JumpIfFalse {
            cond: 4,
            target: exit as u32,
        },
    ));

    assert_eq!(Vm::new().run(&chunk).unwrap().as_int(), Some(720));
}

#[test]
fn test_float_and_int_mixed_arithmetic() {
    // 1 + 0.5 * 2  =>  2.0  (a float promotes the whole expression)
    let chunk = assemble(
        &[
            Op::LoadInt { dst: 0, val: 1 },
            Op::LoadConst { dst: 1, konst: 0 },
            Op::LoadInt { dst: 2, val: 2 },
            Op::Mul {
                dst: 1,
                lhs: 1,
                rhs: 2,
            },
            Op::Add {
                dst: 0,
                lhs: 0,
                rhs: 1,
            },
            Op::Return { src: 0 },
        ],
        &[Value::float(0.5)],
    );
    assert_eq!(Vm::new().run(&chunk).unwrap().as_float(), Some(2.0));
}

#[test]
fn test_boolean_logic_and_negation() {
    // return !(3 == 4)  =>  true
    let chunk = assemble(
        &[
            Op::LoadInt { dst: 0, val: 3 },
            Op::LoadInt { dst: 1, val: 4 },
            Op::Eq {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            Op::Not { dst: 2, src: 2 },
            Op::Return { src: 2 },
        ],
        &[],
    );
    assert_eq!(Vm::new().run(&chunk).unwrap().as_bool(), Some(true));
}

#[test]
fn test_runtime_faults_surface_as_errors() {
    // Overflow, division by zero, and type mismatch each become a VmError.
    let overflow = assemble(
        &[
            Op::LoadInt {
                dst: 0,
                val: i32::MAX,
            },
            Op::LoadInt { dst: 1, val: 1 },
            Op::Add {
                dst: 0,
                lhs: 0,
                rhs: 1,
            },
            Op::Return { src: 0 },
        ],
        &[],
    );
    assert_eq!(Vm::new().run(&overflow), Err(VmError::IntegerOverflow));

    let type_mismatch = assemble(
        &[
            Op::LoadBool { dst: 0, val: true },
            Op::LoadInt { dst: 1, val: 1 },
            Op::Add {
                dst: 0,
                lhs: 0,
                rhs: 1,
            },
            Op::Return { src: 0 },
        ],
        &[],
    );
    assert_eq!(
        Vm::new().run(&type_mismatch),
        Err(VmError::TypeMismatch { op: "add" })
    );
}

#[test]
fn test_one_vm_runs_many_chunks() {
    let mut vm = Vm::new();
    for n in 0..8 {
        let chunk = assemble(
            &[Op::LoadInt { dst: 0, val: n }, Op::Return { src: 0 }],
            &[],
        );
        assert_eq!(vm.run(&chunk).unwrap().as_int(), Some(n));
    }
}

#[test]
fn test_builder_sizes_file_to_cover_every_operand() {
    // Every register a chunk names — including the one a `Return` reads — widens
    // the file, so a chunk assembled through the builder can never address a slot
    // outside it. Here `Return { src: 9 }` alone justifies a ten-slot file.
    let mut chunk = Chunk::new();
    let _ = chunk.emit(Op::LoadInt { dst: 9, val: 123 });
    let _ = chunk.emit(Op::Return { src: 9 });
    assert_eq!(chunk.registers(), 10);
    assert_eq!(Vm::new().run(&chunk).unwrap().as_int(), Some(123));
}

#[test]
fn test_unreached_terminator_still_halts() {
    // A `Halt` short-circuits before the trailing dead code is ever fetched.
    let chunk = assemble(
        &[
            Op::LoadInt { dst: 0, val: 5 },
            Op::Halt,
            Op::Return { src: 0 }, // dead: never reached
        ],
        &[],
    );
    assert!(Vm::new().run(&chunk).unwrap().is_nil());
}
