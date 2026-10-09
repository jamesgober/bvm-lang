//! LSB conformance, coroutines (LSB §5.13): every instruction of the group
//! (`coro_new`, `coro_new_indirect`, `yield`, `yield_kv`, `await`, `resume`,
//! `resume_throw`, `coro_status`, `coro_current`, `spawn`, `coro_close`,
//! `coro_key`, `coro_result`) and rules 1-12 and 14. Rule 13 (dropping) and
//! the collector are in `coroutine_gc.rs`; the scheduler in `scheduler.rs`.
//! Each test names the instructions or rules it covers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use bvm_lang::{Host, Limits, LoadErrorKind, Program, Value, Vm, VmError};
use bytecode_lang::{
    Callee, CoroState, ErrorKind, FuncId, FunctionBuilder, GlobalId, Hook, Inst, IntOp, IntTy,
    Kind, ModuleBuilder, Policy, Prim, Reg, TypeDef, TypeId, ValType,
};

const D: ValType = ValType::Dyn;
const I64: ValType = ValType::I64;

fn pol() -> Policy {
    Policy::new()
}

/// A `dyn` register loaded with `v`.
fn dint(f: &mut FunctionBuilder, v: i32) -> Reg {
    let r = f.reg(D);
    f.emit(Inst::DLoadInt { dst: r, val: v });
    r
}

/// A new empty `dyn` array in a `dyn` register.
fn out(f: &mut FunctionBuilder, at: TypeId) -> Reg {
    let (len, a) = (f.reg(I64), f.reg(D));
    let ty = f.type_ref(at);
    f.emit(Inst::LoadInt {
        dst: len,
        val: 0,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewArray { dst: a, len, ty });
    a
}

fn push(f: &mut FunctionBuilder, arr: Reg, v: Reg) {
    f.emit(Inst::ArrayPush { arr, src: v });
}

/// Pushes a coroutine's status as a dyn int.
fn push_status(f: &mut FunctionBuilder, arr: Reg, coro: Reg) {
    let (s, d) = (f.reg(ValType::U8), f.reg(D));
    f.emit(Inst::CoroStatus { dst: s, coro });
    f.emit(Inst::ToDyn {
        dst: d,
        src: s,
        from: Prim::U8,
    });
    push(f, arr, d);
}

/// A window of `n + 1` dyn registers (destination, then arguments).
fn window(f: &mut FunctionBuilder, n: usize) -> Reg {
    f.regs(&vec![D; n + 1])
}

fn arg(w: Reg, i: u16) -> Reg {
    Reg(w.0 + 1 + i)
}

fn load(m: ModuleBuilder) -> Program {
    Program::load(m.finish().unwrap(), &Host::new()).unwrap()
}

fn ints(vm: &Vm<'_>, v: Value) -> Vec<Value> {
    vm.elements(v).expect("an array")
}

/// `gen(start)`: `x = yield start; y = yield x + 1; return y * 10`.
fn gen_echo(m: &mut ModuleBuilder) -> FuncId {
    let mut g = m.function("gen", &[D], &[D]);
    let (x, t, y, ten) = (g.reg(D), g.reg(D), g.reg(D), g.reg(D));
    g.emit(Inst::Yield {
        dst: x,
        src: g.param(0),
    });
    let one = dint(&mut g, 1);
    g.emit(Inst::DAdd {
        dst: t,
        lhs: x,
        rhs: one,
        pol: pol(),
    });
    g.emit(Inst::Yield { dst: y, src: t });
    g.emit(Inst::DLoadInt { dst: ten, val: 10 });
    g.emit(Inst::DMul {
        dst: y,
        lhs: y,
        rhs: ten,
        pol: pol(),
    });
    g.ret(y);
    m.add_function(g).unwrap()
}

#[test]
fn op_coro_new_resume_yield_status_result_and_send() {
    // coro_new, resume, yield, coro_status, coro_result, coro_key; rules 1,
    // 3, 10 (automatic keys).
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let gen_id = gen_echo(&mut m);
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let w = window(&mut f, 1);
    f.emit(Inst::DLoadInt {
        dst: arg(w, 0),
        val: 5,
    });
    f.emit(Inst::CoroNew {
        dst: w,
        func: gen_id,
        argc: 1,
    });
    let c = f.reg(D);
    f.mov(c, w);
    push_status(&mut f, o, c); // created 0
    let (r, k) = (f.reg(D), f.reg(D));
    f.emit(Inst::CoroKey { dst: k, coro: c });
    push(&mut f, o, k); // nil before the first yield
    let ignored = dint(&mut f, 1000);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: ignored,
    });
    push(&mut f, o, r); // 5
    push_status(&mut f, o, c); // yielded 2
    f.emit(Inst::CoroKey { dst: k, coro: c });
    push(&mut f, o, k); // key 0
    let sent = dint(&mut f, 41);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: sent,
    });
    push(&mut f, o, r); // 42
    f.emit(Inst::CoroKey { dst: k, coro: c });
    push(&mut f, o, k); // key 1
    let sent2 = dint(&mut f, 7);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: sent2,
    });
    push(&mut f, o, r); // 70 (return value)
    push_status(&mut f, o, c); // returned 4
    f.emit(Inst::CoroResult { dst: r, coro: c });
    push(&mut f, o, r); // 70 again
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    use Value::{Int, Nil};
    assert_eq!(
        ints(&vm, v),
        vec![
            Int(0),
            Nil,
            Int(5),
            Int(2),
            Int(0),
            Int(42),
            Int(1),
            Int(70),
            Int(4),
            Int(70)
        ]
    );
}

#[test]
fn resuming_a_finished_running_or_failed_coroutine_is_invalid_coro_state() {
    // resume (rule 1), coro_result (E0110), coro_status of failed.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let mut empty = m.function("empty", &[], &[]);
    empty.ret_void();
    let empty = m.add_function(empty).unwrap();
    let mut bad = m.function("bad", &[], &[D]);
    let (one, zero, q) = (dint(&mut bad, 1), dint(&mut bad, 0), bad.reg(D));
    bad.emit(Inst::DDiv {
        dst: q,
        lhs: one,
        rhs: zero,
        pol: pol(),
    });
    bad.ret(q);
    let bad = m.add_function(bad).unwrap();

    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let (c, r, e, n) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    let code = f.reg(ValType::U32);
    let cd = f.reg(D);
    // A void body returns nil; resuming it again is invalid.
    f.emit(Inst::CoroNew {
        dst: c,
        func: empty,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: n,
    });
    push(&mut f, o, r); // nil
    f.emit(Inst::CoroResult { dst: r, coro: c });
    push(&mut f, o, r); // nil (void body)
    let catch_code = |f: &mut FunctionBuilder, o: Reg, inst: Inst| {
        let (s, end, h) = (f.label(), f.label(), f.label());
        let after = f.label();
        f.bind(s);
        f.emit(inst);
        f.bind(end);
        f.jmp(after);
        f.bind(h);
        f.emit(Inst::ErrCode { dst: code, src: e });
        f.emit(Inst::ToDyn {
            dst: cd,
            src: code,
            from: Prim::U32,
        });
        push(f, o, cd);
        f.bind(after);
        f.try_region(s, end, h, e);
    };
    catch_code(
        &mut f,
        o,
        Inst::Resume {
            dst: r,
            coro: c,
            src: n,
        },
    ); // 110
    // A failing body: the error is raised at the resume, then the
    // coroutine is failed.
    f.emit(Inst::CoroNew {
        dst: c,
        func: bad,
        argc: 0,
    });
    catch_code(
        &mut f,
        o,
        Inst::Resume {
            dst: r,
            coro: c,
            src: n,
        },
    ); // 2 (DivByZero)
    push_status(&mut f, o, c); // 5
    catch_code(&mut f, o, Inst::CoroResult { dst: r, coro: c }); // 110
    catch_code(
        &mut f,
        o,
        Inst::Resume {
            dst: r,
            coro: c,
            src: n,
        },
    ); // 110
    // Not a coroutine at all.
    catch_code(
        &mut f,
        o,
        Inst::Resume {
            dst: r,
            coro: n,
            src: n,
        },
    ); // 101 NullReference (nil)
    let one = dint(&mut f, 1);
    catch_code(
        &mut f,
        o,
        Inst::CoroStatus {
            dst: code,
            coro: one,
        },
    ); // 100 TypeError
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    use Value::{Int, Nil};
    assert_eq!(
        ints(&vm, v),
        vec![
            Nil,
            Nil,
            Int(110),
            Int(2),
            Int(5),
            Int(110),
            Int(110),
            Int(101),
            Int(100)
        ]
    );
}

