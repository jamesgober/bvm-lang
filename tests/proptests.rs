//! Property-based tests over the public API.
//!
//! These assert invariants across a wide input space that example-based tests
//! only sample: that integer arithmetic matches Rust's own checked semantics
//! exactly, that comparisons agree with native ordering, and — the security
//! property that matters most for a VM — that *arbitrary* straight-line bytecode
//! is executed to a `Result` and never panics.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bvm_lang::{Chunk, Op, Value, Vm, VmError};
use proptest::prelude::*;

/// Build `LoadInt r0=x; LoadInt r1=y; <op> r0=r0,r1; Return r0` and run it.
fn run_binary(x: i32, y: i32, op: fn(u16, u16, u16) -> Op) -> Result<Value, VmError> {
    let mut chunk = Chunk::new();
    let _ = chunk.emit(Op::LoadInt { dst: 0, val: x });
    let _ = chunk.emit(Op::LoadInt { dst: 1, val: y });
    let _ = chunk.emit(op(0, 0, 1));
    let _ = chunk.emit(Op::Return { src: 0 });
    Vm::new().run(&chunk)
}

proptest! {
    #[test]
    fn prop_add_matches_checked(x: i32, y: i32) {
        let got = run_binary(x, y, |d, l, r| Op::Add { dst: d, lhs: l, rhs: r });
        match x.checked_add(y) {
            Some(sum) => prop_assert_eq!(got.unwrap().as_int(), Some(sum)),
            None => prop_assert_eq!(got, Err(VmError::IntegerOverflow)),
        }
    }

    #[test]
    fn prop_sub_matches_checked(x: i32, y: i32) {
        let got = run_binary(x, y, |d, l, r| Op::Sub { dst: d, lhs: l, rhs: r });
        match x.checked_sub(y) {
            Some(v) => prop_assert_eq!(got.unwrap().as_int(), Some(v)),
            None => prop_assert_eq!(got, Err(VmError::IntegerOverflow)),
        }
    }

    #[test]
    fn prop_mul_matches_checked(x: i32, y: i32) {
        let got = run_binary(x, y, |d, l, r| Op::Mul { dst: d, lhs: l, rhs: r });
        match x.checked_mul(y) {
            Some(v) => prop_assert_eq!(got.unwrap().as_int(), Some(v)),
            None => prop_assert_eq!(got, Err(VmError::IntegerOverflow)),
        }
    }

    #[test]
    fn prop_div_matches_checked(x: i32, y: i32) {
        let got = run_binary(x, y, |d, l, r| Op::Div { dst: d, lhs: l, rhs: r });
        if y == 0 {
            prop_assert_eq!(got, Err(VmError::DivideByZero));
        } else {
            match x.checked_div(y) {
                Some(v) => prop_assert_eq!(got.unwrap().as_int(), Some(v)),
                None => prop_assert_eq!(got, Err(VmError::IntegerOverflow)),
            }
        }
    }

    #[test]
    fn prop_lt_matches_native_ordering(x: i32, y: i32) {
        let got = run_binary(x, y, |d, l, r| Op::Lt { dst: d, lhs: l, rhs: r });
        prop_assert_eq!(got.unwrap().as_bool(), Some(x < y));
    }

    #[test]
    fn prop_eq_matches_native(x: i32, y: i32) {
        let got = run_binary(x, y, |d, l, r| Op::Eq { dst: d, lhs: l, rhs: r });
        prop_assert_eq!(got.unwrap().as_bool(), Some(x == y));
    }
}

/// The number of distinct registers a generated straight-line program may name.
const REGS: u16 = 6;

/// A strategy for a single control-flow-free instruction over registers `0..REGS`.
///
/// No branches are generated, so any sequence of these terminates when a `Return`
/// is appended — which lets the no-panic property run without risking an infinite
/// loop from a random back-edge.
fn straight_line_op() -> impl Strategy<Value = Op> {
    let reg = 0..REGS;
    prop_oneof![
        (reg.clone(), any::<i32>()).prop_map(|(dst, val)| Op::LoadInt { dst, val }),
        (reg.clone(), any::<bool>()).prop_map(|(dst, val)| Op::LoadBool { dst, val }),
        reg.clone().prop_map(|dst| Op::LoadNil { dst }),
        (reg.clone(), reg.clone()).prop_map(|(dst, src)| Op::Move { dst, src }),
        (reg.clone(), reg.clone(), reg.clone()).prop_map(|(dst, lhs, rhs)| Op::Add {
            dst,
            lhs,
            rhs
        }),
        (reg.clone(), reg.clone(), reg.clone()).prop_map(|(dst, lhs, rhs)| Op::Sub {
            dst,
            lhs,
            rhs
        }),
        (reg.clone(), reg.clone(), reg.clone()).prop_map(|(dst, lhs, rhs)| Op::Mul {
            dst,
            lhs,
            rhs
        }),
        (reg.clone(), reg.clone(), reg.clone()).prop_map(|(dst, lhs, rhs)| Op::Div {
            dst,
            lhs,
            rhs
        }),
        (reg.clone(), reg.clone(), reg.clone()).prop_map(|(dst, lhs, rhs)| Op::Lt {
            dst,
            lhs,
            rhs
        }),
        (reg.clone(), reg.clone()).prop_map(|(dst, src)| Op::Neg { dst, src }),
    ]
}

proptest! {
    /// Executing arbitrary straight-line bytecode always yields a `Result` and
    /// never panics, however the operands and register references fall out.
    #[test]
    fn prop_arbitrary_program_never_panics(
        mut ops in proptest::collection::vec(straight_line_op(), 0..64)
    ) {
        ops.push(Op::Return { src: 0 });
        let mut chunk = Chunk::new();
        for op in ops {
            let _ = chunk.emit(op);
        }
        // The assertion is simply that this returns rather than panicking or
        // looping; both the Ok and Err arms are acceptable outcomes.
        let _ = Vm::new().run(&chunk);
    }
}
