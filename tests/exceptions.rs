//! Exceptions (LSB §4.3): handler regions, precise unwind points, errors
//! crossing frames, and the canonical `try`/`finally` lowering with pending
//! completions, including a `return` in `finally` overriding the pending
//! completion and an error in `finally` replacing it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{
    ErrorKind, FuncId, FunctionBuilder, GlobalId, Inst, IntOp, IntTy, Label, ModuleBuilder, Reg,
    ValType,
};

const D: ValType = ValType::Dyn;
const I64: ValType = ValType::I64;

fn op() -> IntOp {
    IntOp::new(IntTy::I64)
}

/// What the `try` body does.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Body {
    /// Falls off the end.
    Normal,
    /// `return 10`.
    Return,
    /// `throw 20`.
    Throw,
    /// `1 / 0` (a runtime error).
    DivZero,
    /// `break` out of the enclosing loop (completion kind 3).
    Break,
}

/// What the `finally` body does after counting itself.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fin {
    /// Nothing: dispatch the pending completion.
    Plain,
    /// `return 99`: overrides the pending completion.
    Return,
    /// `throw 77`: replaces the pending completion.
    Throw,
}

/// Builds `main() -> dyn`:
///
/// ```text
/// loop {
///     try { <body> } finally { count += 1; <fin> }
///     return 1          // reached after normal completion
/// }
/// return 2              // reached by break
/// ```
///
/// lowered canonically (LSB §4.3): completion kind in an `i8` register,
/// value in a `dyn` register, one copy of the finally body ending in a
/// `switch` on the kind.
fn program(body: Body, fin: Fin) -> (Program, GlobalId) {
    let mut m = ModuleBuilder::new();
    let count = m.global("count", I64, true, None);
    let mut f = m.function("main", &[], &[D]);
    let kind = f.reg(ValType::I8);
    let val = f.reg(D);
    let err = f.reg(D);
    let (t, one, zero, q) = (f.reg(D), f.reg(I64), f.reg(I64), f.reg(I64));
    let (try_start, try_end, handler, finally, after, broke, ret_pending, throw_pending) = (
        f.label(),
        f.label(),
        f.label(),
        f.label(),
        f.label(),
        f.label(),
        f.label(),
        f.label(),
    );
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    f.bind(try_start);
    match body {
        Body::Normal => {}
        Body::Return => {
            f.emit(Inst::DLoadInt { dst: val, val: 10 });
            f.emit(Inst::LoadInt {
                dst: kind,
                val: 1,
                ty: IntTy::I8,
            });
            f.jmp(finally);
        }
        Body::Throw => {
            f.emit(Inst::DLoadInt { dst: t, val: 20 });
            f.emit(Inst::Throw { src: t });
        }
        Body::DivZero => {
            f.emit(Inst::IDiv {
                dst: q,
                lhs: one,
                rhs: zero,
                op: op(),
            });
        }
        Body::Break => {
            f.emit(Inst::LoadInt {
                dst: kind,
                val: 3,
                ty: IntTy::I8,
            });
            f.jmp(finally);
        }
    }
    f.emit(Inst::LoadInt {
        dst: kind,
        val: 0,
        ty: IntTy::I8,
    });
    f.jmp(finally);
    f.bind(try_end);
    // The catch-all handler: kind 2, value = the error.
    f.bind(handler);
    f.emit(Inst::Mov { dst: val, src: err });
    f.emit(Inst::LoadInt {
        dst: kind,
        val: 2,
        ty: IntTy::I8,
    });
    // (falls into the finally body)
    f.bind(finally);
    let c = f.reg(I64);
    f.emit(Inst::GetGlobal {
        dst: c,
        global: count,
    });
    f.emit(Inst::IAdd {
        dst: c,
        lhs: c,
        rhs: one,
        op: op(),
    });
    f.emit(Inst::SetGlobal {
        global: count,
        src: c,
    });
    match fin {
        Fin::Plain => {}
        Fin::Return => {
            f.emit(Inst::DLoadInt { dst: t, val: 99 });
            f.ret(t);
        }
        Fin::Throw => {
            f.emit(Inst::DLoadInt { dst: t, val: 77 });
            f.emit(Inst::Throw { src: t });
        }
    }
    f.switch(
        IntTy::I8,
        kind,
        &[after, ret_pending, throw_pending, broke],
        after,
    );
    f.bind(ret_pending);
    f.ret(val);
    f.bind(throw_pending);
    f.emit(Inst::Throw { src: val });
    f.bind(after);
    f.emit(Inst::DLoadInt { dst: t, val: 1 });
    f.ret(t);
    f.bind(broke);
    f.emit(Inst::DLoadInt { dst: t, val: 2 });
    f.ret(t);
    f.try_region(try_start, try_end, handler, err);
    m.add_function(f).unwrap();
    (
        Program::load(m.finish().unwrap(), &Host::new()).unwrap(),
        count,
    )
}

