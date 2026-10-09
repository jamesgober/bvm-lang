//! Budgets (ISSUES H09): fuel bounds every loop and recursion, the memory
//! budget bounds the heap, the depth and stack limits bound frames; every
//! exhaustion is a deterministic error with the function and pc, never a
//! hang, a crash, or an allocation failure of the host.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

mod common;

use bvm_lang::{Host, Limits, Program, Value, Vm, VmError};
use bytecode_lang::{
    Const, ErrorKind, FuncId, Inst, IntOp, IntTy, ModuleBuilder, Reg, Target, TypeDef, ValType,
};
use common::{D, I64};

#[test]
fn an_infinite_loop_without_a_safepoint_runs_out_of_fuel() {
    // H09's reproducer: `jmp 0` with no safepoint at all.
    let out = common::eval_with(&[], &[], &[], Limits::new().with_fuel(10_000), |_, f| {
        f.emit(Inst::Nop {});
        f.emit(Inst::Jmp { target: Target(0) });
    });
    assert_eq!(
        out,
        Err(VmError::Trap {
            kind: ErrorKind::OutOfFuel,
            func: FuncId(0),
            pc: 1
        })
    );
}

#[test]
fn infinite_recursion_hits_the_depth_limit_or_fuel() {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[], &[]);
    let me = f.id();
    let w = f.reg(D);
    f.emit(Inst::Call {
        dst: w,
        func: me,
        argc: 0,
    });
    f.ret_void();
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::with_limits(&p, Limits::new().with_depth(1_000));
    let err = vm.run(FuncId(0), &[]).unwrap_err();
    assert_eq!(
        err,
        VmError::Raised {
            kind: ErrorKind::StackOverflow,
            func: FuncId(0),
            pc: 0
        }
    );
    // With less fuel than depth, fuel runs out first.
    let err = vm
        .run_with(FuncId(0), &[], Limits::new().with_fuel(100))
        .unwrap_err();
    assert_eq!(
        err,
        VmError::Trap {
            kind: ErrorKind::OutOfFuel,
            func: FuncId(0),
            pc: 0
        }
    );
}

#[test]
fn infinite_tail_recursion_runs_out_of_fuel() {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[], &[]);
    let me = f.id();
    // Even an unused window register must exist (LSB V-F2).
    let w = f.reg(D);
    f.emit(Inst::TailCall {
        func: me,
        args: w,
        argc: 0,
    });
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let out = Vm::new(&p).run_with(FuncId(0), &[], Limits::new().with_fuel(5_000));
    assert_eq!(
        out,
        Err(VmError::Trap {
            kind: ErrorKind::OutOfFuel,
            func: FuncId(0),
            pc: 0
        })
    );
}

#[test]
fn the_stack_slot_limit_bounds_wide_frames() {
    // A function with 60,000 registers recursing: the register stack, not
    // the depth, is what runs out.
    let mut m = ModuleBuilder::new();
    let mut f = m.function("wide", &[], &[]);
    let me = f.id();
    for _ in 0..60_000 {
        let _ = f.reg(I64);
    }
    f.emit(Inst::Call {
        dst: Reg(0),
        func: me,
        argc: 0,
    });
    f.ret_void();
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::with_limits(&p, Limits::new().with_stack(1_000_000));
    let err = vm.run(FuncId(0), &[]).unwrap_err();
    assert_eq!(
        err,
        VmError::Raised {
            kind: ErrorKind::StackOverflow,
            func: FuncId(0),
            pc: 0
        }
    );
}

