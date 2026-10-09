//! LSB conformance, control flow and calls (LSB §5.9, §4.2) and the
//! coroutine group's alpha.1 behaviour (§5.13).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

mod common;

use bvm_lang::{Host, HostError, Limits, Program, Value, Vm, VmError};
use bytecode_lang::{
    ErrorKind, FuncId, Inst, IntOp, IntTy, ModuleBuilder, Opcode, Reg, Target, ValType,
};
use common::{BOOL, D, I64};

fn op() -> IntOp {
    IntOp::new(IntTy::I64)
}

#[test]
fn op_jmp_jmp_if_jmp_if_not() {
    // if x { 1 } else { 2 }, through both conditional forms.
    for (x, expect) in [(true, 1), (false, 2)] {
        let out = common::eval(&[BOOL], &[I64], &[Value::Bool(x)], |_, f| {
            let r = f.reg(I64);
            let (els, end) = (f.label(), f.label());
            f.jmp_if_not(Reg(0), els);
            f.emit(Inst::LoadInt {
                dst: r,
                val: 1,
                ty: IntTy::I64,
            });
            f.jmp(end);
            f.bind(els);
            f.emit(Inst::LoadInt {
                dst: r,
                val: 2,
                ty: IntTy::I64,
            });
            f.bind(end);
            f.ret(r);
        });
        assert_eq!(out, Ok(Value::Int(expect)));
        let out = common::eval(&[BOOL], &[I64], &[Value::Bool(x)], |_, f| {
            let r = f.reg(I64);
            let then = f.label();
            f.jmp_if(Reg(0), then);
            f.emit(Inst::LoadInt {
                dst: r,
                val: 2,
                ty: IntTy::I64,
            });
            f.ret(r);
            f.bind(then);
            f.emit(Inst::LoadInt {
                dst: r,
                val: 1,
                ty: IntTy::I64,
            });
            f.ret(r);
        });
        assert_eq!(out, Ok(Value::Int(expect)));
    }
}

#[test]
fn op_switch_in_range_out_of_range_and_negative() {
    let run = |v: i64| {
        common::eval(&[I64], &[I64], &[Value::Int(v)], |_, f| {
            let r = f.reg(I64);
            let (a, b, d) = (f.label(), f.label(), f.label());
            f.switch(IntTy::I64, Reg(0), &[a, b], d);
            f.bind(a);
            f.emit(Inst::LoadInt {
                dst: r,
                val: 10,
                ty: IntTy::I64,
            });
            f.ret(r);
            f.bind(b);
            f.emit(Inst::LoadInt {
                dst: r,
                val: 20,
                ty: IntTy::I64,
            });
            f.ret(r);
            f.bind(d);
            f.emit(Inst::LoadInt {
                dst: r,
                val: 99,
                ty: IntTy::I64,
            });
            f.ret(r);
        })
    };
    assert_eq!(run(0), Ok(Value::Int(10)));
    assert_eq!(run(1), Ok(Value::Int(20)));
    assert_eq!(run(2), Ok(Value::Int(99)));
    assert_eq!(run(-1), Ok(Value::Int(99)));
    assert_eq!(run(i64::MIN), Ok(Value::Int(99)));
}

/// `fib(n)` by double recursion through `call`.
fn fib_program() -> (Program, FuncId) {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("fib", &[I64], &[I64]);
    let me = f.id();
    let (two, cond, a, b) = (f.reg(I64), f.reg(BOOL), f.reg(I64), f.reg(I64));
    let win = f.regs(&[I64, I64]);
    let small = f.label();
    f.emit(Inst::LoadInt {
        dst: two,
        val: 2,
        ty: IntTy::I64,
    });
    f.emit(Inst::ILt {
        dst: cond,
        lhs: Reg(0),
        rhs: two,
        ty: IntTy::I64,
    });
    f.jmp_if(cond, small);
    f.emit(Inst::LoadInt {
        dst: a,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::ISub {
        dst: Reg(win.0 + 1),
        lhs: Reg(0),
        rhs: a,
        op: op(),
    });
    f.emit(Inst::Call {
        dst: win,
        func: me,
        argc: 1,
    });
    f.emit(Inst::Mov { dst: a, src: win });
    f.emit(Inst::ISub {
        dst: Reg(win.0 + 1),
        lhs: Reg(0),
        rhs: two,
        op: op(),
    });
    f.emit(Inst::Call {
        dst: win,
        func: me,
        argc: 1,
    });
    f.emit(Inst::IAdd {
        dst: b,
        lhs: a,
        rhs: win,
        op: op(),
    });
    f.ret(b);
    f.bind(small);
    f.ret(Reg(0));
    let id = m.add_function(f).unwrap();
    (
        Program::load(m.finish().unwrap(), &Host::new()).unwrap(),
        id,
    )
}

#[test]
fn op_call_and_ret_recursive_fib() {
    let (p, fib) = fib_program();
    let mut vm = Vm::new(&p);
    assert_eq!(vm.run(fib, &[Value::Int(20)]), Ok(Value::Int(6765)));
    assert_eq!(vm.run(fib, &[Value::Int(0)]), Ok(Value::Int(0)));
}

#[test]
fn call_depth_limit_raises_catchable_stack_overflow() {
    let (p, fib) = fib_program();
    let mut vm = Vm::with_limits(&p, Limits::new().with_depth(10));
    let err = vm.run(fib, &[Value::Int(30)]).unwrap_err();
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

#[test]
fn op_ret_void_leaves_call_destination_untouched() {
    let mut m = ModuleBuilder::new();
    let mut v = m.function("v", &[], &[]);
    v.ret_void();
    let vid = v.id();
    let mut main = m.function("main", &[], &[I64]);
    let win = main.regs(&[I64]);
    main.emit(Inst::LoadInt {
        dst: win,
        val: 5,
        ty: IntTy::I64,
    });
    main.emit(Inst::Call {
        dst: win,
        func: vid,
        argc: 0,
    });
    main.ret(win);
    m.add_function(v).unwrap();
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Int(5)));
}