fn run(body: Body, fin: Fin) -> (Result<Value, VmError>, Option<Value>) {
    let (p, count) = program(body, fin);
    let mut vm = Vm::new(&p);
    let out = vm.run(FuncId(0), &[]);
    (out, vm.global(count))
}

#[test]
fn finally_runs_once_on_every_exit_and_delivers_the_pending_completion() {
    let once = Some(Value::Int(1));
    assert_eq!(run(Body::Normal, Fin::Plain), (Ok(Value::Int(1)), once));
    assert_eq!(run(Body::Return, Fin::Plain), (Ok(Value::Int(10)), once));
    assert_eq!(run(Body::Break, Fin::Plain), (Ok(Value::Int(2)), once));
    let (out, c) = run(Body::Throw, Fin::Plain);
    assert_eq!(c, once);
    assert!(
        matches!(
            out,
            Err(VmError::Thrown {
                value: Value::Int(20),
                ..
            })
        ),
        "{out:?}"
    );
    let (out, c) = run(Body::DivZero, Fin::Plain);
    assert_eq!(c, once);
    // The rethrown error value keeps the pc that raised it.
    assert!(
        matches!(
            out,
            Err(VmError::Raised {
                kind: ErrorKind::DivByZero,
                pc: 1,
                ..
            })
        ),
        "{out:?}"
    );
}

#[test]
fn return_in_finally_overrides_every_pending_completion() {
    for body in [
        Body::Normal,
        Body::Return,
        Body::Throw,
        Body::DivZero,
        Body::Break,
    ] {
        assert_eq!(
            run(body, Fin::Return),
            (Ok(Value::Int(99)), Some(Value::Int(1))),
            "{body:?}"
        );
    }
}

#[test]
fn error_in_finally_replaces_the_pending_completion() {
    for body in [
        Body::Normal,
        Body::Return,
        Body::Throw,
        Body::DivZero,
        Body::Break,
    ] {
        let (out, c) = run(body, Fin::Throw);
        assert_eq!(c, Some(Value::Int(1)), "{body:?}");
        assert!(
            matches!(
                out,
                Err(VmError::Thrown {
                    value: Value::Int(77),
                    ..
                })
            ),
            "{body:?}: {out:?}"
        );
    }
}

/// `try { try { throw 1 } catch { throw e + 1 } } catch { return e + 1 }`
/// with the inner region listed first (LSB V-CF3: nested before
/// containing).
#[test]
fn nested_regions_innermost_first_and_rethrow_from_a_handler() {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("main", &[], &[D]);
    let (e1, e2, one, x) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    let (outer_s, inner_s, inner_e, outer_e, inner_h, outer_h) = (
        f.label(),
        f.label(),
        f.label(),
        f.label(),
        f.label(),
        f.label(),
    );
    f.emit(Inst::DLoadInt { dst: one, val: 1 });
    f.bind(outer_s);
    f.bind(inner_s);
    f.emit(Inst::Throw { src: one });
    f.bind(inner_e);
    f.bind(inner_h);
    f.emit(Inst::DAdd {
        dst: x,
        lhs: e1,
        rhs: one,
        pol: bytecode_lang::Policy::new(),
    });
    f.emit(Inst::Throw { src: x });
    f.bind(outer_e);
    f.bind(outer_h);
    f.emit(Inst::DAdd {
        dst: x,
        lhs: e2,
        rhs: one,
        pol: bytecode_lang::Policy::new(),
    });
    f.ret(x);
    f.try_region(inner_s, inner_e, inner_h, e1);
    f.try_region(outer_s, outer_e, outer_h, e2);
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(3)));
}

/// Builds a callee that raises `kind` via the given instruction and a
/// caller whose handler returns the error code.
fn caught_code(emit: fn(&mut FunctionBuilder)) -> Result<Value, VmError> {
    let mut m = ModuleBuilder::new();
    let mut callee = m.function("callee", &[], &[]);
    emit(&mut callee);
    callee.ret_void();
    let cid = callee.id();
    let mut main = m.function("main", &[], &[ValType::U32]);
    let (win, e, code) = (main.reg(D), main.reg(D), main.reg(ValType::U32));
    let (s, end, h): (Label, Label, Label) = (main.label(), main.label(), main.label());
    main.bind(s);
    main.emit(Inst::Call {
        dst: win,
        func: cid,
        argc: 0,
    });
    main.bind(end);
    main.ret(code);
    main.bind(h);
    main.emit(Inst::ErrCode { dst: code, src: e });
    main.ret(code);
    main.try_region(s, end, h, e);
    m.add_function(callee).unwrap();
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    Vm::new(&p).run(id, &[])
}