#[test]
fn unbounded_allocation_traps_at_the_memory_budget() {
    // Push onto an array forever (with a safepoint): OutOfMemory, not a host
    // allocation failure.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(I64));
    let mut f = m.function("f", &[], &[]);
    let tr = f.type_ref(at);
    let (a, n) = (f.reg(ValType::Ref(at)), f.reg(I64));
    f.emit(Inst::NewArray {
        dst: a,
        len: n,
        ty: tr,
    });
    f.emit(Inst::ArrayPush { arr: a, src: n });
    f.emit(Inst::Safepoint {});
    f.emit(Inst::Jmp { target: Target(1) });
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let limits = Limits::new().with_memory(4 << 20);
    let err = Vm::with_limits(&p, limits).run(FuncId(0), &[]).unwrap_err();
    assert!(
        matches!(
            err,
            VmError::Trap {
                kind: ErrorKind::OutOfMemory,
                pc: 1,
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn a_hostile_array_length_is_a_trap_not_an_abort() {
    let out = common::eval_with(
        &[I64],
        &[],
        &[Value::Int(i64::MAX / 2)],
        Limits::new().with_memory(1 << 20),
        |m, f| {
            let at = m.add_type(TypeDef::Array(I64));
            let tr = f.type_ref(at);
            let a = f.reg(ValType::Ref(at));
            f.emit(Inst::NewArray {
                dst: a,
                len: Reg(0),
                ty: tr,
            });
            f.ret_void();
        },
    );
    assert_eq!(
        out,
        Err(VmError::Trap {
            kind: ErrorKind::OutOfMemory,
            func: FuncId(0),
            pc: 0
        })
    );
}

#[test]
fn string_doubling_traps_at_the_memory_budget() {
    let out = common::eval_with(&[], &[], &[], Limits::new().with_memory(1 << 20), |m, f| {
        let k = m.constant(Const::Bytes(b"xx".to_vec()));
        let s = f.reg(ValType::Str);
        f.emit(Inst::LoadConst { dst: s, k });
        f.emit(Inst::StrConcat {
            dst: s,
            lhs: s,
            rhs: s,
        });
        f.emit(Inst::Jmp { target: Target(1) });
    });
    assert!(
        matches!(
            out,
            Err(VmError::Trap {
                kind: ErrorKind::OutOfMemory,
                pc: 1,
                ..
            })
        ),
        "{out:?}"
    );
}

#[test]
fn garbage_does_not_count_against_the_budget_once_collected() {
    // Allocate 1 MiB of short-lived strings per iteration, 200 times, under
    // a 4 MiB budget: collection keeps the run alive.
    let out = common::eval_with(
        &[],
        &[I64],
        &[],
        Limits::new().with_memory(4 << 20),
        |m, f| {
            let k = m.constant(Const::Bytes(vec![b'z'; 1 << 10]));
            let (s, t, i, one, lim, c) = (
                f.reg(ValType::Str),
                f.reg(ValType::Str),
                f.reg(I64),
                f.reg(I64),
                f.reg(I64),
                f.reg(ValType::Bool),
            );
            let op = IntOp::new(IntTy::I64);
            f.emit(Inst::LoadConst { dst: s, k });
            f.emit(Inst::LoadInt {
                dst: one,
                val: 1,
                ty: IntTy::I64,
            });
            f.emit(Inst::LoadInt {
                dst: lim,
                val: 200 * 1024,
                ty: IntTy::I64,
            });
            let (top, done) = (f.label(), f.label());
            f.bind(top);
            f.emit(Inst::ILt {
                dst: c,
                lhs: i,
                rhs: lim,
                ty: IntTy::I64,
            });
            f.jmp_if_not(c, done);
            f.emit(Inst::StrConcat {
                dst: t,
                lhs: s,
                rhs: s,
            }); // 2 KiB of garbage
            f.emit(Inst::IAdd {
                dst: i,
                lhs: i,
                rhs: one,
                op,
            });
            f.emit(Inst::Safepoint {});
            f.jmp(top);
            f.bind(done);
            f.ret(i);
        },
    );
    assert_eq!(out, Ok(Value::Int(200 * 1024)));
}

#[test]
fn fuel_is_per_run_and_reported() {
    let p = common::load(common::module(&[], &[], |_, f| {
        for _ in 0..7 {
            f.emit(Inst::Safepoint {});
        }
        f.ret_void();
    }));
    let mut vm = Vm::with_limits(&p, Limits::new().with_fuel(7));
    assert_eq!(vm.run(FuncId(0), &[]), Ok(Value::Nil));
    assert_eq!(vm.fuel_used(), 7);
    // Fuel restarts at the budget each run.
    assert_eq!(vm.run(FuncId(0), &[]), Ok(Value::Nil));
    assert!(
        vm.run_with(FuncId(0), &[], Limits::new().with_fuel(6))
            .is_err()
    );
}

#[test]
fn deep_hook_recursion_is_bounded_by_depth() {
    // An `add` hook that does `dadd` on its operands again: each hook call
    // pushes a frame, so the depth limit stops it.
    let mut m = ModuleBuilder::new();
    let mut h = m.function("add", &[D, D], &[D]);
    let r = h.reg(D);
    h.emit(Inst::DAdd {
        dst: r,
        lhs: Reg(0),
        rhs: Reg(1),
        pol: bytecode_lang::Policy::new(),
    });
    h.ret(r);
    let hid = h.id();
    m.add_function(h).unwrap();
    m.hook(bytecode_lang::Hook::Add, bytecode_lang::Callee::Func(hid));
    let mut main = m.function("main", &[], &[D]);
    let (a, b) = (main.reg(D), main.reg(D));
    main.emit(Inst::DAdd {
        dst: a,
        lhs: a,
        rhs: b,
        pol: bytecode_lang::Policy::new(),
    });
    main.ret(a);
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let err = Vm::with_limits(&p, Limits::new().with_depth(500))
        .run(id, &[])
        .unwrap_err();
    assert!(
        matches!(
            err,
            VmError::Raised {
                kind: ErrorKind::StackOverflow,
                ..
            }
        ),
        "{err:?}"
    );
}