#[test]
fn op_tail_call_runs_in_constant_depth() {
    // count(n, acc) = n == 0 ? acc : count(n - 1, acc + 1), a million deep
    // with a depth limit of 4.
    let mut m = ModuleBuilder::new();
    let mut f = m.function("count", &[I64, I64], &[I64]);
    let me = f.id();
    let (zero, one, cond) = (f.reg(I64), f.reg(I64), f.reg(BOOL));
    let args = f.regs(&[I64, I64]);
    let done = f.label();
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::IEq {
        dst: cond,
        lhs: Reg(0),
        rhs: zero,
        ty: IntTy::I64,
    });
    f.jmp_if(cond, done);
    f.emit(Inst::ISub {
        dst: args,
        lhs: Reg(0),
        rhs: one,
        op: op(),
    });
    f.emit(Inst::IAdd {
        dst: Reg(args.0 + 1),
        lhs: Reg(1),
        rhs: one,
        op: op(),
    });
    f.emit(Inst::TailCall {
        func: me,
        args,
        argc: 2,
    });
    f.bind(done);
    f.ret(Reg(1));
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::with_limits(&p, Limits::new().with_depth(4));
    assert_eq!(
        vm.run(id, &[Value::Int(1_000_000), Value::Int(0)]),
        Ok(Value::Int(1_000_000))
    );
    // Each tail call costs one unit of fuel.
    assert_eq!(vm.fuel_used(), 1_000_000);
}

#[test]
fn op_call_indirect_and_tail_call_indirect_through_closures() {
    // make_closure of `add_k` capturing k = 100; call it indirectly; then
    // tail-call it indirectly from a second function.
    let mut m = ModuleBuilder::new();
    let mut add_k = m.function("add_k", &[I64], &[I64]);
    let k = add_k.capture(I64);
    let (kv, r) = (add_k.reg(I64), add_k.reg(I64));
    add_k.emit(Inst::GetUpval { dst: kv, idx: k });
    add_k.emit(Inst::IAdd {
        dst: r,
        lhs: Reg(0),
        rhs: kv,
        op: op(),
    });
    add_k.ret(r);
    let add_k_id = add_k.id();
    let fn_t = m.func_type(&[I64], &[I64]);
    let mut tail = m.function("tail", &[ValType::Ref(fn_t), I64], &[I64]);
    tail.emit(Inst::TailCallIndirect {
        callee: Reg(0),
        args: Reg(1),
        argc: 1,
    });
    let tail_id = tail.id();
    let mut main = m.function("main", &[I64], &[I64]);
    let clo = main.regs(&[ValType::Ref(fn_t), I64]); // closure, then its capture
    let win = main.regs(&[I64, I64]);
    let win2 = main.regs(&[I64, ValType::Ref(fn_t), I64]);
    main.emit(Inst::LoadInt {
        dst: Reg(clo.0 + 1),
        val: 100,
        ty: IntTy::I64,
    });
    main.emit(Inst::MakeClosure {
        dst: clo,
        func: add_k_id,
    });
    main.emit(Inst::Mov {
        dst: Reg(win.0 + 1),
        src: Reg(0),
    });
    main.emit(Inst::CallIndirect {
        dst: win,
        callee: clo,
        argc: 1,
    }); // x + 100
    main.emit(Inst::Mov {
        dst: Reg(win2.0 + 1),
        src: clo,
    });
    main.emit(Inst::Mov {
        dst: Reg(win2.0 + 2),
        src: win,
    });
    main.emit(Inst::Call {
        dst: win2,
        func: tail_id,
        argc: 2,
    }); // + 100 again
    main.ret(win2);
    m.add_function(add_k).unwrap();
    m.add_function(tail).unwrap();
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[Value::Int(1)]), Ok(Value::Int(201)));
}