#[test]
fn every_catchable_runtime_error_unwinds_with_its_code() {
    let cases: Vec<(fn(&mut FunctionBuilder), u64)> = vec![
        (
            |f| {
                let (a, b) = (f.reg(I64), f.reg(I64));
                f.emit(Inst::LoadInt {
                    dst: a,
                    val: 1,
                    ty: IntTy::I64,
                });
                f.emit(Inst::IDiv {
                    dst: a,
                    lhs: a,
                    rhs: b,
                    op: op(),
                });
            },
            2,
        ),
        (
            |f| {
                let (a, b) = (f.reg(I64), f.reg(I64));
                f.emit(Inst::LoadInt {
                    dst: b,
                    val: 64,
                    ty: IntTy::I64,
                });
                f.emit(Inst::IShl {
                    dst: a,
                    lhs: a,
                    rhs: b,
                    op: op(),
                });
            },
            3,
        ),
        (
            |f| {
                let (a, b) = (f.reg(D), f.reg(D));
                f.emit(Inst::DAdd {
                    dst: a,
                    lhs: a,
                    rhs: b,
                    pol: bytecode_lang::Policy::new(),
                });
            },
            100,
        ),
        (
            |f| {
                let fn_t = ValType::Dyn;
                let (a, b) = (f.reg(fn_t), f.reg(D));
                f.emit(Inst::ArrayLen { dst: b, arr: a });
            },
            101,
        ),
        (
            |f| {
                let u = f.reg(ValType::U32);
                let c = f.reg(ValType::Char);
                f.emit(Inst::LoadInt {
                    dst: u,
                    val: 0xD800,
                    ty: IntTy::U32,
                });
                f.emit(Inst::CharFromU32 { dst: c, src: u });
            },
            5,
        ),
    ];
    for (emit, code) in cases {
        assert_eq!(caught_code(emit), Ok(Value::UInt(code)));
    }
}

#[test]
fn traps_skip_handlers() {
    let out = caught_code(|f| {
        f.emit(Inst::Unreachable {});
    });
    assert!(
        matches!(
            out,
            Err(VmError::Trap {
                kind: ErrorKind::Unreachable,
                func: FuncId(0),
                pc: 0
            })
        ),
        "{out:?}"
    );
    let _ = Reg(0);
}

#[test]
fn a_handler_loop_without_back_edges_still_runs_out_of_fuel() {
    // A handler whose target is the throwing instruction itself: no jump
    // instruction anywhere, so only handler-entry fuel bounds it.
    let mut m = ModuleBuilder::new();
    let mut f = m.function("main", &[], &[]);
    let e = f.reg(D);
    let (s, end) = (f.label(), f.label());
    f.bind(s);
    f.emit(Inst::Throw { src: e });
    f.bind(end);
    f.try_region(s, end, s, e);
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let out = Vm::new(&p).run_with(FuncId(0), &[], bvm_lang::Limits::new().with_fuel(1000));
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
fn many_regions_use_the_first_covering_handler_in_list_order() {
    // 40 nested regions around one throw, listed innermost first: the
    // innermost handler (list index 0) must catch it. With more than eight
    // regions the VM uses its per-pc table; the answer must not change.
    for regions in [3usize, 40] {
        let mut m = ModuleBuilder::new();
        let mut f = m.function("main", &[], &[I64]);
        let (e, r) = (f.reg(D), f.reg(I64));
        let starts: Vec<_> = (0..regions).map(|_| f.label()).collect();
        let ends: Vec<_> = (0..regions).map(|_| f.label()).collect();
        let handlers: Vec<_> = (0..regions).map(|_| f.label()).collect();
        for s in starts.iter().rev() {
            f.bind(*s);
        }
        f.emit(Inst::Throw { src: e });
        for end in &ends {
            f.bind(*end);
        }
        for (i, h) in handlers.iter().enumerate() {
            f.bind(*h);
            f.emit(Inst::LoadInt {
                dst: r,
                val: i32::try_from(i).unwrap(),
                ty: IntTy::I64,
            });
            f.ret(r);
        }
        for i in 0..regions {
            f.try_region(starts[i], ends[i], handlers[i], e);
        }
        m.add_function(f).unwrap();
        let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
        assert_eq!(
            Vm::new(&p).run(FuncId(0), &[]),
            Ok(Value::Int(0)),
            "{regions} regions"
        );
    }
}