#[test]
fn a_coroutine_resuming_itself_or_its_resumer_is_invalid_and_status_is_running() {
    // resume (rule 1: running includes resumers), coro_current, coro_status
    // of a running coroutine.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let g_out = m.global("out", D, true, None);
    let g_outer = m.global("outer", D, true, None);
    // inner(): tries to resume `outer` (its resumer) and itself.
    let mut inner = m.function("inner", &[], &[D]);
    let (o, me, r, e, n, code) = (
        inner.reg(D),
        inner.reg(D),
        inner.reg(D),
        inner.reg(D),
        inner.reg(D),
        inner.reg(ValType::U32),
    );
    inner.emit(Inst::GetGlobal {
        dst: o,
        global: g_out,
    });
    inner.emit(Inst::CoroCurrent { dst: me });
    let outer = inner.reg(D);
    inner.emit(Inst::GetGlobal {
        dst: outer,
        global: g_outer,
    });
    push_status(&mut inner, o, outer); // 1 running (a resumer)
    push_status(&mut inner, o, me); // 1 running (itself)
    for target in [outer, me] {
        let (s, end, h, after) = (inner.label(), inner.label(), inner.label(), inner.label());
        inner.bind(s);
        inner.emit(Inst::Resume {
            dst: r,
            coro: target,
            src: n,
        });
        inner.bind(end);
        inner.jmp(after);
        inner.bind(h);
        inner.emit(Inst::ErrCode { dst: code, src: e });
        let cd = inner.reg(D);
        inner.emit(Inst::ToDyn {
            dst: cd,
            src: code,
            from: Prim::U32,
        });
        push(&mut inner, o, cd);
        inner.bind(after);
        inner.try_region(s, end, h, e);
    }
    inner.ret(me);
    let inner = m.add_function(inner).unwrap();
    // outer(): resumes inner, returns what inner returned (inner itself).
    let mut ob = m.function("outer", &[], &[D]);
    let (c, r2, n2, me2) = (ob.reg(D), ob.reg(D), ob.reg(D), ob.reg(D));
    ob.emit(Inst::CoroCurrent { dst: me2 });
    ob.emit(Inst::SetGlobal {
        global: g_outer,
        src: me2,
    });
    ob.emit(Inst::CoroNew {
        dst: c,
        func: inner,
        argc: 0,
    });
    ob.emit(Inst::Resume {
        dst: r2,
        coro: c,
        src: n2,
    });
    ob.ret(r2);
    let outer_f = m.add_function(ob).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    f.emit(Inst::SetGlobal {
        global: g_out,
        src: o,
    });
    let (cur, c, r, n) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroCurrent { dst: cur });
    push(&mut f, o, cur); // nil on the main stack
    f.emit(Inst::CoroNew {
        dst: c,
        func: outer_f,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: n,
    });
    let k = f.reg(ValType::U8);
    f.emit(Inst::TypeOf { dst: k, src: r });
    let kd = f.reg(D);
    f.emit(Inst::ToDyn {
        dst: kd,
        src: k,
        from: Prim::U8,
    });
    push(&mut f, o, kd); // 13: the inner coroutine came back as the result
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    use Value::{Int, Nil};
    assert_eq!(
        ints(&vm, v),
        vec![Nil, Int(1), Int(1), Int(110), Int(110), Int(13)]
    );
}

#[test]
fn stackful_yield_from_nested_calls_and_hooks() {
    // yield at depth (rule 2: every frame from the body up is suspended),
    // including through a `dadd` hook frame; coro_current inside a helper.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    // The `add` hook yields its left operand, then returns lhs + the value
    // it was sent.
    let mut hook = m.function("add_hook", &[D, D], &[D]);
    let (s, t) = (hook.reg(D), hook.reg(D));
    hook.emit(Inst::Yield {
        dst: s,
        src: hook.param(0),
    });
    // The left operand is a string here (no fast path): the hook yields
    // it, then returns the value it was sent.
    hook.emit(Inst::Mov { dst: t, src: s });
    hook.ret(t);
    let hook_id = m.add_function(hook).unwrap();
    m.hook(Hook::Add, Callee::Func(hook_id));
    // leaf(x): y = yield x; return y + 1
    let mut leaf = m.function("leaf", &[D], &[D]);
    let (y, one) = (leaf.reg(D), leaf.reg(D));
    leaf.emit(Inst::Yield {
        dst: y,
        src: leaf.param(0),
    });
    leaf.emit(Inst::DLoadInt { dst: one, val: 1 });
    leaf.emit(Inst::DAdd {
        dst: y,
        lhs: y,
        rhs: one,
        pol: pol(),
    });
    leaf.ret(y);
    let leaf_id = m.add_function(leaf).unwrap();
    // mid(x): return leaf(x * 2) * 3
    let mut mid = m.function("mid", &[D], &[D]);
    let w = window(&mut mid, 1);
    let two = dint(&mut mid, 2);
    mid.emit(Inst::DMul {
        dst: arg(w, 0),
        lhs: mid.param(0),
        rhs: two,
        pol: pol(),
    });
    mid.emit(Inst::Call {
        dst: w,
        func: leaf_id,
        argc: 1,
    });
    let three = dint(&mut mid, 3);
    mid.emit(Inst::DMul {
        dst: w,
        lhs: w,
        rhs: three,
        pol: pol(),
    });
    mid.ret(w);
    let mid_id = m.add_function(mid).unwrap();
    // body(): a = mid(5); b = "s" + a (hook yields "s"); return b
    let mut body = m.function("body", &[], &[D]);
    let w = window(&mut body, 1);
    body.emit(Inst::DLoadInt {
        dst: arg(w, 0),
        val: 5,
    });
    body.emit(Inst::Call {
        dst: w,
        func: mid_id,
        argc: 1,
    });
    let str_k = m.constant(bytecode_lang::Const::Bytes(b"s".to_vec()));
    let (s, b) = (body.reg(D), body.reg(D));
    body.emit(Inst::DLoadConst { dst: s, k: str_k });
    body.emit(Inst::DAdd {
        dst: b,
        lhs: s,
        rhs: w,
        pol: pol(),
    });
    body.ret(b);
    let body_id = m.add_function(body).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let (c, r) = (f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: body_id,
        argc: 0,
    });
    let nil = f.reg(D);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    push(&mut f, o, r); // 10 (yielded by leaf, two frames up)
    let sent = dint(&mut f, 4);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: sent,
    });
    let k = f.reg(ValType::U8);
    f.emit(Inst::TypeOf { dst: k, src: r });
    let kd = f.reg(D);
    f.emit(Inst::ToDyn {
        dst: kd,
        src: k,
        from: Prim::U8,
    });
    push(&mut f, o, kd); // 5: the hook yielded the string "s"
    let sent = dint(&mut f, 1000);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: sent,
    });
    push(&mut f, o, r); // 1000: the hook returned what it was sent
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    assert_eq!(
        ints(&vm, v),
        vec![Value::Int(10), Value::Int(5), Value::Int(1000)]
    );
}

#[test]
fn yield_and_await_outside_a_coroutine_raise_cannot_suspend() {
    // yield, yield_kv, await: E0111, catchable.
    for inst in [
        Inst::Yield {
            dst: Reg(0),
            src: Reg(1),
        },
        Inst::YieldKv {
            dst: Reg(0),
            key: Reg(1),
            src: Reg(1),
        },
        Inst::Await {
            dst: Reg(0),
            src: Reg(1),
        },
    ] {
        let out = common::eval(&[], &[], &[], |_, f| {
            let _ = f.reg(D);
            let _ = f.reg(D);
            f.emit(Inst::Nop {});
            f.emit(inst);
            f.ret_void();
        });
        assert_eq!(
            out,
            Err(VmError::Raised {
                kind: ErrorKind::CannotSuspend,
                payload: bvm_lang::Value::Nil,
                func: FuncId(0),
                pc: 1
            }),
            "{inst}"
        );
    }
}