#[test]
fn call_indirect_on_nil_is_null_reference() {
    let mut m = ModuleBuilder::new();
    let fn_t = m.func_type(&[], &[]);
    let mut main = m.function("main", &[], &[]);
    let (callee, win) = (main.reg(ValType::Ref(fn_t)), main.reg(D));
    main.emit(Inst::CallIndirect {
        dst: win,
        callee,
        argc: 0,
    });
    main.ret_void();
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(id, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::NullReference,
            func: id,
            pc: 0
        })
    );
}

#[test]
fn op_call_import_and_load_import() {
    let mut host = Host::new();
    host.register("math", "twice", |_, args| match args {
        [Value::Int(i)] => Ok(Value::Int(i * 2)),
        _ => Err(HostError::Raise(ErrorKind::TypeError)),
    });
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[I64], &[I64]);
    let twice = m.import("math", "twice", sig);
    let mut main = m.function("main", &[I64], &[I64]);
    let win = main.regs(&[I64, I64]);
    let fv = main.reg(ValType::Ref(sig));
    let win2 = main.regs(&[I64, I64]);
    main.emit(Inst::Mov {
        dst: Reg(win.0 + 1),
        src: Reg(0),
    });
    main.emit(Inst::CallImport {
        dst: win,
        import: twice,
        argc: 1,
    });
    main.emit(Inst::LoadImport {
        dst: fv,
        import: twice,
    });
    main.emit(Inst::Mov {
        dst: Reg(win2.0 + 1),
        src: win,
    });
    main.emit(Inst::CallIndirect {
        dst: win2,
        callee: fv,
        argc: 1,
    });
    main.ret(win2);
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &host).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[Value::Int(5)]), Ok(Value::Int(20)));
}

#[test]
fn host_result_of_the_wrong_type_is_a_type_error_at_the_call() {
    let mut host = Host::new();
    host.register("env", "bad", |_, _| Ok(Value::Float(1.0)));
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[], &[I64]);
    let bad = m.import("env", "bad", sig);
    let mut main = m.function("main", &[], &[I64]);
    let win = main.regs(&[I64]);
    main.emit(Inst::CallImport {
        dst: win,
        import: bad,
        argc: 0,
    });
    main.ret(win);
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &host).unwrap();
    assert_eq!(
        Vm::new(&p).run(id, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            func: id,
            pc: 0
        })
    );
}

#[test]
fn op_throw_is_uncaught_without_handler_and_err_code() {
    let out = common::eval(&[D], &[], &[Value::Int(42)], |_, f| {
        f.emit(Inst::Throw { src: Reg(0) });
    });
    assert_eq!(
        out,
        Err(VmError::Thrown {
            value: Value::Int(42),
            func: FuncId(0),
            pc: 0
        })
    );
    // err_code of a caught runtime error, and 0 for a thrown non-error.
    let out = common::eval(&[], &[ValType::U32], &[], |_, f| {
        let (zero, q, e, code) = (f.reg(I64), f.reg(I64), f.reg(D), f.reg(ValType::U32));
        let (s, end, h) = (f.label(), f.label(), f.label());
        f.bind(s);
        f.emit(Inst::IDiv {
            dst: q,
            lhs: q,
            rhs: zero,
            op: op(),
        });
        f.bind(end);
        f.emit(Inst::Unreachable {});
        f.bind(h);
        f.emit(Inst::ErrCode { dst: code, src: e });
        f.ret(code);
        f.try_region(s, end, h, e);
    });
    assert_eq!(out, Ok(Value::UInt(2)));
    let out = common::eval(&[D], &[ValType::U32], &[Value::Int(1)], |_, f| {
        let code = f.reg(ValType::U32);
        f.emit(Inst::ErrCode {
            dst: code,
            src: Reg(0),
        });
        f.ret(code);
    });
    assert_eq!(out, Ok(Value::UInt(0)));
}

#[test]
fn op_safepoint_charges_fuel_and_unreachable_traps() {
    let out = common::eval_with(&[], &[], &[], Limits::new().with_fuel(2), |_, f| {
        f.emit(Inst::Safepoint {});
        f.emit(Inst::Safepoint {});
        f.emit(Inst::Safepoint {});
        f.ret_void();
    });
    assert_eq!(
        out,
        Err(VmError::Trap {
            kind: ErrorKind::OutOfFuel,
            func: FuncId(0),
            pc: 2
        })
    );
    let out = common::eval(&[], &[], &[], |_, f| {
        f.emit(Inst::Unreachable {});
    });
    assert_eq!(
        out,
        Err(VmError::Trap {
            kind: ErrorKind::Unreachable,
            func: FuncId(0),
            pc: 0
        })
    );
}

