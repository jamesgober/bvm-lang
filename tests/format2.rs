//! LSB format 2 conformance beyond the PHP programs: loading (format 1
//! refused, parameter lists and call shapes checked), the `call_shape` and
//! `call` hooks of non-callable callees, the fuel point of a dynamic call,
//! `dparam_ref` edge cases, coroutine binding, error payloads, the reference
//! and separation instructions' error cases and typed corners, and the
//! collector keeping references and payloads alive.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use bvm_lang::{Host, Limits, LoadErrorKind, Program, Value, Vm, VmError};
use bytecode_lang::{
    ArgKind, Callee, DecodeErrorKind, ErrorKind, Field, FieldIdx, Hook, Inst, IntTy, ModuleBuilder,
    Param, ParamKind, ParamList, Policy, Prim, Reg, StrId, StructDef, TypeDef, ValType,
};

const D: ValType = ValType::Dyn;

fn load(m: ModuleBuilder, host: &Host) -> Program {
    Program::load(m.finish().unwrap(), host).unwrap()
}

fn load_err(m: ModuleBuilder) -> LoadErrorKind {
    Program::load(m.finish().unwrap(), &Host::new())
        .unwrap_err()
        .kind()
        .clone()
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[test]
fn format_1_bytes_are_refused_and_format_2_round_trips() {
    let mut m = ModuleBuilder::new();
    let a = m.string("a");
    let mut f = m.function("f", &[D, D], &[D]);
    f.set_params(ParamList::new(vec![
        Param::normal(a),
        Param::new(ParamKind::RestNamed, None).by_ref(),
    ]));
    let w = f.regs(&[D, D]);
    f.dcall_shape(w, Reg(0), &[ArgKind::Named(a)]);
    f.ret(w);
    m.add_function(f).unwrap();
    let module = m.finish().unwrap();
    let bytes = bytecode_lang::encode(&module);
    assert_eq!(&bytes[4..8], &2u32.to_le_bytes());
    assert!(Program::decode(&bytes, &Host::new()).is_ok());
    let mut old = bytes.clone();
    old[4..8].copy_from_slice(&1u32.to_le_bytes());
    let err = Program::decode(&old, &Host::new()).unwrap_err();
    match err.kind() {
        LoadErrorKind::Decode(e) => assert_eq!(e.kind(), &DecodeErrorKind::UnsupportedVersion(1)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_by_reference_parameter_must_be_dyn_or_a_cell_of_dyn() {
    let mut m = ModuleBuilder::new();
    let a = m.string("a");
    let arr = m.add_type(TypeDef::Array(D));
    let mut f = m.function("f", &[ValType::Ref(arr)], &[]);
    f.set_params(ParamList::new(vec![Param::normal(a).by_ref()]));
    f.ret_void();
    m.add_function(f).unwrap();
    assert_eq!(load_err(m), LoadErrorKind::BadParamList);

    // A `ref cell dyn` by-reference parameter loads and receives the
    // reference itself.
    let mut m = ModuleBuilder::new();
    let a = m.string("a");
    let cell = m.add_type(TypeDef::Cell(D));
    let mut f = m.function("f", &[ValType::Ref(cell)], &[D]);
    f.set_params(ParamList::new(vec![Param::normal(a).by_ref()]));
    let v = f.reg(D);
    f.emit(Inst::DLoadInt { dst: v, val: 9 });
    f.emit(Inst::CellSet {
        cell: Reg(0),
        src: v,
    });
    f.ret(v);
    let callee = m.add_function(f).unwrap();
    let mut main = m.function("main", &[], &[D]);
    let (fv, x, r) = (main.reg(D), main.reg(D), main.reg(D));
    let w = main.regs(&[D, D]);
    main.emit(Inst::MakeClosure {
        dst: fv,
        func: callee,
    });
    main.emit(Inst::DLoadInt { dst: x, val: 1 });
    main.emit(Inst::NewRef { dst: r, src: x });
    main.mov(Reg(w.0 + 1), r);
    main.emit(Inst::DCall {
        dst: w,
        callee: fv,
        argc: 1,
    });
    main.emit(Inst::CellGet { dst: x, cell: r });
    main.ret(x);
    let main = m.add_function(main).unwrap();
    let p = load(m, &Host::new());
    assert_eq!(Vm::new(&p).run(main, &[]), Ok(Value::Int(9)));
}

#[test]
fn names_out_of_the_string_table_are_refused() {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[D], &[]);
    f.set_params(ParamList::new(vec![Param::normal(StrId(77))]));
    f.ret_void();
    m.add_function(f).unwrap();
    assert_eq!(load_err(m), LoadErrorKind::BadParamList);

    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[D], &[]);
    let w = f.regs(&[D, D]);
    f.dcall_shape(w, Reg(0), &[ArgKind::Named(StrId(77))]);
    f.ret_void();
    m.add_function(f).unwrap();
    assert_eq!(load_err(m), LoadErrorKind::BadCallShape);
}

#[test]
fn a_call_shape_window_must_fit_the_frame_and_raise_must_be_catchable() {
    let mut m = ModuleBuilder::new();
    let a = m.string("a");
    let mut f = m.function("f", &[D], &[]);
    let w = f.reg(D);
    // Two arguments after `w`, but `w` is the last register.
    f.dcall_shape(w, Reg(0), &[ArgKind::Positional, ArgKind::Named(a)]);
    f.ret_void();
    m.add_function(f).unwrap();
    assert!(matches!(
        load_err(m),
        LoadErrorKind::OutOfRange {
            what: "register",
            ..
        }
    ));

    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[D], &[]);
    f.emit(Inst::Raise {
        src: Reg(0),
        kind: ErrorKind::OutOfFuel,
    });
    f.ret_void();
    m.add_function(f).unwrap();
    assert_eq!(load_err(m), LoadErrorKind::BadModifier);
}

#[test]
fn the_new_hooks_are_checked_at_load() {
    // pow (2 operands), abs (1), call_shape (3): a hook with another shape
    // is refused.
    for (hook, n) in [(Hook::Pow, 1), (Hook::Abs, 2), (Hook::CallShape, 2)] {
        let mut m = ModuleBuilder::new();
        let mut h = m.function("h", &vec![D; n], &[D]);
        h.ret(Reg(0));
        let h = m.add_function(h).unwrap();
        m.hook(hook, Callee::Func(h));
        assert_eq!(load_err(m), LoadErrorKind::BadHook(hook), "{hook}");
    }
}

// ---------------------------------------------------------------------------
// Non-callables and hooks
// ---------------------------------------------------------------------------

/// `main` calls the non-callable `7` with `(1, b: 2)` (named) or `(1, 2)`.
fn call_non_callable(named: bool, shape_hook: bool, call_hook: bool) -> Result<String, ErrorKind> {
    let mut m = ModuleBuilder::new();
    let b = m.string("b");
    // call_shape(callee, positional, named) => [callee, positional, named]
    let at = m.add_type(TypeDef::Array(D));
    let mut hs = m.function("call_shape", &[D, D, D], &[D]);
    let (n, out) = (hs.reg(ValType::I64), hs.reg(D));
    let ty = hs.type_ref(at);
    hs.emit(Inst::LoadInt {
        dst: n,
        val: 0,
        ty: IntTy::I64,
    });
    hs.emit(Inst::NewArray {
        dst: out,
        len: n,
        ty,
    });
    for r in 0..3 {
        hs.emit(Inst::ArrayPush {
            arr: out,
            src: Reg(r),
        });
    }
    hs.ret(out);
    let hs = m.add_function(hs).unwrap();
    let mut hc = m.function("call", &[D, D], &[D]);
    hc.ret(Reg(1));
    let hc = m.add_function(hc).unwrap();
    if shape_hook {
        m.hook(Hook::Call, Callee::Func(hc));
    }
    if call_hook {
        m.hook(Hook::CallShape, Callee::Func(hs));
    }
    let mut main = m.function("main", &[], &[D]);
    let callee = main.reg(D);
    let w = main.regs(&[D, D, D]);
    main.emit(Inst::DLoadInt {
        dst: callee,
        val: 7,
    });
    main.emit(Inst::DLoadInt {
        dst: Reg(w.0 + 1),
        val: 1,
    });
    main.emit(Inst::DLoadInt {
        dst: Reg(w.0 + 2),
        val: 2,
    });
    let second = if named {
        ArgKind::Named(b)
    } else {
        ArgKind::Positional
    };
    main.dcall_shape(w, callee, &[ArgKind::Positional, second]);
    main.ret(w);
    let main = m.add_function(main).unwrap();
    let p = load(m, &Host::new());
    let mut vm = Vm::new(&p);
    match vm.run(main, &[]) {
        Ok(v) => Ok(render(&vm, v)),
        Err(e) => Err(e.kind().unwrap_or(ErrorKind::TypeError)),
    }
}

fn render(vm: &Vm<'_>, v: Value) -> String {
    if let Some(items) = vm.elements(v) {
        let parts: Vec<String> = items.into_iter().map(|x| render(vm, x)).collect();
        return format!("[{}]", parts.join(", "));
    }
    if let Some(entries) = vm.entries(v) {
        let parts: Vec<String> = entries
            .into_iter()
            .map(|(k, x)| format!("{}: {}", render(vm, k), render(vm, x)))
            .collect();
        return format!("{{{}}}", parts.join(", "));
    }
    if let Some(b) = vm.str_bytes(v) {
        return String::from_utf8_lossy(b).into_owned();
    }
    match v {
        Value::Int(i) => i.to_string(),
        Value::Nil => "nil".into(),
        Value::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

#[test]
fn non_callables_go_to_the_call_shape_hook_then_the_call_hook() {
    // Bound `call_shape`: (callee, positional array, named map).
    assert_eq!(
        call_non_callable(true, true, true),
        Ok("[7, [1], {b: 2}]".into())
    );
    assert_eq!(
        call_non_callable(false, true, true),
        Ok("[7, [1, 2], {}]".into())
    );
    // Only `call`: used when nothing is named.
    assert_eq!(call_non_callable(false, true, false), Ok("[1, 2]".into()));
    assert_eq!(
        call_non_callable(true, true, false),
        Err(ErrorKind::TypeError)
    );
    // No hooks.
    assert_eq!(
        call_non_callable(false, false, false),
        Err(ErrorKind::TypeError)
    );
}

#[test]
fn flattening_errors_come_before_the_hooks_and_cost_nothing() {
    // A spread of an int: TypeError; a spread map with a float key:
    // ArgumentError; a named spread with an int key: ArgumentError. With the
    // `call_shape` hook bound, uncharged.
    for (case, want) in [
        (0, ErrorKind::TypeError),
        (1, ErrorKind::ArgumentError),
        (2, ErrorKind::ArgumentError),
    ] {
        let mut m = ModuleBuilder::new();
        let mt = m.add_type(TypeDef::Map { key: D, value: D });
        let k = m.constant(bytecode_lang::Const::f64(1.5));
        let mut hs = m.function("call_shape", &[D, D, D], &[D]);
        hs.ret(Reg(0));
        let hs = m.add_function(hs).unwrap();
        m.hook(Hook::CallShape, Callee::Func(hs));
        let mut main = m.function("main", &[], &[D]);
        let (callee, mp, key) = (main.reg(D), main.reg(D), main.reg(D));
        let w = main.regs(&[D, D]);
        let ty = main.type_ref(mt);
        main.emit(Inst::DLoadInt {
            dst: callee,
            val: 7,
        });
        main.emit(Inst::NewMap { dst: mp, ty });
        let kind = match case {
            0 => {
                main.emit(Inst::DLoadInt {
                    dst: Reg(w.0 + 1),
                    val: 3,
                });
                ArgKind::Spread
            }
            1 => {
                main.emit(Inst::DLoadConst { dst: key, k });
                main.emit(Inst::MapSet {
                    map: mp,
                    key,
                    src: callee,
                });
                main.mov(Reg(w.0 + 1), mp);
                ArgKind::Spread
            }
            _ => {
                main.emit(Inst::MapPush {
                    map: mp,
                    src: callee,
                });
                main.mov(Reg(w.0 + 1), mp);
                ArgKind::SpreadNamed
            }
        };
        main.dcall_shape(w, callee, &[kind]);
        main.ret(w);
        let main = m.add_function(main).unwrap();
        let p = load(m, &Host::new());
        let mut vm = Vm::new(&p);
        let err = vm
            .run_with(main, &[], Limits::new().with_fuel(0))
            .unwrap_err();
        assert_eq!(err.kind(), Some(want), "case {case}");
        assert!(matches!(err, VmError::Raised { .. }), "case {case}: {err}");
    }
}

#[test]
fn a_dynamic_call_is_charged_after_binding() {
    // f($a) called with one argument under zero fuel: OutOfFuel at the
    // dcall (binding succeeded); with two arguments: ArgumentError instead.
    for (argc, trapped) in [(1u8, true), (2, false)] {
        let mut m = ModuleBuilder::new();
        let a = m.string("a");
        let mut f = m.function("f", &[D], &[D]);
        f.set_params(ParamList::new(vec![Param::normal(a)]));
        f.ret(Reg(0));
        let f = m.add_function(f).unwrap();
        let mut main = m.function("main", &[], &[D]);
        let fv = main.reg(D);
        let w = main.regs(&[D, D, D]);
        main.emit(Inst::MakeClosure { dst: fv, func: f });
        main.emit(Inst::DCall {
            dst: w,
            callee: fv,
            argc,
        });
        main.ret(w);
        let main = m.add_function(main).unwrap();
        let p = load(m, &Host::new());
        let err = Vm::new(&p)
            .run_with(main, &[], Limits::new().with_fuel(0))
            .unwrap_err();
        if trapped {
            assert_eq!(
                err,
                VmError::Trap {
                    kind: ErrorKind::OutOfFuel,
                    func: main,
                    pc: 1
                }
            );
        } else {
            assert_eq!(err.kind(), Some(ErrorKind::ArgumentError));
        }
    }
}

// ---------------------------------------------------------------------------
// dparam_ref
// ---------------------------------------------------------------------------

#[test]
fn dparam_ref_answers_for_every_callee_and_position() {
    // f($a, &$b, &...$rest) and g(p, /, q, *, &r, **kw) with kw by reference.
    let mut m = ModuleBuilder::new();
    let (a, b, p_, q, r) = (
        m.string("a"),
        m.string("b"),
        m.string("p"),
        m.string("q"),
        m.string("r"),
    );
    let mut f = m.function("f", &[D, D, D], &[]);
    f.set_params(ParamList::new(vec![
        Param::normal(a),
        Param::normal(b).by_ref(),
        Param::new(ParamKind::Rest, None).by_ref(),
    ]));
    f.ret_void();
    let f = m.add_function(f).unwrap();
    let mut g = m.function("g", &[D, D, D, D], &[]);
    g.set_params(ParamList::new(vec![
        Param::new(ParamKind::PositionalOnly, Some(p_)),
        Param::normal(q),
        Param::new(ParamKind::NamedOnly, Some(r)).by_ref(),
        Param::new(ParamKind::RestNamed, None).by_ref(),
    ]));
    g.ret_void();
    let g = m.add_function(g).unwrap();
    let mut plain = m.function("plain", &[D], &[]);
    plain.ret_void();
    let plain = m.add_function(plain).unwrap();
    let at = m.add_type(TypeDef::Array(D));
    let mut main = m.function("main", &[], &[D]);
    let (out, n, pos, flag, d, fv, gv, pv, nc) = (
        main.reg(D),
        main.reg(ValType::I64),
        main.reg(ValType::I64),
        main.reg(ValType::Bool),
        main.reg(D),
        main.reg(D),
        main.reg(D),
        main.reg(D),
        main.reg(D),
    );
    let ty = main.type_ref(at);
    main.emit(Inst::LoadInt {
        dst: n,
        val: 0,
        ty: IntTy::I64,
    });
    main.emit(Inst::NewArray {
        dst: out,
        len: n,
        ty,
    });
    main.emit(Inst::MakeClosure { dst: fv, func: f });
    main.emit(Inst::MakeClosure { dst: gv, func: g });
    main.emit(Inst::MakeClosure {
        dst: pv,
        func: plain,
    });
    main.emit(Inst::DLoadInt { dst: nc, val: 3 });
    let ask = |main: &mut bytecode_lang::FunctionBuilder,
               callee: Reg,
               at: Option<i32>,
               name: Option<StrId>| {
        match (at, name) {
            (Some(i), _) => {
                main.emit(Inst::LoadInt {
                    dst: pos,
                    val: i,
                    ty: IntTy::I64,
                });
                main.emit(Inst::DParamRef {
                    dst: flag,
                    callee,
                    pos,
                });
            }
            (None, Some(s)) => {
                let nr = main.name_ref(s);
                main.emit(Inst::DParamRefNamed {
                    dst: flag,
                    callee,
                    name: nr,
                });
            }
            _ => {}
        }
        main.emit(Inst::ToDyn {
            dst: d,
            src: flag,
            from: Prim::Bool,
        });
        main.emit(Inst::ArrayPush { arr: out, src: d });
    };
    // f: positions 0 (a), 1 (&b), 2 and 9 (&...rest), -1; names a, b, zz.
    for i in [0, 1, 2, 9, -1] {
        ask(&mut main, fv, Some(i), None);
    }
    for s in [a, b] {
        ask(&mut main, fv, None, Some(s));
    }
    // g: positions 0 (p), 1 (q), 2 (nothing takes it); names p (to **kw),
    // q, r, other (to **kw).
    for i in [0, 1, 2] {
        ask(&mut main, gv, Some(i), None);
    }
    for s in [p_, q, r, a] {
        ask(&mut main, gv, None, Some(s));
    }
    // No parameter list; a non-callable.
    ask(&mut main, pv, Some(0), None);
    ask(&mut main, nc, Some(0), None);
    main.ret(out);
    let main = m.add_function(main).unwrap();
    let p = load(m, &Host::new());
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    let got: Vec<bool> = vm
        .elements(v)
        .unwrap()
        .into_iter()
        .map(|x| x.as_bool().unwrap())
        .collect();
    assert_eq!(
        got,
        [
            false, true, true, true, false, // f positions
            false, true, // f names a, b (b is by reference; a is not)
            false, false, false, // g positions: nothing takes 2
            true, false, true, true, // g names: p -> **kw (by ref), q, r, a -> **kw
            false, false, // no list, non-callable
        ]
    );
}

// ---------------------------------------------------------------------------
// Coroutines bind like dcall
// ---------------------------------------------------------------------------

#[test]
fn coro_new_indirect_binds_a_parameter_list_and_spawn_too() {
    // gen($n, &$out): yields $n, then writes $n * 10 through $out.
    let mut m = ModuleBuilder::new();
    let (n, o) = (m.string("n"), m.string("out"));
    let mut g = m.function("gen", &[D, D], &[D]);
    g.set_params(ParamList::new(vec![
        Param::normal(n),
        Param::normal(o).by_ref(),
    ]));
    let (s, ten) = (g.reg(D), g.reg(D));
    g.emit(Inst::Yield {
        dst: s,
        src: Reg(0),
    });
    g.emit(Inst::DLoadInt { dst: ten, val: 10 });
    g.emit(Inst::DMul {
        dst: s,
        lhs: Reg(0),
        rhs: ten,
        pol: Policy::new(),
    });
    g.emit(Inst::CellSet {
        cell: Reg(1),
        src: s,
    });
    g.ret(s);
    let g = m.add_function(g).unwrap();
    let mut main = m.function("main", &[], &[D]);
    let (gv, x, r, c, nil, y) = (
        main.reg(D),
        main.reg(D),
        main.reg(D),
        main.reg(D),
        main.reg(D),
        main.reg(D),
    );
    let w = main.regs(&[D, D, D]);
    main.emit(Inst::MakeClosure { dst: gv, func: g });
    main.emit(Inst::DLoadInt { dst: x, val: 0 });
    main.emit(Inst::NewRef { dst: r, src: x });
    main.emit(Inst::DLoadInt {
        dst: Reg(w.0 + 1),
        val: 4,
    });
    main.mov(Reg(w.0 + 2), r);
    main.emit(Inst::CoroNewIndirect {
        dst: w,
        callee: gv,
        argc: 2,
    });
    main.mov(c, w);
    main.emit(Inst::Resume {
        dst: y,
        coro: c,
        src: nil,
    });
    main.emit(Inst::Resume {
        dst: y,
        coro: c,
        src: nil,
    });
    main.emit(Inst::CellGet { dst: x, cell: r });
    main.ret(x);
    let main_id = m.add_function(main).unwrap();
    let p = load(m, &Host::new());
    assert_eq!(Vm::new(&p).run(main_id, &[]), Ok(Value::Int(40)));
}

#[test]
fn coro_new_indirect_reports_argument_errors_and_refuses_imports() {
    for case in 0..2 {
        let mut m = ModuleBuilder::new();
        let n = m.string("n");
        let sig = m.func_type(&[D], &[D]);
        let imp = m.import("env", "id", sig);
        let mut g = m.function("gen", &[D], &[D]);
        g.set_params(ParamList::new(vec![Param::normal(n)]));
        g.ret(Reg(0));
        let g = m.add_function(g).unwrap();
        let mut main = m.function("main", &[], &[D]);
        let gv = main.reg(D);
        let w = main.regs(&[D, D, D]);
        if case == 0 {
            main.emit(Inst::MakeClosure { dst: gv, func: g });
            main.emit(Inst::CoroNewIndirect {
                dst: w,
                callee: gv,
                argc: 2,
            });
        } else {
            main.emit(Inst::LoadImport {
                dst: gv,
                import: imp,
            });
            main.emit(Inst::CoroNewIndirect {
                dst: w,
                callee: gv,
                argc: 1,
            });
        }
        main.ret(w);
        let main = m.add_function(main).unwrap();
        let mut host = Host::new();
        host.register("env", "id", |_, a| Ok(a[0]));
        let p = load(m, &host);
        let want = if case == 0 {
            ErrorKind::ArgumentError
        } else {
            ErrorKind::TypeError
        };
        assert_eq!(Vm::new(&p).run(main, &[]).unwrap_err().kind(), Some(want));
    }
}

// ---------------------------------------------------------------------------
// Errors with payloads
// ---------------------------------------------------------------------------

#[test]
fn err_payload_reads_raise_operands_and_is_nil_otherwise() {
    // [payload of raise(TypeError, "x"), payload of a DivByZero, payload of
    //  a non-error, code of the raised one]
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let k = m.constant(bytecode_lang::Const::Bytes(b"x".to_vec()));
    let mut f = m.function("main", &[], &[D]);
    let (out, n, x, e, p, zero, code, cd) = (
        f.reg(D),
        f.reg(ValType::I64),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(ValType::U32),
        f.reg(D),
    );
    let ty = f.type_ref(at);
    f.emit(Inst::LoadInt {
        dst: n,
        val: 0,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewArray {
        dst: out,
        len: n,
        ty,
    });
    f.emit(Inst::DLoadConst { dst: x, k });
    let (s1, e1) = (f.label(), f.label());
    f.bind(s1);
    f.emit(Inst::Raise {
        src: x,
        kind: ErrorKind::TypeError,
    });
    f.bind(e1);
    f.try_region(s1, e1, e1, e);
    f.emit(Inst::ErrPayload { dst: p, src: e });
    f.emit(Inst::ArrayPush { arr: out, src: p });
    f.emit(Inst::ErrCode { dst: code, src: e });
    f.emit(Inst::ToDyn {
        dst: cd,
        src: code,
        from: Prim::U32,
    });
    let (s2, e2) = (f.label(), f.label());
    f.emit(Inst::DLoadInt { dst: zero, val: 0 });
    f.bind(s2);
    f.emit(Inst::DDiv {
        dst: p,
        lhs: x,
        rhs: zero,
        pol: Policy::new(),
    });
    f.bind(e2);
    f.try_region(s2, e2, e2, e);
    f.emit(Inst::ErrPayload { dst: p, src: e });
    f.emit(Inst::ArrayPush { arr: out, src: p });
    f.emit(Inst::ErrPayload { dst: p, src: x });
    f.emit(Inst::ArrayPush { arr: out, src: p });
    f.emit(Inst::ArrayPush { arr: out, src: cd });
    f.ret(out);
    let main = m.add_function(f).unwrap();
    let p = load(m, &Host::new());
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    // `x / 0` with a string is the div hook's case (TypeError), whose
    // payload is nil too.
    assert_eq!(render(&vm, v), "[x, nil, nil, 100]");
}

#[test]
fn references_and_payloads_survive_collections() {
    // r = new_ref([1, 2]); e = error(raise payload [3]); then a megabyte of
    // garbage strings (collections run), then read both back.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let s = m.constant(bytecode_lang::Const::Bytes(vec![b'z'; 4096]));
    let mut f = m.function("main", &[], &[D]);
    let (n, a, r, e, i, lim, one, cond, tmp, b, out) = (
        f.reg(ValType::I64),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(ValType::I64),
        f.reg(ValType::I64),
        f.reg(ValType::I64),
        f.reg(ValType::Bool),
        f.reg(D),
        f.reg(D),
        f.reg(D),
    );
    let ty = f.type_ref(at);
    f.emit(Inst::LoadInt {
        dst: n,
        val: 0,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewArray { dst: a, len: n, ty });
    f.emit(Inst::DLoadInt { dst: tmp, val: 1 });
    f.emit(Inst::ArrayPush { arr: a, src: tmp });
    f.emit(Inst::DLoadInt { dst: tmp, val: 2 });
    f.emit(Inst::ArrayPush { arr: a, src: tmp });
    f.emit(Inst::NewRef { dst: r, src: a });
    f.emit(Inst::NewArray { dst: b, len: n, ty });
    f.emit(Inst::DLoadInt { dst: tmp, val: 3 });
    f.emit(Inst::ArrayPush { arr: b, src: tmp });
    let (s1, e1) = (f.label(), f.label());
    f.bind(s1);
    f.emit(Inst::Raise {
        src: b,
        kind: ErrorKind::NoMatch,
    });
    f.bind(e1);
    f.try_region(s1, e1, e1, e);
    f.emit(Inst::LoadNil { dst: a });
    f.emit(Inst::LoadNil { dst: b });
    f.emit(Inst::LoadInt {
        dst: i,
        val: 0,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: lim,
        val: 2000,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::ILt {
        dst: cond,
        lhs: i,
        rhs: lim,
        ty: IntTy::I64,
    });
    f.jmp_if_not(cond, done);
    f.emit(Inst::DLoadConst { dst: tmp, k: s });
    f.emit(Inst::DConcat {
        dst: tmp,
        lhs: tmp,
        rhs: tmp,
    });
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: bytecode_lang::IntOp::new(IntTy::I64),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    f.emit(Inst::NewArray {
        dst: out,
        len: n,
        ty,
    });
    f.emit(Inst::CellGet { dst: tmp, cell: r });
    f.emit(Inst::ArrayPush { arr: out, src: tmp });
    f.emit(Inst::ErrPayload { dst: tmp, src: e });
    f.emit(Inst::ArrayPush { arr: out, src: tmp });
    f.ret(out);
    let main = m.add_function(f).unwrap();
    let p = load(m, &Host::new());
    let mut vm = Vm::new(&p);
    let v = vm.run(main, &[]).unwrap();
    assert!(vm.collections() > 0, "the loop must have collected");
    assert_eq!(render(&vm, v), "[[1, 2], [3]]");
}

// ---------------------------------------------------------------------------
// References and separation: errors and typed corners
// ---------------------------------------------------------------------------

/// Runs one instruction sequence in a fresh function with a dyn array `r0`
/// (`[10]`), a dyn map `r1` (`{"k": 20}`), and a struct `r2` with fields
/// `d: dyn` and `i: i64`; returns the outcome as text.
fn corner(
    emit: impl FnOnce(&mut bytecode_lang::FunctionBuilder, [bytecode_lang::NameRef; 3], Reg),
) -> String {
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let mt = m.add_type(TypeDef::Map { key: D, value: D });
    let (dn, in_, sn, xn) = (m.string("d"), m.string("i"), m.string("S"), m.string("x"));
    let st = m.add_type(TypeDef::Struct(StructDef {
        name: sn,
        fields: vec![
            Field { name: dn, ty: D },
            Field {
                name: in_,
                ty: ValType::I64,
            },
        ],
        ..Default::default()
    }));
    let k = m.constant(bytecode_lang::Const::Bytes(b"k".to_vec()));
    let mut f = m.function("main", &[], &[D]);
    let (arr, map, obj) = (f.reg(D), f.reg(D), f.reg(ValType::Ref(st)));
    let (n, t, out) = (f.reg(ValType::I64), f.reg(D), f.reg(D));
    let (aty, mty, sty) = (f.type_ref(at), f.type_ref(mt), f.type_ref(st));
    f.emit(Inst::LoadInt {
        dst: n,
        val: 0,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewArray {
        dst: arr,
        len: n,
        ty: aty,
    });
    f.emit(Inst::DLoadInt { dst: t, val: 10 });
    f.emit(Inst::ArrayPush { arr, src: t });
    f.emit(Inst::NewMap { dst: map, ty: mty });
    f.emit(Inst::DLoadConst { dst: out, k });
    f.emit(Inst::DLoadInt { dst: t, val: 20 });
    f.emit(Inst::MapSet {
        map,
        key: out,
        src: t,
    });
    f.emit(Inst::NewStruct { dst: obj, ty: sty });
    f.emit(Inst::LoadNil { dst: out });
    let names = [f.name_ref(dn), f.name_ref(in_), f.name_ref(xn)];
    emit(&mut f, names, out);
    f.ret(out);
    let main = m.add_function(f).unwrap();
    let p = load(m, &Host::new());
    let mut vm = Vm::new(&p);
    match vm.run(main, &[]) {
        Ok(v) => render(&vm, v),
        Err(e) => format!("{:?}", e.kind().unwrap_or(ErrorKind::Unreachable)),
    }
}

#[test]
fn reference_instruction_errors() {
    let (arr, map, obj) = (Reg(0), Reg(1), Reg(2));
    // dref_index out of range; on a non-container; dref_prop of a typed field
    // and of a missing one; dbind of a non-reference; dup of a reference;
    // cell_get of a non-cell; dunref of a non-container.
    let k = Reg(4);
    assert_eq!(
        corner(|f, _, out| {
            f.emit(Inst::DLoadInt { dst: k, val: 5 });
            f.emit(Inst::DRefIndex {
                dst: out,
                obj: arr,
                key: k,
            });
        }),
        "IndexOutOfBounds"
    );
    assert_eq!(
        corner(|f, _, out| {
            f.emit(Inst::DLoadInt { dst: k, val: 0 });
            f.emit(Inst::DRefIndex {
                dst: out,
                obj: k,
                key: k,
            });
        }),
        "TypeError"
    );
    assert_eq!(
        corner(|f, [_, i, _], out| {
            f.emit(Inst::DRefProp {
                dst: out,
                obj,
                name: i,
            });
        }),
        "TypeError"
    );
    assert_eq!(
        corner(|f, [_, _, x], out| {
            f.emit(Inst::DRefProp {
                dst: out,
                obj,
                name: x,
            });
        }),
        "UndefinedProperty"
    );
    assert_eq!(
        corner(|f, _, _| {
            f.emit(Inst::DLoadInt { dst: k, val: 0 });
            f.emit(Inst::DBindIndex {
                obj: arr,
                key: k,
                src: k,
            });
        }),
        "TypeError"
    );
    assert_eq!(
        corner(|f, _, out| {
            f.emit(Inst::NewRef { dst: out, src: arr });
            f.emit(Inst::Dup { dst: out, src: out });
        }),
        "TypeError"
    );
    assert_eq!(
        corner(|f, _, out| {
            f.emit(Inst::CellGet {
                dst: out,
                cell: arr,
            });
        }),
        "TypeError"
    );
    assert_eq!(
        corner(|f, _, _| {
            f.emit(Inst::DLoadInt { dst: k, val: 0 });
            f.emit(Inst::DUnrefIndex { obj: k, key: k });
        }),
        "TypeError"
    );
    // Unbinding a slot that is not a reference, or an absent key: nothing.
    assert_eq!(
        corner(|f, _, out| {
            f.emit(Inst::DLoadInt { dst: k, val: 7 });
            f.emit(Inst::DUnrefIndex { obj: map, key: k });
            f.emit(Inst::DLoadInt { dst: k, val: 0 });
            f.emit(Inst::DUnrefIndex { obj: arr, key: k });
            f.mov(out, arr);
        }),
        "[10]"
    );
}

#[test]
fn reference_slots_in_struct_fields_are_transparent_to_typed_access() {
    let (arr, obj) = (Reg(0), Reg(2));
    let (k, x) = (Reg(4), Reg(5));
    // r = &$obj->d; $obj->d = 5 (set_field); get_field d; cell_get r;
    // get_prop d; new_ref of a reference holds its value.
    assert_eq!(
        corner(|f, [d, _, _], out| {
            let _ = arr;
            f.emit(Inst::DRefProp {
                dst: k,
                obj,
                name: d,
            });
            f.emit(Inst::DLoadInt { dst: x, val: 5 });
            f.emit(Inst::SetField {
                obj,
                field: FieldIdx(0),
                src: x,
            });
            f.emit(Inst::GetField {
                dst: x,
                obj,
                field: FieldIdx(0),
            });
            f.emit(Inst::ArrayPush { arr, src: x });
            f.emit(Inst::CellGet { dst: x, cell: k });
            f.emit(Inst::ArrayPush { arr, src: x });
            f.emit(Inst::GetProp {
                dst: x,
                obj,
                name: d,
            });
            f.emit(Inst::ArrayPush { arr, src: x });
            f.emit(Inst::NewRef { dst: x, src: k });
            f.emit(Inst::CellGet { dst: x, cell: x });
            f.emit(Inst::ArrayPush { arr, src: x });
            // A reference stored by set_prop stores its value.
            f.emit(Inst::SetProp {
                obj,
                name: d,
                src: k,
            });
            f.emit(Inst::CellGet { dst: x, cell: k });
            f.emit(Inst::ArrayPush { arr, src: x });
            f.mov(out, arr);
        }),
        "[10, 5, 5, 5, 5, 5]"
    );
}

#[test]
fn separation_of_other_kinds_is_exactly_the_read() {
    let (map, obj) = (Reg(1), Reg(2));
    let (k, x) = (Reg(4), Reg(5));
    // dsep_index on a string is dget_index (the byte); dsep_prop of a typed
    // field is get_prop; dsep of an absent key is nil and stores nothing.
    assert_eq!(
        corner(|f, [_, i, _], out| {
            f.emit(Inst::DLoadConst {
                dst: x,
                k: bytecode_lang::ConstId(0),
            });
            f.emit(Inst::DLoadInt { dst: k, val: 0 });
            f.emit(Inst::DSepIndex {
                dst: out,
                obj: x,
                key: k,
            });
            let a = Reg(0);
            f.emit(Inst::ArrayPush { arr: a, src: out });
            f.emit(Inst::DSepProp {
                dst: out,
                obj,
                name: i,
            });
            f.emit(Inst::ArrayPush { arr: a, src: out });
            f.emit(Inst::DLoadInt { dst: k, val: 9 });
            f.emit(Inst::DSepIndex {
                dst: out,
                obj: map,
                key: k,
            });
            f.emit(Inst::ArrayPush { arr: a, src: out });
            let n = Reg(3);
            f.emit(Inst::MapLen { dst: n, map });
            f.emit(Inst::ToDyn {
                dst: out,
                src: n,
                from: Prim::I64,
            });
            f.emit(Inst::ArrayPush { arr: a, src: out });
            f.mov(out, a);
        }),
        // "k"[0] = 107; the i64 field's default 0; nil; the map still has 1.
        "[10, 107, 0, nil, 1]"
    );
}

#[test]
fn a_repeated_name_for_the_call_shape_hook_is_an_argument_error() {
    // 7(a: 1, ...['a' => 2]) with `call_shape` bound: the named map would
    // hold `a` twice.
    let mut m = ModuleBuilder::new();
    let a = m.string("a");
    let mt = m.add_type(TypeDef::Map { key: D, value: D });
    let k = m.constant(bytecode_lang::Const::Bytes(b"a".to_vec()));
    let mut hs = m.function("call_shape", &[D, D, D], &[D]);
    hs.ret(Reg(0));
    let hs = m.add_function(hs).unwrap();
    m.hook(Hook::CallShape, Callee::Func(hs));
    let mut main = m.function("main", &[], &[D]);
    let (callee, mp, key) = (main.reg(D), main.reg(D), main.reg(D));
    let w = main.regs(&[D, D, D]);
    let ty = main.type_ref(mt);
    main.emit(Inst::DLoadInt {
        dst: callee,
        val: 7,
    });
    main.emit(Inst::NewMap { dst: mp, ty });
    main.emit(Inst::DLoadConst { dst: key, k });
    main.emit(Inst::MapSet {
        map: mp,
        key,
        src: callee,
    });
    main.emit(Inst::DLoadInt {
        dst: Reg(w.0 + 1),
        val: 1,
    });
    main.mov(Reg(w.0 + 2), mp);
    main.dcall_shape(w, callee, &[ArgKind::Named(a), ArgKind::SpreadNamed]);
    main.ret(w);
    let main = m.add_function(main).unwrap();
    let p = load(m, &Host::new());
    let err = Vm::new(&p).run(main, &[]).unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::ArgumentError));
}

#[test]
fn a_reference_casts_to_a_cell_of_dyn_and_nothing_else() {
    // cast(new_ref(1), ref cell dyn) passes; to ref cell i64 or an array
    // type it is a TypeError; is_kind says reference.
    for (target, ok) in [(0, true), (1, false), (2, false)] {
        let mut m = ModuleBuilder::new();
        let cd = m.add_type(TypeDef::Cell(D));
        let ci = m.add_type(TypeDef::Cell(ValType::I64));
        let ar = m.add_type(TypeDef::Array(D));
        let t = [cd, ci, ar][target];
        let mut f = m.function("main", &[], &[D]);
        let (x, r, kind) = (f.reg(D), f.reg(D), f.reg(ValType::Bool));
        let typed = f.reg(ValType::Ref(t));
        let tr = f.type_ref(t);
        f.emit(Inst::DLoadInt { dst: x, val: 1 });
        f.emit(Inst::NewRef { dst: r, src: x });
        f.emit(Inst::IsKind {
            dst: kind,
            src: r,
            kind: bytecode_lang::Kind::Reference,
        });
        f.emit(Inst::Cast {
            dst: typed,
            src: r,
            ty: tr,
        });
        f.emit(Inst::ToDyn {
            dst: x,
            src: kind,
            from: Prim::Bool,
        });
        f.ret(x);
        let main = m.add_function(f).unwrap();
        let p = load(m, &Host::new());
        let got = Vm::new(&p).run(main, &[]);
        if ok {
            assert_eq!(got, Ok(Value::Bool(true)));
        } else {
            assert_eq!(got.unwrap_err().kind(), Some(ErrorKind::TypeError));
        }
    }
}