#[test]
fn op_coro_new_indirect_with_closures_dyn_conversion_and_errors() {
    // coro_new_indirect: a closure's captures are visible to the body; a
    // dyn callee converts dyn arguments to the body's typed parameters
    // (`dcall` rules); arity, nil, non-function, and import callees fail.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let sig = m.func_type(&[D], &[D]);
    let imp = m.import("env", "id", sig);
    // body(n: i64) captures k (dyn): yields k, returns n * 2 (as i64).
    let mut body = m.function("body", &[I64], &[I64]);
    body.capture(D);
    let (k, s, r) = (body.reg(D), body.reg(D), body.reg(I64));
    body.emit(Inst::GetUpval {
        dst: k,
        idx: bytecode_lang::UpvalIdx(0),
    });
    body.emit(Inst::Yield { dst: s, src: k });
    body.emit(Inst::IAdd {
        dst: r,
        lhs: body.param(0),
        rhs: body.param(0),
        op: IntOp::new(IntTy::I64),
    });
    body.ret(r);
    let body_id = m.add_function(body).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let cl = window(&mut f, 1);
    f.emit(Inst::DLoadInt {
        dst: arg(cl, 0),
        val: 77,
    });
    f.emit(Inst::MakeClosure {
        dst: cl,
        func: body_id,
    });
    let w = window(&mut f, 1);
    f.emit(Inst::DLoadInt {
        dst: arg(w, 0),
        val: 21,
    });
    f.emit(Inst::CoroNewIndirect {
        dst: w,
        callee: cl,
        argc: 1,
    });
    let (c, r, nil, e) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.mov(c, w);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    push(&mut f, o, r); // 77, the capture
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    push(&mut f, o, r); // 42: the i64 result boxed by to_dyn
    let code = f.reg(ValType::U32);
    let cd = f.reg(D);
    let try_code = |f: &mut FunctionBuilder, emit: &dyn Fn(&mut FunctionBuilder)| {
        let (s, end, h, after) = (f.label(), f.label(), f.label(), f.label());
        f.bind(s);
        emit(f);
        f.bind(end);
        f.jmp(after);
        f.bind(h);
        f.emit(Inst::ErrCode { dst: code, src: e });
        f.emit(Inst::ToDyn {
            dst: cd,
            src: code,
            from: Prim::U32,
        });
        push(f, o, cd);
        f.bind(after);
        f.try_region(s, end, h, e);
    };
    // A dyn argument that is not an int: TypeError (dcall conversion).
    try_code(&mut f, &|f| {
        let w = window(f, 1);
        f.emit(Inst::LoadNil { dst: arg(w, 0) });
        f.emit(Inst::CoroNewIndirect {
            dst: w,
            callee: cl,
            argc: 1,
        });
    });
    // Wrong arity: an ArgumentError (E0114, format 2), as `dcall` binds.
    try_code(&mut f, &|f| {
        let w = window(f, 0);
        f.emit(Inst::CoroNewIndirect {
            dst: w,
            callee: cl,
            argc: 0,
        });
    });
    // nil callee.
    try_code(&mut f, &|f| {
        let (w, nil) = (window(f, 0), f.reg(D));
        f.emit(Inst::CoroNewIndirect {
            dst: w,
            callee: nil,
            argc: 0,
        });
    });
    // An import: host functions cannot be suspended.
    try_code(&mut f, &|f| {
        let (w, h) = (window(f, 1), f.reg(D));
        f.emit(Inst::LoadImport {
            dst: h,
            import: imp,
        });
        f.emit(Inst::CoroNewIndirect {
            dst: w,
            callee: h,
            argc: 1,
        });
    });
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let mut host = Host::new();
    host.register("env", "id", |_, a| Ok(a[0]));
    let p = Program::load(m.finish().unwrap(), &host).unwrap();
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    use Value::Int;
    assert_eq!(
        ints(&vm, v),
        vec![Int(77), Int(42), Int(100), Int(114), Int(101), Int(100)]
    );
}

#[test]
fn coro_new_is_checked_at_load_like_call() {
    // coro_new: window, arity, and no captures are load-time facts.
    let mut m = ModuleBuilder::new();
    let mut body = m.function("body", &[D], &[]);
    body.ret_void();
    let body = m.add_function(body).unwrap();
    let mut f = m.function("main", &[], &[]);
    let w = window(&mut f, 1);
    f.emit(Inst::CoroNew {
        dst: w,
        func: body,
        argc: 0,
    });
    f.ret_void();
    m.add_function(f).unwrap();
    let err = Program::load(m.finish().unwrap(), &Host::new()).unwrap_err();
    assert_eq!(
        err.kind(),
        &LoadErrorKind::ArityMismatch {
            expected: 1,
            found: 0
        }
    );

    let mut m = ModuleBuilder::new();
    let mut body = m.function("body", &[], &[]);
    body.capture(D);
    body.ret_void();
    let body = m.add_function(body).unwrap();
    let mut f = m.function("main", &[], &[]);
    let w = window(&mut f, 0);
    f.emit(Inst::CoroNew {
        dst: w,
        func: body,
        argc: 0,
    });
    f.ret_void();
    m.add_function(f).unwrap();
    let err = Program::load(m.finish().unwrap(), &Host::new()).unwrap_err();
    assert_eq!(err.kind(), &LoadErrorKind::CalleeHasCaptures);
}

/// `gen()`: `try { yield 1; yield 2 } catch (e) { yield e + 100 } return 9`.
fn gen_catching(m: &mut ModuleBuilder) -> FuncId {
    let mut g = m.function("gen", &[], &[D]);
    let (s, e, t) = (g.reg(D), g.reg(D), g.reg(D));
    let (start, end, h) = (g.label(), g.label(), g.label());
    let one = dint(&mut g, 1);
    let two = dint(&mut g, 2);
    let hundred = dint(&mut g, 100);
    let nine = dint(&mut g, 9);
    g.bind(start);
    g.emit(Inst::Yield { dst: s, src: one });
    g.emit(Inst::Yield { dst: s, src: two });
    g.bind(end);
    g.ret(nine);
    g.bind(h);
    g.emit(Inst::DAdd {
        dst: t,
        lhs: e,
        rhs: hundred,
        pol: pol(),
    });
    g.emit(Inst::Yield { dst: s, src: t });
    g.ret(nine);
    g.try_region(start, end, h, e);
    m.add_function(g).unwrap()
}

#[test]
fn op_resume_throw_is_caught_at_the_yield_or_fails_the_coroutine() {
    // resume_throw (rules 4 and 5): a try around the yield catches it; an
    // uncaught throw fails the coroutine and is raised at resume_throw; on a
    // created coroutine nothing runs.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let g = gen_catching(&mut m);
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let (c, r, nil, e) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    push(&mut f, o, r); // 1
    let five = dint(&mut f, 5);
    f.emit(Inst::ResumeThrow {
        dst: r,
        coro: c,
        src: five,
    });
    push(&mut f, o, r); // 105: caught at the yield, handler yields
    // Now suspended in the handler, outside the region: uncaught.
    let (s, end, h, after) = (f.label(), f.label(), f.label(), f.label());
    let six = dint(&mut f, 6);
    f.bind(s);
    f.emit(Inst::ResumeThrow {
        dst: r,
        coro: c,
        src: six,
    });
    f.bind(end);
    f.jmp(after);
    f.bind(h);
    push(&mut f, o, e); // 6, raised at the resume_throw
    push_status(&mut f, o, c); // 5 failed
    f.bind(after);
    f.try_region(s, end, h, e);
    // On a created coroutine: failed without running, raised here.
    let (s, end, h, after) = (f.label(), f.label(), f.label(), f.label());
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    let seven = dint(&mut f, 7);
    f.bind(s);
    f.emit(Inst::ResumeThrow {
        dst: r,
        coro: c,
        src: seven,
    });
    f.bind(end);
    f.jmp(after);
    f.bind(h);
    push(&mut f, o, e); // 7
    push_status(&mut f, o, c); // 5
    f.bind(after);
    f.try_region(s, end, h, e);
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    use Value::Int;
    assert_eq!(
        ints(&vm, v),
        vec![Int(1), Int(105), Int(6), Int(5), Int(7), Int(5)]
    );
}