#[test]
fn unreachable_is_not_catchable() {
    let out = common::eval(&[], &[], &[], |_, f| {
        let e = f.reg(D);
        let (s, end, h) = (f.label(), f.label(), f.label());
        f.bind(s);
        f.emit(Inst::Unreachable {});
        f.bind(end);
        f.bind(h);
        f.ret_void();
        f.try_region(s, end, h, e);
    });
    assert_eq!(
        out,
        Err(VmError::Trap {
            kind: ErrorKind::Unreachable,
            func: FuncId(0),
            pc: 0
        })
    );
}

#[test]
fn coroutine_instructions_are_unsupported_with_their_pc() {
    let insts = [
        Inst::CoroNew {
            dst: Reg(0),
            func: FuncId(0),
            argc: 0,
        },
        Inst::CoroNewIndirect {
            dst: Reg(0),
            callee: Reg(0),
            argc: 0,
        },
        Inst::Yield {
            dst: Reg(0),
            src: Reg(0),
        },
        Inst::YieldKv {
            dst: Reg(0),
            key: Reg(0),
            src: Reg(0),
        },
        Inst::Await {
            dst: Reg(0),
            src: Reg(0),
        },
        Inst::Resume {
            dst: Reg(0),
            coro: Reg(0),
            src: Reg(0),
        },
        Inst::ResumeThrow {
            dst: Reg(0),
            coro: Reg(0),
            src: Reg(0),
        },
        Inst::CoroStatus {
            dst: Reg(0),
            coro: Reg(0),
        },
        Inst::CoroCurrent { dst: Reg(0) },
        Inst::Spawn {
            dst: Reg(0),
            callee: Reg(0),
            argc: 0,
        },
        Inst::CoroClose {
            dst: Reg(0),
            coro: Reg(0),
            src: Reg(0),
        },
        Inst::CoroKey {
            dst: Reg(0),
            coro: Reg(0),
        },
        Inst::CoroResult {
            dst: Reg(0),
            coro: Reg(0),
        },
    ];
    for inst in insts {
        let out = common::eval(&[], &[], &[], |_, f| {
            let _ = f.reg(D);
            f.emit(Inst::Nop {});
            f.emit(inst);
            f.ret_void();
        });
        assert_eq!(
            out,
            Err(VmError::Unsupported {
                opcode: inst.opcode(),
                func: FuncId(0),
                pc: 1
            }),
            "{inst}"
        );
        assert!((0xF0..=0xFC).contains(&(inst.opcode() as u8)));
    }
    assert!(matches!(Opcode::from_u8(0xF0), Some(Opcode::CoroNew)));
}

#[test]
fn errors_escaping_a_callee_are_raised_at_the_call() {
    // callee divides by zero; caller's try covers only its call.
    let mut m = ModuleBuilder::new();
    let mut callee = m.function("callee", &[], &[I64]);
    let (a, z) = (callee.reg(I64), callee.reg(I64));
    callee.emit(Inst::IDiv {
        dst: a,
        lhs: a,
        rhs: z,
        op: op(),
    });
    callee.ret(a);
    let cid = callee.id();
    let mut main = m.function("main", &[], &[I64]);
    let (win, e, code) = (main.regs(&[I64]), main.reg(D), main.reg(ValType::U32));
    let (s, end, h) = (main.label(), main.label(), main.label());
    main.emit(Inst::LoadInt {
        dst: win,
        val: -1,
        ty: IntTy::I64,
    });
    main.bind(s);
    main.emit(Inst::Call {
        dst: win,
        func: cid,
        argc: 0,
    });
    main.bind(end);
    main.ret(win);
    main.bind(h);
    // dst untouched by the failed call: still -1.
    main.emit(Inst::ErrCode { dst: code, src: e });
    main.ret(win);
    main.try_region(s, end, h, e);
    m.add_function(callee).unwrap();
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Int(-1)));
    // Uncaught, the error names the callee and its pc.
    let mut vm = Vm::new(&p);
    assert_eq!(
        vm.run(cid, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::DivByZero,
            func: cid,
            pc: 0
        })
    );
}

#[test]
fn entry_errors() {
    let (p, fib) = fib_program();
    let mut vm = Vm::new(&p);
    assert_eq!(
        vm.run(FuncId(9), &[]),
        Err(VmError::NoSuchFunction(FuncId(9)))
    );
    assert_eq!(
        vm.run(fib, &[]),
        Err(VmError::ArgumentCount {
            expected: 1,
            found: 0
        })
    );
    assert_eq!(
        vm.run(fib, &[Value::Float(1.0)]),
        Err(VmError::ArgumentType { index: 0 })
    );
    assert_eq!(vm.run_export("nope", &[]), Err(VmError::NoSuchExport));
    let _ = Target(0);
}