#[test]
fn errors_escaping_a_coroutine_end_the_run_with_their_origin() {
    // rule 3: an error escaping the body is raised at the resume; uncaught,
    // the run ends with the original location.
    let mut m = ModuleBuilder::new();
    let mut g = m.function("gen", &[], &[]);
    let (one, zero, q) = (dint(&mut g, 1), dint(&mut g, 0), g.reg(D));
    g.emit(Inst::DFloorDiv {
        dst: q,
        lhs: one,
        rhs: zero,
        pol: pol(),
    });
    g.ret_void();
    let gid = m.add_function(g).unwrap();
    let mut f = m.function("main", &[], &[]);
    let (c, r, n) = (f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: gid,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: n,
    });
    f.ret_void();
    let main = m.add_function(f).unwrap();
    let p = load(m);
    assert_eq!(
        Vm::new(&p).run(main, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::DivByZero,
            payload: bvm_lang::Value::Nil,
            func: gid,
            pc: 2
        })
    );
}

#[test]
fn op_yield_kv_keys_and_the_automatic_key_rule() {
    // yield_kv, yield, coro_key (rule 10): explicit keys of any kind; an
    // explicit integer key larger than any before moves the automatic key;
    // a smaller one does not; past i64::MAX the automatic key overflows.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let a_k = m.constant(bytecode_lang::Const::Bytes(b"a".to_vec()));
    let max_k = m.constant(bytecode_lang::Const::Int(i64::MAX));
    let mut g = m.function("gen", &[], &[]);
    let (s, a, e) = (g.reg(D), g.reg(D), g.reg(D));
    g.emit(Inst::DLoadConst { dst: a, k: a_k });
    let v1 = dint(&mut g, 1);
    g.emit(Inst::YieldKv {
        dst: s,
        key: a,
        src: v1,
    }); // "a"
    g.emit(Inst::Yield { dst: s, src: v1 }); // 0
    let ten = dint(&mut g, 10);
    g.emit(Inst::YieldKv {
        dst: s,
        key: ten,
        src: v1,
    }); // 10
    g.emit(Inst::Yield { dst: s, src: v1 }); // 11
    let three = dint(&mut g, 3);
    g.emit(Inst::YieldKv {
        dst: s,
        key: three,
        src: v1,
    }); // 3
    g.emit(Inst::Yield { dst: s, src: v1 }); // 12
    let big = g.reg(D);
    g.emit(Inst::DLoadConst { dst: big, k: max_k });
    g.emit(Inst::YieldKv {
        dst: s,
        key: big,
        src: v1,
    }); // i64::MAX
    let (st, end, h, after) = (g.label(), g.label(), g.label(), g.label());
    g.bind(st);
    g.emit(Inst::Yield { dst: s, src: v1 }); // ArithOverflow
    g.bind(end);
    g.jmp(after);
    g.bind(h);
    let code = g.reg(ValType::U32);
    let cd = g.reg(D);
    g.emit(Inst::ErrCode { dst: code, src: e });
    g.emit(Inst::ToDyn {
        dst: cd,
        src: code,
        from: Prim::U32,
    });
    g.emit(Inst::YieldKv {
        dst: s,
        key: cd,
        src: v1,
    }); // key 1 (the error code), largest stays i64::MAX
    g.bind(after);
    g.ret_void();
    g.try_region(st, end, h, e);
    let gid = m.add_function(g).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let (c, r, k, nil) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: gid,
        argc: 0,
    });
    for _ in 0..8 {
        f.emit(Inst::Resume {
            dst: r,
            coro: c,
            src: nil,
        });
        f.emit(Inst::CoroKey { dst: k, coro: c });
        push(&mut f, o, k);
    }
    // After the body returns the key is still the last one.
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    f.emit(Inst::CoroKey { dst: k, coro: c });
    push(&mut f, o, k);
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    let keys = ints(&vm, v);
    assert_eq!(vm.str_bytes(keys[0]), Some(&b"a"[..]));
    use Value::Int;
    assert_eq!(
        keys[1..].to_vec(),
        vec![
            Int(0),
            Int(10),
            Int(11),
            Int(3),
            Int(12),
            Int(i64::MAX),
            Int(1),
            Int(1)
        ]
    );
}

#[test]
fn the_automatic_key_after_only_negative_keys_is_zero_as_in_php_generators() {
    // Rule 10 (format 2, PHP's generators): the counter starts at -1 and an
    // explicit key only raises it, so after only `yield -5 => x` the next
    // automatic key is 0 (format 1's map_push rule gave -4).
    let mut m = ModuleBuilder::new();
    let mut g = m.function("gen", &[], &[]);
    let s = g.reg(D);
    let neg = dint(&mut g, -5);
    g.emit(Inst::YieldKv {
        dst: s,
        key: neg,
        src: neg,
    });
    g.emit(Inst::Yield { dst: s, src: neg });
    g.ret_void();
    let gid = m.add_function(g).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let (c, r, k, nil) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: gid,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    f.emit(Inst::CoroKey { dst: k, coro: c });
    f.ret(k);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    assert_eq!(Vm::new(&p).run(main, &[]), Ok(Value::Int(0)));
}

#[test]
fn a_coroutine_body_returns_typed_results_through_to_dyn() {
    // rule 3: the result is converted by to_dyn; a u64 above i64::MAX
    // raises ArithOverflow at the body's `ret` (catchable there).
    let mut m = ModuleBuilder::new();
    let big = m.constant(bytecode_lang::Const::UInt(u64::MAX));
    let mut g = m.function("gen", &[], &[ValType::U64]);
    let r = g.reg(ValType::U64);
    g.emit(Inst::LoadConst { dst: r, k: big });
    g.ret(r);
    let gid = m.add_function(g).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let (c, out, nil) = (f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: gid,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: out,
        coro: c,
        src: nil,
    });
    f.ret(out);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    assert_eq!(
        Vm::new(&p).run(main, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::ArithOverflow,
            payload: bvm_lang::Value::Nil,
            func: gid,
            pc: 1
        })
    );
}

/// `gen(count_global)`: `try { yield 1; yield 2 } finally { count += 1 }`
/// in the canonical lowering, plus an optional extra action in the
/// handler-side path.
#[derive(Clone, Copy, PartialEq, Debug)]
enum OnClose {
    /// Let the signal run `finally` and propagate (a normal close).
    Finally,
    /// Catch the signal and return 55.
    CatchReturn,
    /// Catch the signal and throw 66 instead.
    CatchThrow,
    /// Catch the signal and yield 77 (refusing the close).
    CatchYield,
    /// Catch the signal, await 88, then rethrow the signal.
    CatchAwait,
}

fn gen_closable(m: &mut ModuleBuilder, count: GlobalId, on: OnClose) -> FuncId {
    let mut g = m.function("gen", &[], &[D]);
    let (s, e, c) = (g.reg(D), g.reg(D), g.reg(I64));
    let (start, end, h) = (g.label(), g.label(), g.label());
    let one = dint(&mut g, 1);
    let two = dint(&mut g, 2);
    g.bind(start);
    g.emit(Inst::Yield { dst: s, src: one });
    g.emit(Inst::Yield { dst: s, src: two });
    g.bind(end);
    let zero = dint(&mut g, 0);
    g.ret(zero);
    g.bind(h);
    // finally: count += 1
    g.emit(Inst::GetGlobal {
        dst: c,
        global: count,
    });
    let k1 = g.reg(I64);
    g.emit(Inst::LoadInt {
        dst: k1,
        val: 1,
        ty: IntTy::I64,
    });
    g.emit(Inst::IAdd {
        dst: c,
        lhs: c,
        rhs: k1,
        op: IntOp::new(IntTy::I64),
    });
    g.emit(Inst::SetGlobal {
        global: count,
        src: c,
    });
    match on {
        OnClose::Finally => {
            g.emit(Inst::Throw { src: e });
        }
        OnClose::CatchReturn => {
            let v = dint(&mut g, 55);
            g.ret(v);
        }
        OnClose::CatchThrow => {
            let v = dint(&mut g, 66);
            g.emit(Inst::Throw { src: v });
        }
        OnClose::CatchYield => {
            let v = dint(&mut g, 77);
            g.emit(Inst::Yield { dst: s, src: v });
            g.ret(s);
        }
        OnClose::CatchAwait => {
            let v = dint(&mut g, 88);
            g.emit(Inst::Await { dst: s, src: v });
            g.emit(Inst::Throw { src: e });
        }
    }
    g.try_region(start, end, h, e);
    m.add_function(g).unwrap()
}

/// Runs: make the generator, resume once (suspended at `yield 1` inside the
/// try), close it with signal 999 under a catch-all, recording the close's
/// result (or the caught error's value), the status, and the count; then
/// any `extra` steps.
fn close_scenario(on: OnClose, extra: impl Fn(&mut FunctionBuilder, Reg, Reg)) -> Vec<Value> {
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let count = m.global("count", I64, true, None);
    let g = gen_closable(&mut m, count, on);
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let (c, r, nil, e, sig) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    f.emit(Inst::DLoadInt { dst: sig, val: 999 });
    let (s, end, h, after) = (f.label(), f.label(), f.label(), f.label());
    f.bind(s);
    f.emit(Inst::CoroClose {
        dst: r,
        coro: c,
        src: sig,
    });
    f.bind(end);
    push(&mut f, o, r);
    f.jmp(after);
    f.bind(h);
    let k = f.reg(ValType::U32);
    let kd = f.reg(D);
    f.emit(Inst::ErrCode { dst: k, src: e });
    f.emit(Inst::ToDyn {
        dst: kd,
        src: k,
        from: Prim::U32,
    });
    // An error value reports its code; a thrown value is pushed as is.
    let is_err = f.reg(ValType::Bool);
    f.emit(Inst::IsKind {
        dst: is_err,
        src: e,
        kind: Kind::Error,
    });
    let thrown = f.label();
    f.jmp_if_not(is_err, thrown);
    push(&mut f, o, kd);
    f.jmp(after);
    f.bind(thrown);
    push(&mut f, o, e);
    f.bind(after);
    f.try_region(s, end, h, e);
    push_status(&mut f, o, c);
    let cnt = f.reg(I64);
    let cntd = f.reg(D);
    f.emit(Inst::GetGlobal {
        dst: cnt,
        global: count,
    });
    f.emit(Inst::ToDyn {
        dst: cntd,
        src: cnt,
        from: Prim::I64,
    });
    push(&mut f, o, cntd);
    extra(&mut f, o, c);
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    ints(&vm, v)
}

#[test]
fn op_coro_close_runs_finally_and_the_signal_escaping_is_success() {
    // coro_close (rule 11): finally runs; the signal itself escaping means
    // the close succeeded (returned, dst nil); closing again is a no-op.
    use Value::{Int, Nil};
    let out = close_scenario(OnClose::Finally, |f, o, c| {
        let (r, sig) = (f.reg(D), f.reg(D));
        f.emit(Inst::CoroClose {
            dst: r,
            coro: c,
            src: sig,
        });
        push(f, o, r);
    });
    assert_eq!(out, vec![Nil, Int(4), Int(1), Nil]);
}

#[test]
fn coro_close_returns_the_bodys_return_value_or_raises_its_other_error() {
    use Value::Int;
    // The body catches the signal and returns: dst = the value.
    assert_eq!(
        close_scenario(OnClose::CatchReturn, |_, _, _| {}),
        vec![Int(55), Int(4), Int(1)]
    );
    // Another error escapes: failed, raised at the closer.
    assert_eq!(
        close_scenario(OnClose::CatchThrow, |_, _, _| {}),
        vec![Int(66), Int(5), Int(1)]
    );
}

#[test]
fn a_coroutine_that_yields_while_closing_raises_close_ignored() {
    // rule 11: it stays suspended at that yield, no longer closing, and
    // CloseIgnored (E0113) is raised at the closer; a later close runs from
    // the new yield (outside the try: no finally, the signal escapes).
    use Value::{Int, Nil};
    let out = close_scenario(OnClose::CatchYield, |f, o, c| {
        let (r, sig) = (f.reg(D), f.reg(D));
        f.emit(Inst::DLoadInt { dst: sig, val: 1 });
        f.emit(Inst::CoroClose {
            dst: r,
            coro: c,
            src: sig,
        });
        push(f, o, r);
        push_status(f, o, c);
    });
    assert_eq!(out, vec![Int(113), Int(2), Int(1), Nil, Int(4)]);
}

#[test]
fn a_coroutine_that_awaits_while_closing_is_driven_with_resume() {
    // rule 11: an await while closing suspends normally (dst = the
    // awaitable, state awaiting, still closing); `resume` drives it and the
    // outcome rules apply when it next stops: here the signal escapes, so the
    // close succeeds and the resume's dst is nil.
    use Value::{Int, Nil};
    let out = close_scenario(OnClose::CatchAwait, |f, o, c| {
        let (r, nil) = (f.reg(D), f.reg(D));
        f.emit(Inst::Resume {
            dst: r,
            coro: c,
            src: nil,
        });
        push(f, o, r);
        push_status(f, o, c);
    });
    assert_eq!(out, vec![Int(88), Int(3), Int(1), Nil, Int(4)]);
}

#[test]
fn coro_close_of_created_finished_and_running_coroutines() {
    // rule 11: created -> returned without running; returned/failed ->
    // nothing; running -> InvalidCoroState.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let count = m.global("count", I64, true, None);
    let g = gen_closable(&mut m, count, OnClose::Finally);
    // selfclose(): coro_close of itself.
    let mut sc = m.function("selfclose", &[], &[D]);
    let (me, r, sig) = (sc.reg(D), sc.reg(D), sc.reg(D));
    sc.emit(Inst::CoroCurrent { dst: me });
    sc.emit(Inst::CoroClose {
        dst: r,
        coro: me,
        src: sig,
    });
    sc.ret(r);
    let sc = m.add_function(sc).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let (c, r, nil, e) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    f.emit(Inst::CoroClose {
        dst: r,
        coro: c,
        src: nil,
    });
    push(&mut f, o, r); // nil
    push_status(&mut f, o, c); // 4
    f.emit(Inst::CoroResult { dst: r, coro: c });
    push(&mut f, o, r); // nil
    f.emit(Inst::CoroClose {
        dst: r,
        coro: c,
        src: nil,
    });
    push(&mut f, o, r); // nil again
    let cnt = f.reg(I64);
    let cntd = f.reg(D);
    f.emit(Inst::GetGlobal {
        dst: cnt,
        global: count,
    });
    f.emit(Inst::ToDyn {
        dst: cntd,
        src: cnt,
        from: Prim::I64,
    });
    push(&mut f, o, cntd); // 0: the body never ran
    f.emit(Inst::CoroNew {
        dst: c,
        func: sc,
        argc: 0,
    });
    let (s, end, h, after) = (f.label(), f.label(), f.label(), f.label());
    f.bind(s);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    f.bind(end);
    f.jmp(after);
    f.bind(h);
    let k = f.reg(ValType::U32);
    let kd = f.reg(D);
    f.emit(Inst::ErrCode { dst: k, src: e });
    f.emit(Inst::ToDyn {
        dst: kd,
        src: k,
        from: Prim::U32,
    });
    push(&mut f, o, kd); // 110, escaping the body
    f.bind(after);
    f.try_region(s, end, h, e);
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    use Value::{Int, Nil};
    assert_eq!(ints(&vm, v), vec![Nil, Int(4), Nil, Nil, Int(0), Int(110)]);
}

/// `gen()`: `try { return 5 } finally { x = yield 1; <after> }` in the
/// canonical lowering: the `finally` yields with a pending `return`.
fn gen_yield_in_finally(m: &mut ModuleBuilder, override_return: bool) -> FuncId {
    let mut g = m.function("gen", &[], &[D]);
    let kind = g.reg(ValType::I8);
    let (val, e, s) = (g.reg(D), g.reg(D), g.reg(D));
    let (start, end, h, fin) = (g.label(), g.label(), g.label(), g.label());
    g.bind(start);
    g.emit(Inst::DLoadInt { dst: val, val: 5 });
    g.emit(Inst::LoadInt {
        dst: kind,
        val: 1,
        ty: IntTy::I8,
    });
    g.jmp(fin);
    g.bind(end);
    g.bind(h);
    g.emit(Inst::Mov { dst: val, src: e });
    g.emit(Inst::LoadInt {
        dst: kind,
        val: 2,
        ty: IntTy::I8,
    });
    g.bind(fin);
    let one = dint(&mut g, 1);
    g.emit(Inst::Yield { dst: s, src: one });
    if override_return {
        g.ret(s);
    } else {
        // switch kind: 1 -> return val, 2 -> throw val
        let (l_ret, l_throw, l_norm) = (g.label(), g.label(), g.label());
        let _ = g.switch(IntTy::I8, kind, &[l_norm, l_ret, l_throw], l_norm);
        g.bind(l_ret);
        g.ret(val);
        g.bind(l_throw);
        g.emit(Inst::Throw { src: val });
        g.bind(l_norm);
        let z = dint(&mut g, 0);
        g.ret(z);
    }
    g.try_region(start, end, h, e);
    m.add_function(g).unwrap()
}

#[test]
fn finally_with_a_suspension_keeps_its_pending_completion() {
    // rule 12 and §4.3 rule 4: the completion registers survive the
    // suspension; resumed, the finally delivers the pending return.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let g = gen_yield_in_finally(&mut m, false);
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let (c, r, nil, e) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    push(&mut f, o, r); // 1
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    push(&mut f, o, r); // 5: the pending return
    // A resume_throw at the yield in finally replaces the completion.
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    let (s, end, h, after) = (f.label(), f.label(), f.label(), f.label());
    let t = dint(&mut f, 31);
    f.bind(s);
    f.emit(Inst::ResumeThrow {
        dst: r,
        coro: c,
        src: t,
    });
    f.bind(end);
    f.jmp(after);
    f.bind(h);
    push(&mut f, o, e); // 31: replaced the pending return
    f.bind(after);
    f.try_region(s, end, h, e);
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    assert_eq!(
        ints(&vm, v),
        vec![Value::Int(1), Value::Int(5), Value::Int(31)]
    );
}

#[test]
fn a_return_after_the_yield_in_finally_overrides_the_pending_completion() {
    // §4.3 rule 3 across a suspension: `return x` in finally discards the
    // pending return.
    let mut m = ModuleBuilder::new();
    let g = gen_yield_in_finally(&mut m, true);
    let mut f = m.function("main", &[], &[D]);
    let (c, r, nil) = (f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: nil,
    });
    let sent = dint(&mut f, 123);
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: sent,
    });
    f.ret(r);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    assert_eq!(Vm::new(&p).run(main, &[]), Ok(Value::Int(123)));
}

/// `gen(n)`: yields `"k<i>" => i * i` for i in 0..n, returns "end".
fn gen_squares(m: &mut ModuleBuilder, keyed: bool) -> FuncId {
    let mut g = m.function("gen", &[D], &[D]);
    let (i, sq, s, more, one) = (g.reg(D), g.reg(D), g.reg(D), g.reg(ValType::Bool), g.reg(D));
    g.emit(Inst::DLoadInt { dst: one, val: 1 });
    g.emit(Inst::DLoadInt { dst: i, val: 0 });
    let (top, done) = (g.label(), g.label());
    g.bind(top);
    g.emit(Inst::DLt {
        dst: more,
        lhs: i,
        rhs: g.param(0),
    });
    g.jmp_if_not(more, done);
    g.emit(Inst::DMul {
        dst: sq,
        lhs: i,
        rhs: i,
        pol: pol(),
    });
    if keyed {
        let ten = dint(&mut g, 1000);
        let k = g.reg(D);
        g.emit(Inst::DAdd {
            dst: k,
            lhs: i,
            rhs: ten,
            pol: pol(),
        });
        g.emit(Inst::YieldKv {
            dst: s,
            key: k,
            src: sq,
        });
    } else {
        g.emit(Inst::Yield { dst: s, src: sq });
    }
    g.emit(Inst::DAdd {
        dst: i,
        lhs: i,
        rhs: one,
        pol: pol(),
    });
    g.jmp(top);
    g.bind(done);
    let k = m.constant(bytecode_lang::Const::Bytes(b"end".to_vec()));
    let r = g.reg(D);
    g.emit(Inst::DLoadConst { dst: r, k });
    g.ret(r);
    m.add_function(g).unwrap()
}

#[test]
fn generators_iterate_with_iter_new_and_diter_new() {
    // rule 7: iter_new and diter_new over a coroutine; each iter_next
    // resumes it with nil; iter_key reads its key; `returned` ends the loop
    // and the return value stays readable with coro_result.
    for typed in [false, true] {
        let mut m = ModuleBuilder::new();
        let at = m.add_type(TypeDef::Array(D));
        let it_t = m.add_type(TypeDef::Iter { key: D, value: D });
        let g = gen_squares(&mut m, true);
        let mut f = m.function("main", &[], &[D]);
        let o = out(&mut f, at);
        let w = window(&mut f, 1);
        f.emit(Inst::DLoadInt {
            dst: arg(w, 0),
            val: 4,
        });
        f.emit(Inst::CoroNew {
            dst: w,
            func: g,
            argc: 1,
        });
        let (it, v, k, has) = (
            f.reg(ValType::Ref(it_t)),
            f.reg(D),
            f.reg(D),
            f.reg(ValType::Bool),
        );
        if typed {
            f.emit(Inst::IterNew { dst: it, src: w });
        } else {
            f.emit(Inst::DIterNew { dst: it, src: w });
        }
        let (top, done) = (f.label(), f.label());
        f.bind(top);
        f.emit(Inst::IterNext {
            has,
            iter: it,
            val: v,
        });
        f.jmp_if_not(has, done);
        f.emit(Inst::IterKey { dst: k, iter: it });
        push(&mut f, o, k);
        push(&mut f, o, v);
        f.emit(Inst::Safepoint {});
        f.jmp(top);
        f.bind(done);
        // Exhausted: stays exhausted.
        f.emit(Inst::IterNext {
            has,
            iter: it,
            val: v,
        });
        let hd = f.reg(D);
        f.emit(Inst::ToDyn {
            dst: hd,
            src: has,
            from: Prim::Bool,
        });
        push(&mut f, o, hd);
        f.emit(Inst::CoroResult { dst: v, coro: w });
        push(&mut f, o, v);
        f.ret(o);
        let main = m.add_function(f).unwrap();
        let p = load(m);
        let mut vm = Vm::new(&p);
        let out = vm.run(main, &[]).unwrap();
        let got = ints(&vm, out);
        use Value::{Bool, Int};
        assert_eq!(
            got[..9].to_vec(),
            vec![
                Int(1000),
                Int(0),
                Int(1001),
                Int(1),
                Int(1002),
                Int(4),
                Int(1003),
                Int(9),
                Bool(false)
            ],
            "typed: {typed}"
        );
        assert_eq!(vm.str_bytes(got[9]), Some(&b"end"[..]));
    }
}

#[test]
fn iterating_an_awaiting_or_failing_coroutine_raises_at_iter_next() {
    // rule 7: awaiting -> TypeError; a failure propagates to iter_next;
    // iter_key before the first element is IndexOutOfBounds; the `iter`
    // hook may return a coroutine.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let mut aw = m.function("aw", &[], &[]);
    let (s, x) = (aw.reg(D), aw.reg(D));
    aw.emit(Inst::Await { dst: s, src: x });
    aw.ret_void();
    let aw = m.add_function(aw).unwrap();
    let mut bad = m.function("bad", &[], &[]);
    let t = dint(&mut bad, 9);
    bad.emit(Inst::Throw { src: t });
    let bad = m.add_function(bad).unwrap();
    // The iter hook: wraps anything in a generator of it (yields it once).
    let mut once = m.function("once", &[D], &[]);
    let s2 = once.reg(D);
    once.emit(Inst::Yield {
        dst: s2,
        src: once.param(0),
    });
    once.ret_void();
    let once = m.add_function(once).unwrap();
    let mut ih = m.function("iter_hook", &[D], &[D]);
    let w = window(&mut ih, 1);
    ih.mov(arg(w, 0), ih.param(0));
    ih.emit(Inst::CoroNew {
        dst: w,
        func: once,
        argc: 1,
    });
    ih.ret(w);
    let ih = m.add_function(ih).unwrap();
    m.hook(Hook::Iter, Callee::Func(ih));
    let mut f = m.function("main", &[], &[D]);
    let o = out(&mut f, at);
    let (c, it, v, has, e) = (f.reg(D), f.reg(D), f.reg(D), f.reg(ValType::Bool), f.reg(D));
    let code = f.reg(ValType::U32);
    let cd = f.reg(D);
    let record = |f: &mut FunctionBuilder, inst: Inst| {
        let (s, end, h, after) = (f.label(), f.label(), f.label(), f.label());
        f.bind(s);
        f.emit(inst);
        f.bind(end);
        let ok = dint(f, -1);
        push(f, o, ok);
        f.jmp(after);
        f.bind(h);
        f.emit(Inst::ErrCode { dst: code, src: e });
        f.emit(Inst::ToDyn {
            dst: cd,
            src: code,
            from: Prim::U32,
        });
        push(f, o, cd);
        f.bind(after);
        f.try_region(s, end, h, e);
    };
    for body in [aw, bad] {
        f.emit(Inst::CoroNew {
            dst: c,
            func: body,
            argc: 0,
        });
        f.emit(Inst::DIterNew { dst: it, src: c });
        record(&mut f, Inst::IterKey { dst: v, iter: it }); // 102
        record(
            &mut f,
            Inst::IterNext {
                has,
                iter: it,
                val: v,
            },
        ); // aw: 100 (TypeError), bad: 0 (thrown int: no code)
        record(
            &mut f,
            Inst::IterNext {
                has,
                iter: it,
                val: v,
            },
        ); // aw: still awaiting, 100; bad: failed, 110
    }
    // The iter hook made a generator out of 42.
    let x = dint(&mut f, 42);
    f.emit(Inst::DIterNew { dst: it, src: x });
    f.emit(Inst::IterNext {
        has,
        iter: it,
        val: v,
    });
    push(&mut f, o, v);
    f.ret(o);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    let out = vm.run(main, &[]).unwrap();
    use Value::Int;
    assert_eq!(
        ints(&vm, out),
        vec![
            Int(102),
            Int(100),
            Int(100),
            Int(102),
            Int(0),
            Int(110),
            Int(42)
        ]
    );
}

#[test]
fn op_spawn_without_a_hook_is_no_scheduler_and_with_a_function_hook_calls_it() {
    // spawn (rule 8): NoScheduler without the hook; a scheduler written in
    // bytecode receives the coroutine and its result is the task handle.
    let mut m = ModuleBuilder::new();
    let mut body = m.function("body", &[D], &[D]);
    body.ret(body.param(0));
    let body = m.add_function(body).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let (fv, w) = (f.reg(D), window(&mut f, 1));
    f.emit(Inst::MakeClosure {
        dst: fv,
        func: body,
    });
    f.emit(Inst::Spawn {
        dst: w,
        callee: fv,
        argc: 1,
    });
    f.ret(w);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    assert_eq!(
        Vm::new(&p).run(main, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::NoScheduler,
            payload: bvm_lang::Value::Nil,
            func: main,
            pc: 1
        })
    );

    // A bytecode scheduler: runs the task to completion at once and returns
    // its result as the "handle".
    let mut m = ModuleBuilder::new();
    let mut body = m.function("body", &[D], &[D]);
    let (s, t) = (body.reg(D), body.reg(D));
    body.emit(Inst::Yield {
        dst: s,
        src: body.param(0),
    });
    body.emit(Inst::DAdd {
        dst: t,
        lhs: s,
        rhs: body.param(0),
        pol: pol(),
    });
    body.ret(t);
    let body = m.add_function(body).unwrap();
    let mut sched = m.function("sched", &[D], &[D]);
    let (r, r2) = (sched.reg(D), sched.reg(D));
    sched.emit(Inst::Resume {
        dst: r,
        coro: sched.param(0),
        src: r,
    });
    sched.emit(Inst::Resume {
        dst: r2,
        coro: sched.param(0),
        src: r,
    });
    sched.ret(r2);
    let sched = m.add_function(sched).unwrap();
    m.hook(Hook::Spawn, Callee::Func(sched));
    let mut f = m.function("main", &[], &[D]);
    let (fv, w) = (f.reg(D), window(&mut f, 1));
    f.emit(Inst::MakeClosure {
        dst: fv,
        func: body,
    });
    f.emit(Inst::DLoadInt {
        dst: arg(w, 0),
        val: 20,
    });
    f.emit(Inst::Spawn {
        dst: w,
        callee: fv,
        argc: 1,
    });
    f.ret(w);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    assert_eq!(Vm::new(&p).run(main, &[]), Ok(Value::Int(40)));
}

#[test]
fn coroutines_have_kind_coroutine_cast_and_cannot_be_duplicated() {
    // type_of 13, is_kind, cast to `ref coroutine`, dup -> TypeError.
    let mut m = ModuleBuilder::new();
    let ct = m.add_type(TypeDef::Coroutine);
    let mut body = m.function("body", &[], &[]);
    body.ret_void();
    let body = m.add_function(body).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let (c, k, is, typed, d) = (
        f.reg(D),
        f.reg(ValType::U8),
        f.reg(ValType::Bool),
        f.reg(ValType::Ref(ct)),
        f.reg(D),
    );
    f.emit(Inst::CoroNew {
        dst: c,
        func: body,
        argc: 0,
    });
    f.emit(Inst::TypeOf { dst: k, src: c });
    f.emit(Inst::IsKind {
        dst: is,
        src: c,
        kind: Kind::Coroutine,
    });
    let tr = f.type_ref(ct);
    f.emit(Inst::Cast {
        dst: typed,
        src: c,
        ty: tr,
    });
    f.emit(Inst::Dup { dst: d, src: typed });
    f.ret(d);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    assert_eq!(
        Vm::new(&p).run(main, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            payload: bvm_lang::Value::Nil,
            func: main,
            pc: 4
        })
    );
}

#[test]
fn every_coroutine_instruction_charges_one_unit_of_fuel() {
    // LSB §5.14: each coroutine instruction costs one unit; returns and the
    // delivery of a suspension are free.
    let mut m = ModuleBuilder::new();
    let g = gen_echo(&mut m);
    let mut f = m.function("main", &[], &[D]);
    let w = window(&mut f, 1);
    let (r, k, s) = (f.reg(D), f.reg(D), f.reg(ValType::U8));
    f.emit(Inst::DLoadInt {
        dst: arg(w, 0),
        val: 5,
    }); // pc 0
    f.emit(Inst::CoroNew {
        dst: w,
        func: g,
        argc: 1,
    }); // pc 1: unit 1
    f.emit(Inst::Resume {
        dst: r,
        coro: w,
        src: r,
    }); // pc 2: unit 2, then the body's first yield: unit 3
    f.emit(Inst::CoroStatus { dst: s, coro: w }); // pc 3: 4
    f.emit(Inst::CoroKey { dst: k, coro: w }); // pc 4: 5
    f.emit(Inst::CoroCurrent { dst: k }); // pc 5: 6
    f.emit(Inst::Resume {
        dst: r,
        coro: w,
        src: r,
    }); // pc 6: 7, second yield: 8
    f.emit(Inst::Resume {
        dst: r,
        coro: w,
        src: r,
    }); // pc 7: 9; the body returns (free)
    f.emit(Inst::CoroResult { dst: r, coro: w }); // pc 8: 10
    f.emit(Inst::CoroClose {
        dst: k,
        coro: w,
        src: k,
    }); // pc 9: 11
    f.ret(r);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    assert_eq!(vm.run(main, &[]), Ok(Value::Int(60)));
    assert_eq!(vm.fuel_used(), 11);
    // With 8 units the 9th charge traps at main's third resume.
    let err = Vm::new(&p)
        .run_with(main, &[], Limits::new().with_fuel(8))
        .unwrap_err();
    assert_eq!(
        err,
        VmError::Trap {
            kind: ErrorKind::OutOfFuel,
            func: main,
            pc: 7
        }
    );
    // With 7 the 8th charge traps at the generator's second yield.
    let err = Vm::new(&p)
        .run_with(main, &[], Limits::new().with_fuel(7))
        .unwrap_err();
    assert_eq!(
        err,
        VmError::Trap {
            kind: ErrorKind::OutOfFuel,
            func: g,
            pc: 3
        }
    );
}

#[test]
fn coroutine_frames_count_toward_the_depth_limit_when_resumed() {
    // §5.13 Limits: a suspended stack deeper than the room left raises
    // StackOverflow at the resume (catchable), not inside the coroutine.
    let mut m = ModuleBuilder::new();
    // deep(n): if n == 0 { yield 0 } else { deep(n - 1) }
    let mut deep = m.function("deep", &[I64], &[]);
    let me = deep.id();
    let (z, is0, s, one) = (
        deep.reg(I64),
        deep.reg(ValType::Bool),
        deep.reg(D),
        deep.reg(I64),
    );
    let w = deep.regs(&[D, I64]);
    let rec = deep.label();
    deep.emit(Inst::IEq {
        dst: is0,
        lhs: deep.param(0),
        rhs: z,
        ty: IntTy::I64,
    });
    deep.jmp_if_not(is0, rec);
    deep.emit(Inst::Yield { dst: s, src: s });
    deep.ret_void();
    deep.bind(rec);
    deep.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    deep.emit(Inst::ISub {
        dst: Reg(w.0 + 1),
        lhs: deep.param(0),
        rhs: one,
        op: IntOp::new(IntTy::I64),
    });
    deep.emit(Inst::Call {
        dst: w,
        func: me,
        argc: 1,
    });
    deep.ret_void();
    let deep = m.add_function(deep).unwrap();
    // nest(k, c): recurse k more frames, then resume c, returning 1 or the
    // code of the error the resume raised.
    let mut nest = m.function("nest", &[I64, D], &[D]);
    let me = nest.id();
    let (z, is0, one, r, e) = (
        nest.reg(I64),
        nest.reg(ValType::Bool),
        nest.reg(I64),
        nest.reg(D),
        nest.reg(D),
    );
    let w = nest.regs(&[D, I64, D]);
    let rec = nest.label();
    nest.emit(Inst::IEq {
        dst: is0,
        lhs: nest.param(0),
        rhs: z,
        ty: IntTy::I64,
    });
    nest.jmp_if_not(is0, rec);
    let (s, end, h) = (nest.label(), nest.label(), nest.label());
    nest.bind(s);
    nest.emit(Inst::Resume {
        dst: r,
        coro: nest.param(1),
        src: r,
    });
    nest.bind(end);
    let ok = dint(&mut nest, 1);
    nest.ret(ok);
    nest.bind(h);
    let code = nest.reg(ValType::U32);
    let cd = nest.reg(D);
    nest.emit(Inst::ErrCode { dst: code, src: e });
    nest.emit(Inst::ToDyn {
        dst: cd,
        src: code,
        from: Prim::U32,
    });
    nest.ret(cd);
    nest.try_region(s, end, h, e);
    nest.bind(rec);
    nest.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    nest.emit(Inst::ISub {
        dst: Reg(w.0 + 1),
        lhs: nest.param(0),
        rhs: one,
        op: IntOp::new(IntTy::I64),
    });
    nest.mov(Reg(w.0 + 2), nest.param(1));
    nest.emit(Inst::Call {
        dst: w,
        func: me,
        argc: 2,
    });
    nest.ret(w);
    let nest = m.add_function(nest).unwrap();
    // main(k): c = deep(30) suspended 31 frames deep; return nest(k, c).
    let mut f = m.function("main", &[I64], &[D]);
    let w = f.regs(&[D, I64]);
    f.emit(Inst::LoadInt {
        dst: Reg(w.0 + 1),
        val: 30,
        ty: IntTy::I64,
    });
    f.emit(Inst::CoroNew {
        dst: w,
        func: deep,
        argc: 1,
    });
    let r = f.reg(D);
    f.emit(Inst::Resume {
        dst: r,
        coro: w,
        src: r,
    });
    let n = f.regs(&[D, I64, D]);
    f.mov(Reg(n.0 + 1), f.param(0));
    f.mov(Reg(n.0 + 2), w);
    f.emit(Inst::Call {
        dst: n,
        func: nest,
        argc: 2,
    });
    f.ret(n);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let limits = Limits::new().with_depth(40);
    // main + nest(0) + 31 suspended frames = 33: fits.
    assert_eq!(
        Vm::new(&p).run_with(main, &[Value::Int(0)], limits),
        Ok(Value::Int(1))
    );
    // main + nest(0..=9) = 11, + 31 = 42 > 40: StackOverflow at the resume,
    // caught by the try around it.
    assert_eq!(
        Vm::new(&p).run_with(main, &[Value::Int(9)], limits),
        Ok(Value::Int(105))
    );
}

#[test]
fn a_trap_inside_a_coroutine_aborts_the_run_and_fails_it() {
    // Traps abort the whole run (rule 3); the coroutine, whose frames are
    // gone, is `failed` afterwards.
    let mut m = ModuleBuilder::new();
    let g_c = m.global("c", D, true, None);
    let mut body = m.function("body", &[], &[]);
    body.emit(Inst::Unreachable {});
    let body = m.add_function(body).unwrap();
    let mut f = m.function("main", &[], &[]);
    let (c, r) = (f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: body,
        argc: 0,
    });
    f.emit(Inst::SetGlobal {
        global: g_c,
        src: c,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: r,
    });
    f.ret_void();
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    assert_eq!(
        vm.run(main, &[]),
        Err(VmError::Trap {
            kind: ErrorKind::Unreachable,
            func: body,
            pc: 0
        })
    );
    let c = vm.global(g_c).unwrap();
    assert_eq!(vm.coro_state(c), Some(CoroState::Failed));
    assert_eq!(vm.kind(c), Kind::Coroutine);
}

#[test]
fn tail_calls_in_a_coroutine_body_finish_it() {
    // rule 3: "a tail call that returns".
    let mut m = ModuleBuilder::new();
    let mut leaf = m.function("leaf", &[D], &[D]);
    let s = leaf.reg(D);
    leaf.emit(Inst::Yield {
        dst: s,
        src: leaf.param(0),
    });
    leaf.ret(s);
    let leaf = m.add_function(leaf).unwrap();
    let mut body = m.function("body", &[D], &[D]);
    body.emit(Inst::TailCall {
        func: leaf,
        args: body.param(0),
        argc: 1,
    });
    let body = m.add_function(body).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let w = window(&mut f, 1);
    f.emit(Inst::DLoadInt {
        dst: arg(w, 0),
        val: 3,
    });
    f.emit(Inst::CoroNew {
        dst: w,
        func: body,
        argc: 1,
    });
    let (r, x) = (f.reg(D), f.reg(D));
    f.emit(Inst::Resume {
        dst: r,
        coro: w,
        src: x,
    });
    let sent = dint(&mut f, 8);
    f.emit(Inst::Resume {
        dst: r,
        coro: w,
        src: sent,
    });
    f.emit(Inst::DAdd {
        dst: r,
        lhs: r,
        rhs: r,
        pol: pol(),
    });
    f.ret(r);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    assert_eq!(Vm::new(&p).run(main, &[]), Ok(Value::Int(16)));
}

#[test]
fn a_scheduler_import_must_have_the_spawn_signature() {
    let mut host = Host::new();
    host.register_scheduler("ls", "spawn");
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D, D], &[D]);
    let _ = m.import("ls", "spawn", sig);
    let err = Program::load(m.finish().unwrap(), &host).unwrap_err();
    assert_eq!(err.kind(), &LoadErrorKind::BadSignature);
}
