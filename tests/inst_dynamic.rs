//! LSB conformance, dynamic instructions (LSB §5.6–§5.8): the numeric fast
//! path, `promote`, comparisons, truthiness, concatenation, conversions,
//! type tests, indexing, properties, dynamic calls, iteration, and every
//! hook.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::type_complexity
)]

mod common;

use bvm_lang::{Host, HostError, Program, Value, Vm, VmError};
use bytecode_lang::{
    Callee, Const, ErrorKind, Field, FuncId, Hook, Inst, Kind, Method, ModuleBuilder, Overflow,
    Policy, Prim, Reg, StructDef, TypeDef, ValType,
};
use common::{BOOL, D, I64};

/// A dynamic binary instruction on two `dyn` arguments.
fn dbin(
    make: fn(Reg, Reg, Reg, Policy) -> Inst,
    pol: Policy,
    a: Value,
    b: Value,
) -> Result<Value, VmError> {
    common::eval(&[D, D], &[D], &[a, b], |_, f| {
        let r = f.reg(D);
        f.emit(make(r, Reg(0), Reg(1), pol));
        f.ret(r);
    })
}

type DMk = fn(Reg, Reg, Reg, Policy) -> Inst;

const DADD: DMk = |dst, lhs, rhs, pol| Inst::DAdd { dst, lhs, rhs, pol };
const DSUB: DMk = |dst, lhs, rhs, pol| Inst::DSub { dst, lhs, rhs, pol };
const DMUL: DMk = |dst, lhs, rhs, pol| Inst::DMul { dst, lhs, rhs, pol };
const DDIV: DMk = |dst, lhs, rhs, pol| Inst::DDiv { dst, lhs, rhs, pol };
const DREM: DMk = |dst, lhs, rhs, pol| Inst::DRem { dst, lhs, rhs, pol };
const DFLOORDIV: DMk = |dst, lhs, rhs, pol| Inst::DFloorDiv { dst, lhs, rhs, pol };
const DFLOORMOD: DMk = |dst, lhs, rhs, pol| Inst::DFloorMod { dst, lhs, rhs, pol };
const DAND: DMk = |dst, lhs, rhs, pol| Inst::DAnd { dst, lhs, rhs, pol };
const DOR: DMk = |dst, lhs, rhs, pol| Inst::DOr { dst, lhs, rhs, pol };
const DXOR: DMk = |dst, lhs, rhs, pol| Inst::DXor { dst, lhs, rhs, pol };
const DSHL: DMk = |dst, lhs, rhs, pol| Inst::DShl { dst, lhs, rhs, pol };
const DSHR: DMk = |dst, lhs, rhs, pol| Inst::DShr { dst, lhs, rhs, pol };

fn promote() -> Policy {
    Policy::new().with_overflow(Overflow::Promote)
}

fn raised(kind: ErrorKind, pc: u32) -> Result<Value, VmError> {
    Err(VmError::Raised {
        kind,
        func: FuncId(0),
        pc,
    })
}

#[test]
fn op_dynamic_arithmetic_int_and_float_paths() {
    let p = Policy::new();
    let (i, fl) = (Value::Int, Value::Float);
    let cases: Vec<(DMk, Value, Value, Value)> = vec![
        (DADD, i(2), i(3), i(5)),
        (DADD, i(2), fl(0.5), fl(2.5)),
        (DSUB, fl(1.0), i(3), fl(-2.0)),
        (DMUL, i(-4), i(3), i(-12)),
        (DDIV, i(7), i(2), i(3)),
        (DDIV, i(7), fl(2.0), fl(3.5)),
        (DREM, i(-7), i(2), i(-1)),
        (DREM, fl(-7.0), i(2), fl(-1.0)),
        (DFLOORDIV, i(-7), i(2), i(-4)),
        (DFLOORDIV, fl(-7.0), fl(2.0), fl(-4.0)),
        (DFLOORMOD, i(-7), i(2), i(1)),
        (DFLOORMOD, fl(7.0), fl(-2.0), fl(-1.0)),
        (DAND, i(6), i(3), i(2)),
        (DOR, i(6), i(3), i(7)),
        (DXOR, i(6), i(3), i(5)),
        (DSHL, i(1), i(62), i(1 << 62)),
        (DSHR, i(-8), i(1), i(-4)),
        // Results crossing the inline-int range box transparently.
        (DMUL, i(1 << 40), i(1 << 20), i(1 << 60)),
        (DADD, i(i64::MAX - 1), i(1), i(i64::MAX)),
    ];
    for (make, a, b, expect) in cases {
        assert_eq!(
            dbin(make, p, a, b),
            Ok(expect),
            "{:?} {a:?} {b:?}",
            make(Reg(0), Reg(0), Reg(0), p)
        );
    }
}

#[test]
fn op_dynamic_arithmetic_policies() {
    let i = Value::Int;
    // error / wrap / trap / promote on overflow.
    assert_eq!(
        dbin(DADD, Policy::new(), i(i64::MAX), i(1)),
        raised(ErrorKind::ArithOverflow, 0)
    );
    assert_eq!(
        dbin(
            DADD,
            Policy::new().with_overflow(Overflow::Wrap),
            i(i64::MAX),
            i(1)
        ),
        Ok(i(i64::MIN))
    );
    assert_eq!(
        dbin(
            DADD,
            Policy::new().with_overflow(Overflow::Trap),
            i(i64::MAX),
            i(1)
        ),
        Err(VmError::Trap {
            kind: ErrorKind::ArithOverflow,
            func: FuncId(0),
            pc: 0
        })
    );
    // PHP_INT_MAX + 1 is 9.223372036854775808e18.
    assert_eq!(
        dbin(DADD, promote(), i(i64::MAX), i(1)),
        Ok(Value::Float(9_223_372_036_854_775_808.0))
    );
    assert_eq!(
        dbin(DSUB, promote(), i(i64::MIN), i(1)),
        Ok(Value::Float(-9_223_372_036_854_775_808.0))
    );
    assert_eq!(
        dbin(DMUL, promote(), i(i64::MAX), i(i64::MAX)),
        Ok(Value::Float((i64::MAX as f64) * (i64::MAX as f64)))
    );
    // promote division: exact ints stay ints, anything else is the
    // correctly rounded quotient.
    assert_eq!(dbin(DDIV, promote(), i(8), i(2)), Ok(i(4)));
    assert_eq!(dbin(DDIV, promote(), i(7), i(2)), Ok(Value::Float(3.5)));
    assert_eq!(
        dbin(DDIV, promote(), i(i64::MIN), i(-1)),
        Ok(Value::Float(9_223_372_036_854_775_808.0))
    );
    assert_eq!(
        dbin(DDIV, promote(), i(1), i(0)),
        raised(ErrorKind::DivByZero, 0)
    );
    assert_eq!(
        dbin(DFLOORDIV, promote(), i(i64::MIN), i(-1)),
        Ok(Value::Float(9_223_372_036_854_775_808.0))
    );
    // rem and floor_mod never overflow.
    assert_eq!(dbin(DREM, promote(), i(i64::MIN), i(-1)), Ok(i(0)));
    // Shift policy.
    assert_eq!(
        dbin(DSHL, Policy::new(), i(1), i(64)),
        raised(ErrorKind::ShiftOutOfRange, 0)
    );
    assert_eq!(
        dbin(
            DSHL,
            Policy::new().with_shift(bytecode_lang::Shift::Mask),
            i(1),
            i(65)
        ),
        Ok(i(2))
    );
    // div_zero = trap.
    assert_eq!(
        dbin(
            DDIV,
            Policy::new().with_div_zero(bytecode_lang::DivZero::Trap),
            i(1),
            i(0)
        ),
        Err(VmError::Trap {
            kind: ErrorKind::DivByZero,
            func: FuncId(0),
            pc: 0
        })
    );
}

#[test]
fn op_dynamic_arithmetic_without_hook_is_type_error() {
    for make in [
        DADD, DSUB, DMUL, DDIV, DREM, DFLOORDIV, DFLOORMOD, DAND, DOR, DXOR, DSHL, DSHR,
    ] {
        assert_eq!(
            dbin(make, Policy::new(), Value::Bool(true), Value::Int(1)),
            raised(ErrorKind::TypeError, 0)
        );
    }
    // Bitwise instructions have no float path.
    assert_eq!(
        dbin(DAND, Policy::new(), Value::Float(1.0), Value::Int(1)),
        raised(ErrorKind::TypeError, 0)
    );
}

/// A module with `main(dyn, dyn) -> dyn` running `inst` and a hook bound to
/// a bytecode function that returns its second argument (or, for unary
/// hooks, the first).
fn with_hook(
    hook: Hook,
    arity: usize,
    inst: fn(Reg, Reg, Reg) -> Inst,
    args: &[Value],
) -> Result<Value, VmError> {
    let mut m = ModuleBuilder::new();
    let mut main = m.function("main", &[D, D], &[D]);
    let params = vec![D; arity];
    let mut h = m.function("hook", &params, &[D]);
    let r = main.reg(D);
    main.emit(inst(r, Reg(0), Reg(1)));
    main.ret(r);
    h.ret(Reg(u16::try_from(arity - 1).unwrap()));
    let hid = h.id();
    m.add_function(main).unwrap();
    m.add_function(h).unwrap();
    m.hook(hook, Callee::Func(hid));
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    Vm::new(&p).run(FuncId(0), args)
}

#[test]
fn hooks_receive_operands_and_their_result_is_written() {
    let s = Value::Bool(true);
    let cases: Vec<(Hook, DMk)> = vec![
        (Hook::Add, DADD),
        (Hook::Sub, DSUB),
        (Hook::Mul, DMUL),
        (Hook::Div, DDIV),
        (Hook::Rem, DREM),
        (Hook::FloorDiv, DFLOORDIV),
        (Hook::FloorMod, DFLOORMOD),
        (Hook::BitAnd, DAND),
        (Hook::BitOr, DOR),
        (Hook::BitXor, DXOR),
        (Hook::Shl, DSHL),
        (Hook::Shr, DSHR),
    ];
    for (hook, make) in cases {
        // Each instruction is rebuilt with a default policy.
        let inst: fn(Reg, Reg, Reg) -> Inst = match hook {
            Hook::Add => |d, l, r| DADD(d, l, r, Policy::new()),
            Hook::Sub => |d, l, r| DSUB(d, l, r, Policy::new()),
            Hook::Mul => |d, l, r| DMUL(d, l, r, Policy::new()),
            Hook::Div => |d, l, r| DDIV(d, l, r, Policy::new()),
            Hook::Rem => |d, l, r| DREM(d, l, r, Policy::new()),
            Hook::FloorDiv => |d, l, r| DFLOORDIV(d, l, r, Policy::new()),
            Hook::FloorMod => |d, l, r| DFLOORMOD(d, l, r, Policy::new()),
            Hook::BitAnd => |d, l, r| DAND(d, l, r, Policy::new()),
            Hook::BitOr => |d, l, r| DOR(d, l, r, Policy::new()),
            Hook::BitXor => |d, l, r| DXOR(d, l, r, Policy::new()),
            Hook::Shl => |d, l, r| DSHL(d, l, r, Policy::new()),
            _ => |d, l, r| DSHR(d, l, r, Policy::new()),
        };
        let _ = make;
        assert_eq!(
            with_hook(hook, 2, inst, &[s, Value::Int(42)]),
            Ok(Value::Int(42)),
            "{hook}"
        );
    }
    // Unary hooks.
    assert_eq!(
        with_hook(
            Hook::Neg,
            1,
            |d, l, _| Inst::DNeg {
                dst: d,
                src: l,
                pol: Policy::new()
            },
            &[s, Value::Nil]
        ),
        Ok(s)
    );
    assert_eq!(
        with_hook(
            Hook::BitNot,
            1,
            |d, l, _| Inst::DNot {
                dst: d,
                src: l,
                pol: Policy::new()
            },
            &[Value::Float(1.0), Value::Nil]
        ),
        Ok(Value::Float(1.0))
    );
}

#[test]
fn op_dneg_and_dnot() {
    let un = |inst: fn(Reg, Reg) -> Inst, a: Value| {
        common::eval(&[D], &[D], &[a], |_, f| {
            let r = f.reg(D);
            f.emit(inst(r, Reg(0)));
            f.ret(r);
        })
    };
    let neg = |d, s| Inst::DNeg {
        dst: d,
        src: s,
        pol: Policy::new(),
    };
    let neg_p = |d, s| Inst::DNeg {
        dst: d,
        src: s,
        pol: Policy::new().with_overflow(Overflow::Promote),
    };
    let not = |d, s| Inst::DNot {
        dst: d,
        src: s,
        pol: Policy::new(),
    };
    assert_eq!(un(neg, Value::Int(5)), Ok(Value::Int(-5)));
    assert_eq!(un(neg, Value::Float(0.0)), Ok(Value::Float(-0.0)));
    assert_eq!(
        un(neg, Value::Int(i64::MIN)),
        raised(ErrorKind::ArithOverflow, 0)
    );
    assert_eq!(
        un(neg_p, Value::Int(i64::MIN)),
        Ok(Value::Float(9_223_372_036_854_775_808.0))
    );
    assert_eq!(un(not, Value::Int(0)), Ok(Value::Int(-1)));
    assert_eq!(un(not, Value::Float(1.0)), raised(ErrorKind::TypeError, 0));
}

fn dcmp(inst: fn(Reg, Reg, Reg) -> Inst, a: Value, b: Value) -> Result<Value, VmError> {
    common::eval(&[D, D], &[BOOL], &[a, b], |_, f| {
        let r = f.reg(BOOL);
        f.emit(inst(r, Reg(0), Reg(1)));
        f.ret(r);
    })
}

const DEQ: fn(Reg, Reg, Reg) -> Inst = |dst, lhs, rhs| Inst::DEq { dst, lhs, rhs };
const DNE: fn(Reg, Reg, Reg) -> Inst = |dst, lhs, rhs| Inst::DNe { dst, lhs, rhs };
const DLT: fn(Reg, Reg, Reg) -> Inst = |dst, lhs, rhs| Inst::DLt { dst, lhs, rhs };
const DLE: fn(Reg, Reg, Reg) -> Inst = |dst, lhs, rhs| Inst::DLe { dst, lhs, rhs };
const DGT: fn(Reg, Reg, Reg) -> Inst = |dst, lhs, rhs| Inst::DGt { dst, lhs, rhs };
const DGE: fn(Reg, Reg, Reg) -> Inst = |dst, lhs, rhs| Inst::DGe { dst, lhs, rhs };

#[test]
fn op_deq_dne_builtins() {
    let t = Ok(Value::Bool(true));
    let f = Ok(Value::Bool(false));
    let i = Value::Int;
    let fl = Value::Float;
    assert_eq!(dcmp(DEQ, i(1), fl(1.0)), t);
    assert_eq!(
        dcmp(DEQ, i((1 << 53) + 1), fl(9_007_199_254_740_992.0)),
        f,
        "exact, not via f64"
    );
    assert_eq!(dcmp(DEQ, fl(f64::NAN), fl(f64::NAN)), f);
    assert_eq!(dcmp(DEQ, Value::Nil, Value::Nil), t);
    assert_eq!(dcmp(DEQ, Value::Bool(true), Value::Bool(true)), t);
    assert_eq!(dcmp(DEQ, Value::Char('a'), Value::Char('a')), t);
    assert_eq!(
        dcmp(DEQ, Value::Nil, i(0)),
        f,
        "different kinds, no hook: identity"
    );
    assert_eq!(dcmp(DNE, i(1), i(2)), t);
    assert_eq!(dcmp(DNE, fl(f64::NAN), fl(f64::NAN)), t);
}

#[test]
fn op_deq_compares_strings_bytewise() {
    let p = common::load(common::module(&[], &[BOOL], |m, f| {
        let a = m.constant(Const::Bytes(b"ab".to_vec()));
        let b = m.constant(Const::Bytes(b"a".to_vec()));
        let c = m.constant(Const::Bytes(b"b".to_vec()));
        let (x, y, z, w, r) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D), f.reg(BOOL));
        f.emit(Inst::DLoadConst { dst: x, k: a });
        f.emit(Inst::DLoadConst { dst: y, k: b });
        f.emit(Inst::DLoadConst { dst: z, k: c });
        f.emit(Inst::DConcat {
            dst: w,
            lhs: y,
            rhs: z,
        }); // a new "ab"
        f.emit(Inst::DEq {
            dst: r,
            lhs: x,
            rhs: w,
        });
        f.ret(r);
    }));
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Bool(true)));
}

#[test]
fn op_dlt_dle_dgt_dge() {
    let i = Value::Int;
    let fl = Value::Float;
    let b = |x| Ok(Value::Bool(x));
    assert_eq!(dcmp(DLT, i(1), fl(1.5)), b(true));
    assert_eq!(dcmp(DLE, i(2), fl(2.0)), b(true));
    assert_eq!(
        dcmp(DGT, i(i64::MAX), fl(9_223_372_036_854_775_808.0)),
        b(false)
    );
    assert_eq!(dcmp(DGE, fl(f64::NAN), i(0)), b(false));
    assert_eq!(dcmp(DLT, fl(f64::NAN), i(0)), b(false));
    assert_eq!(dcmp(DLT, Value::Char('a'), Value::Char('b')), b(true));
    assert_eq!(dcmp(DGT, Value::Char('a'), Value::Char('b')), b(false));
    assert_eq!(dcmp(DLT, Value::Nil, i(1)), raised(ErrorKind::TypeError, 0));
    assert_eq!(
        dcmp(DGE, Value::Bool(true), Value::Bool(false)),
        raised(ErrorKind::TypeError, 0)
    );
}

#[test]
fn comparison_hooks_are_called_with_swapped_operands_for_gt_ge() {
    // lt hook returns `a == nil` — for dgt(x, nil) it must see (nil, x).
    let mut m = ModuleBuilder::new();
    let mut main = m.function("main", &[D, D], &[BOOL]);
    let mut lt = m.function("lt", &[D, D], &[D]);
    let r = main.reg(BOOL);
    main.emit(Inst::DGt {
        dst: r,
        lhs: Reg(0),
        rhs: Reg(1),
    });
    main.ret(r);
    let (is_nil, out) = (lt.reg(BOOL), lt.reg(D));
    lt.emit(Inst::IsKind {
        dst: is_nil,
        src: Reg(0),
        kind: Kind::Nil,
    });
    lt.emit(Inst::ToDyn {
        dst: out,
        src: is_nil,
        from: Prim::Bool,
    });
    lt.ret(out);
    let ltid = lt.id();
    m.add_function(main).unwrap();
    m.add_function(lt).unwrap();
    m.hook(Hook::Lt, Callee::Func(ltid));
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    assert_eq!(
        vm.run(FuncId(0), &[Value::Bool(true), Value::Nil]),
        Ok(Value::Bool(true))
    );
    assert_eq!(
        vm.run(FuncId(0), &[Value::Nil, Value::Bool(true)]),
        Ok(Value::Bool(false))
    );
}

#[test]
fn eq_hook_result_must_be_a_bool() {
    // An eq hook returning an int is a TypeError at the deq.
    assert_eq!(
        with_hook(
            Hook::Eq,
            2,
            |d, l, r| Inst::DEq {
                dst: d,
                lhs: l,
                rhs: r
            },
            &[Value::Nil, Value::Int(3)]
        ),
        raised(ErrorKind::TypeError, 0)
    );
    // With a bool it is used (and negated by dne).
    let mut m = ModuleBuilder::new();
    let mut main = m.function("main", &[D, D], &[BOOL]);
    let mut eq = m.function("eq", &[D, D], &[D]);
    let r = main.reg(BOOL);
    main.emit(Inst::DNe {
        dst: r,
        lhs: Reg(0),
        rhs: Reg(1),
    });
    main.ret(r);
    let (t, b) = (eq.reg(D), eq.reg(BOOL));
    eq.emit(Inst::LoadBool { dst: b, val: true });
    eq.emit(Inst::ToDyn {
        dst: t,
        src: b,
        from: Prim::Bool,
    });
    eq.ret(t);
    let id = eq.id();
    m.add_function(main).unwrap();
    m.add_function(eq).unwrap();
    m.hook(Hook::Eq, Callee::Func(id));
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    // nil vs int has no built-in rule: the hook says equal, dne says false.
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[Value::Nil, Value::Int(1)]),
        Ok(Value::Bool(false))
    );
}

fn truthy(v: Value, negate: bool) -> Result<Value, VmError> {
    common::eval(&[D], &[BOOL], &[v], |_, f| {
        let r = f.reg(BOOL);
        if negate {
            f.emit(Inst::DLNot {
                dst: r,
                src: Reg(0),
            });
        } else {
            f.emit(Inst::DTruthy {
                dst: r,
                src: Reg(0),
            });
        }
        f.ret(r);
    })
}

#[test]
fn op_dtruthy_and_dlnot_follow_the_table() {
    let cases = [
        (Value::Nil, false),
        (Value::Bool(false), false),
        (Value::Bool(true), true),
        (Value::Int(0), false),
        (Value::Int(-3), true),
        (Value::Float(0.0), false),
        (Value::Float(-0.0), false),
        (Value::Float(f64::NAN), true),
        (Value::Char('\0'), true),
        (Value::Int(1 << 60), true),
    ];
    for (v, expect) in cases {
        assert_eq!(truthy(v, false), Ok(Value::Bool(expect)), "{v:?}");
        assert_eq!(truthy(v, true), Ok(Value::Bool(!expect)), "not {v:?}");
    }
}

#[test]
fn truthy_of_strings_and_collections_and_hook() {
    let p = common::load(common::module(&[], &[D], |m, f| {
        let empty = m.constant(Const::Bytes(Vec::new()));
        let zero = m.constant(Const::Bytes(b"0".to_vec()));
        let arr = m.constant(Const::Array(Vec::new()));
        let (a, b, c) = (f.reg(D), f.reg(D), f.reg(D));
        let (ra, rb, rc) = (f.reg(BOOL), f.reg(BOOL), f.reg(BOOL));
        let out = f.reg(D);
        f.emit(Inst::DLoadConst { dst: a, k: empty });
        f.emit(Inst::DLoadConst { dst: b, k: zero });
        f.emit(Inst::DLoadConst { dst: c, k: arr });
        f.emit(Inst::DTruthy { dst: ra, src: a });
        f.emit(Inst::DTruthy { dst: rb, src: b });
        f.emit(Inst::DTruthy { dst: rc, src: c });
        // Pack the three booleans into an int: ra*4 + rb*2 + rc.
        let (ia, ib, ic) = (f.reg(I64), f.reg(I64), f.reg(I64));
        let op = bytecode_lang::IntOp::new(bytecode_lang::IntTy::I64);
        f.emit(Inst::BoolToInt {
            dst: ia,
            src: ra,
            ty: bytecode_lang::IntTy::I64,
        });
        f.emit(Inst::BoolToInt {
            dst: ib,
            src: rb,
            ty: bytecode_lang::IntTy::I64,
        });
        f.emit(Inst::BoolToInt {
            dst: ic,
            src: rc,
            ty: bytecode_lang::IntTy::I64,
        });
        f.emit(Inst::IAdd {
            dst: ia,
            lhs: ia,
            rhs: ia,
            op,
        });
        f.emit(Inst::IAdd {
            dst: ia,
            lhs: ia,
            rhs: ib,
            op,
        });
        f.emit(Inst::IAdd {
            dst: ia,
            lhs: ia,
            rhs: ia,
            op,
        });
        f.emit(Inst::IAdd {
            dst: ia,
            lhs: ia,
            rhs: ic,
            op,
        });
        f.emit(Inst::ToDyn {
            dst: out,
            src: ia,
            from: Prim::I64,
        });
        f.ret(out);
    }));
    // "" false, "0" true (no hook: non-empty), [] false.
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(0b010)));

    // With a truthy hook (PHP: "0" is falsy), strings go to the hook but
    // ints never do.
    let mut host = Host::new();
    host.register("php", "truthy", |ctx, args| {
        let s = ctx.str_bytes(args[0]).unwrap_or(b"");
        Ok(Value::Bool(!(s.is_empty() || s == b"0")))
    });
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D], &[D]);
    let imp = m.import("php", "truthy", sig);
    m.hook(Hook::Truthy, Callee::Import(imp));
    let zero = m.constant(Const::Bytes(b"0".to_vec()));
    let mut f = m.function("main", &[], &[BOOL]);
    let (s, r) = (f.reg(D), f.reg(BOOL));
    f.emit(Inst::DLoadConst { dst: s, k: zero });
    f.emit(Inst::DTruthy { dst: r, src: s });
    f.ret(r);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Bool(false)));
}

#[test]
fn op_dconcat_strings_and_hook_and_type_error() {
    let p = common::load(common::module(&[], &[D], |m, f| {
        let a = m.constant(Const::Bytes(b"foo".to_vec()));
        let b = m.constant(Const::Bytes(b"bar".to_vec()));
        let (x, y, r) = (f.reg(D), f.reg(D), f.reg(D));
        f.emit(Inst::DLoadConst { dst: x, k: a });
        f.emit(Inst::DLoadConst { dst: y, k: b });
        f.emit(Inst::DConcat {
            dst: r,
            lhs: x,
            rhs: y,
        });
        f.ret(r);
    }));
    let mut vm = Vm::new(&p);
    let s = vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(vm.str_bytes(s), Some(&b"foobar"[..]));
    let err = common::eval(&[D, D], &[D], &[Value::Int(1), Value::Int(2)], |_, f| {
        let r = f.reg(D);
        f.emit(Inst::DConcat {
            dst: r,
            lhs: Reg(0),
            rhs: Reg(1),
        });
        f.ret(r);
    });
    assert_eq!(err, raised(ErrorKind::TypeError, 0));
    assert_eq!(
        with_hook(
            Hook::Concat,
            2,
            |d, l, r| Inst::DConcat {
                dst: d,
                lhs: l,
                rhs: r
            },
            &[Value::Int(1), Value::Int(2)]
        ),
        Ok(Value::Int(2))
    );
}

fn to_dyn(from: Prim, ty: ValType, v: Value) -> Result<Value, VmError> {
    common::eval(&[ty], &[D], &[v], |_, f| {
        let r = f.reg(D);
        f.emit(Inst::ToDyn {
            dst: r,
            src: Reg(0),
            from,
        });
        f.ret(r);
    })
}

fn from_dyn(to: Prim, ty: ValType, v: Value) -> Result<Value, VmError> {
    common::eval(&[D], &[ty], &[v], |_, f| {
        let r = f.reg(ty);
        f.emit(Inst::FromDyn {
            dst: r,
            src: Reg(0),
            to,
        });
        f.ret(r);
    })
}

#[test]
fn op_to_dyn_every_primitive() {
    assert_eq!(
        to_dyn(Prim::Bool, BOOL, Value::Bool(true)),
        Ok(Value::Bool(true))
    );
    assert_eq!(
        to_dyn(Prim::I8, ValType::I8, Value::Int(-128)),
        Ok(Value::Int(-128))
    );
    assert_eq!(
        to_dyn(Prim::I64, I64, Value::Int(i64::MIN)),
        Ok(Value::Int(i64::MIN))
    );
    assert_eq!(
        to_dyn(Prim::U32, ValType::U32, Value::UInt(u64::from(u32::MAX))),
        Ok(Value::Int(i64::from(u32::MAX)))
    );
    assert_eq!(
        to_dyn(Prim::U64, ValType::U64, Value::UInt(u64::MAX)),
        raised(ErrorKind::ArithOverflow, 0)
    );
    assert_eq!(
        to_dyn(Prim::F32, ValType::F32, Value::F32(0.1)),
        Ok(Value::Float(f64::from(0.1f32)))
    );
    assert_eq!(
        to_dyn(Prim::F64, ValType::F64, Value::Float(-0.0)),
        Ok(Value::Float(-0.0))
    );
    assert_eq!(
        to_dyn(Prim::Char, ValType::Char, Value::Char('q')),
        Ok(Value::Char('q'))
    );
    assert_eq!(to_dyn(Prim::Str, ValType::Str, Value::Nil), Ok(Value::Nil));
}

#[test]
fn op_from_dyn_checks_kind_and_range() {
    assert_eq!(from_dyn(Prim::I64, I64, Value::Int(5)), Ok(Value::Int(5)));
    assert_eq!(
        from_dyn(Prim::I8, ValType::I8, Value::Int(300)),
        raised(ErrorKind::ArithOverflow, 0)
    );
    assert_eq!(
        from_dyn(Prim::U8, ValType::U8, Value::Int(-1)),
        raised(ErrorKind::ArithOverflow, 0)
    );
    assert_eq!(
        from_dyn(Prim::I64, I64, Value::Float(1.0)),
        raised(ErrorKind::TypeError, 0)
    );
    assert_eq!(
        from_dyn(Prim::F64, ValType::F64, Value::Int(1)),
        raised(ErrorKind::TypeError, 0)
    );
    assert_eq!(
        from_dyn(Prim::F32, ValType::F32, Value::Float(0.1)),
        Ok(Value::F32(0.1))
    );
    assert_eq!(
        from_dyn(Prim::Bool, BOOL, Value::Bool(false)),
        Ok(Value::Bool(false))
    );
    assert_eq!(
        from_dyn(Prim::Char, ValType::Char, Value::Char('z')),
        Ok(Value::Char('z'))
    );
    assert_eq!(
        from_dyn(Prim::Str, ValType::Str, Value::Int(1)),
        raised(ErrorKind::TypeError, 0)
    );
    assert_eq!(
        from_dyn(Prim::Str, ValType::Str, Value::Nil),
        Ok(Value::Nil)
    );
}

#[test]
fn op_type_of_and_is_kind() {
    let kinds = [
        (Value::Nil, Kind::Nil),
        (Value::Bool(true), Kind::Bool),
        (Value::Int(1), Kind::Int),
        (Value::Int(1 << 62), Kind::Int),
        (Value::Float(1.0), Kind::Float),
        (Value::Char('c'), Kind::Char),
    ];
    for (v, k) in kinds {
        let out = common::eval(&[D], &[ValType::U8], &[v], |_, f| {
            let r = f.reg(ValType::U8);
            f.emit(Inst::TypeOf {
                dst: r,
                src: Reg(0),
            });
            f.ret(r);
        });
        assert_eq!(out, Ok(Value::UInt(u64::from(k.code()))), "{v:?}");
        let is = common::eval(&[D], &[BOOL], &[v], |_, f| {
            let r = f.reg(BOOL);
            f.emit(Inst::IsKind {
                dst: r,
                src: Reg(0),
                kind: k,
            });
            f.ret(r);
        });
        assert_eq!(is, Ok(Value::Bool(true)));
    }
    // Heap kinds through constants.
    let p = common::load(common::module(&[], &[D], |m, f| {
        let arr = m.constant(Const::Array(Vec::new()));
        let map = m.constant(Const::Map(Vec::new()));
        let s = m.constant(Const::Bytes(Vec::new()));
        let (a, b, c) = (f.reg(D), f.reg(D), f.reg(D));
        let (ka, kb, kc) = (f.reg(ValType::U8), f.reg(ValType::U8), f.reg(ValType::U8));
        f.emit(Inst::DLoadConst { dst: a, k: arr });
        f.emit(Inst::DLoadConst { dst: b, k: map });
        f.emit(Inst::DLoadConst { dst: c, k: s });
        f.emit(Inst::TypeOf { dst: ka, src: a });
        f.emit(Inst::TypeOf { dst: kb, src: b });
        f.emit(Inst::TypeOf { dst: kc, src: c });
        // ka*256 + kb*16 + kc as a dyn int, via u8 -> to_dyn and dyn math.
        let (da, db, dc, s256, s16) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D), f.reg(D));
        f.emit(Inst::ToDyn {
            dst: da,
            src: ka,
            from: Prim::U8,
        });
        f.emit(Inst::ToDyn {
            dst: db,
            src: kb,
            from: Prim::U8,
        });
        f.emit(Inst::ToDyn {
            dst: dc,
            src: kc,
            from: Prim::U8,
        });
        f.emit(Inst::DLoadInt {
            dst: s256,
            val: 256,
        });
        f.emit(Inst::DLoadInt { dst: s16, val: 16 });
        f.emit(Inst::DMul {
            dst: da,
            lhs: da,
            rhs: s256,
            pol: Policy::new(),
        });
        f.emit(Inst::DMul {
            dst: db,
            lhs: db,
            rhs: s16,
            pol: Policy::new(),
        });
        f.emit(Inst::DAdd {
            dst: da,
            lhs: da,
            rhs: db,
            pol: Policy::new(),
        });
        f.emit(Inst::DAdd {
            dst: da,
            lhs: da,
            rhs: dc,
            pol: Policy::new(),
        });
        f.ret(da);
    }));
    let expect = i64::from(Kind::Array.code()) * 256
        + i64::from(Kind::Map.code()) * 16
        + i64::from(Kind::Str.code());
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(expect)));
}

/// A module with struct `Base { x: i64 }` (method `get`) and
/// `Derived : Base { x: i64, y: dyn }`.
struct Shapes {
    m: ModuleBuilder,
    base: bytecode_lang::TypeId,
    derived: bytecode_lang::TypeId,
    other: bytecode_lang::TypeId,
    get: FuncId,
    x: bytecode_lang::StrId,
    y: bytecode_lang::StrId,
}

fn shapes() -> Shapes {
    let mut m = ModuleBuilder::new();
    let x = m.string("x");
    let y = m.string("y");
    let get_name = m.string("get");
    let base = m.reserve_type();
    let mut get = m.function("Base.get", &[ValType::Ref(base)], &[I64]);
    let v = get.reg(I64);
    get.emit(Inst::GetField {
        dst: v,
        obj: Reg(0),
        field: bytecode_lang::FieldIdx(0),
    });
    get.ret(v);
    let get_id = m.add_function(get).unwrap();
    let base_name = m.string("Base");
    m.define_type(
        base,
        TypeDef::Struct(StructDef {
            name: base_name,
            parent: None,
            fields: vec![Field { name: x, ty: I64 }],
            methods: vec![Method {
                name: get_name,
                func: get_id,
            }],
        }),
    );
    let derived_name = m.string("Derived");
    let derived = m.add_type(TypeDef::Struct(StructDef {
        name: derived_name,
        parent: Some(base),
        fields: vec![Field { name: x, ty: I64 }, Field { name: y, ty: D }],
        methods: Vec::new(),
    }));
    let other_name = m.string("Other");
    let other = m.add_type(TypeDef::Struct(StructDef {
        name: other_name,
        ..Default::default()
    }));
    Shapes {
        m,
        base,
        derived,
        other,
        get: get_id,
        x,
        y,
    }
}

#[test]
fn op_is_type_and_cast_follow_inheritance() {
    let mut s = shapes();
    let mut f = s.m.function("main", &[], &[I64]);
    let (base_r, derived_r, other_r) = (
        f.type_ref(s.base),
        f.type_ref(s.derived),
        f.type_ref(s.other),
    );
    let obj = f.reg(D);
    let (b1, b2, b3) = (f.reg(BOOL), f.reg(BOOL), f.reg(BOOL));
    let casted = f.reg(ValType::Ref(s.base));
    let out = f.reg(I64);
    f.emit(Inst::NewStruct {
        dst: obj,
        ty: derived_r,
    });
    f.emit(Inst::IsType {
        dst: b1,
        src: obj,
        ty: base_r,
    }); // true
    f.emit(Inst::IsType {
        dst: b2,
        src: obj,
        ty: derived_r,
    }); // true
    f.emit(Inst::IsType {
        dst: b3,
        src: obj,
        ty: other_r,
    }); // false
    f.emit(Inst::Cast {
        dst: casted,
        src: obj,
        ty: base_r,
    }); // passes
    let op = bytecode_lang::IntOp::new(bytecode_lang::IntTy::I64);
    let (i1, i2, i3) = (f.reg(I64), f.reg(I64), f.reg(I64));
    f.emit(Inst::BoolToInt {
        dst: i1,
        src: b1,
        ty: bytecode_lang::IntTy::I64,
    });
    f.emit(Inst::BoolToInt {
        dst: i2,
        src: b2,
        ty: bytecode_lang::IntTy::I64,
    });
    f.emit(Inst::BoolToInt {
        dst: i3,
        src: b3,
        ty: bytecode_lang::IntTy::I64,
    });
    f.emit(Inst::IAdd {
        dst: out,
        lhs: i1,
        rhs: i1,
        op,
    });
    f.emit(Inst::IAdd {
        dst: out,
        lhs: out,
        rhs: out,
        op,
    });
    f.emit(Inst::IAdd {
        dst: i2,
        lhs: i2,
        rhs: i2,
        op,
    });
    f.emit(Inst::IAdd {
        dst: out,
        lhs: out,
        rhs: i2,
        op,
    });
    f.emit(Inst::IAdd {
        dst: out,
        lhs: out,
        rhs: i3,
        op,
    });
    f.ret(out);
    let id = s.m.add_function(f).unwrap();
    let p = Program::load(s.m.finish().unwrap(), &Host::new()).unwrap();
    // is_type Base: 1, is_type Derived: 1, is_type Other: 0 -> 0b110.
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Int(6)));
    let _ = (s.get, s.x, s.y);
}

#[test]
fn op_cast_passes_nil_and_rejects_scalars() {
    let mut s = shapes();
    let mut f = s.m.function("main", &[D], &[ValType::Ref(s.base)]);
    let t = f.type_ref(s.base);
    let r = f.reg(ValType::Ref(s.base));
    f.emit(Inst::Cast {
        dst: r,
        src: Reg(0),
        ty: t,
    });
    f.ret(r);
    let id = s.m.add_function(f).unwrap();
    let p = Program::load(s.m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    assert_eq!(vm.run(id, &[Value::Nil]), Ok(Value::Nil));
    assert_eq!(
        vm.run(id, &[Value::Int(1)]),
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            func: id,
            pc: 0
        })
    );
}

#[test]
fn op_cast_rejects_an_unrelated_struct() {
    let mut s = shapes();
    let mut f = s.m.function("main", &[], &[]);
    let (base_r, other_r) = (f.type_ref(s.base), f.type_ref(s.other));
    let (obj, casted) = (f.reg(D), f.reg(ValType::Ref(s.base)));
    f.emit(Inst::NewStruct {
        dst: obj,
        ty: other_r,
    });
    f.emit(Inst::Cast {
        dst: casted,
        src: obj,
        ty: base_r,
    });
    f.ret_void();
    let id = s.m.add_function(f).unwrap();
    let p = Program::load(s.m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(id, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            func: id,
            pc: 1
        })
    );
}

#[test]
fn op_get_prop_set_prop_has_prop_on_structs() {
    let mut s = shapes();
    let mut f = s.m.function("main", &[], &[D]);
    let (xn, yn) = (f.name_ref(s.x), f.name_ref(s.y));
    let missing = s.m.string("nope");
    let mn = f.name_ref(missing);
    let get_name = s.m.string("get");
    let gn = f.name_ref(get_name);
    let dt = f.type_ref(s.derived);
    let (obj, v, method, res, has_get, has_nope) = (
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(BOOL),
        f.reg(BOOL),
    );
    let win = f.regs(&[D, D]); // dcall window: result, then the receiver
    f.emit(Inst::NewStruct { dst: obj, ty: dt });
    f.emit(Inst::DLoadInt { dst: v, val: 41 });
    f.emit(Inst::SetProp {
        obj,
        name: xn,
        src: v,
    }); // field x (i64) <- dyn 41
    f.emit(Inst::SetProp {
        obj,
        name: yn,
        src: v,
    }); // field y (dyn)
    f.emit(Inst::GetProp {
        dst: method,
        obj,
        name: gn,
    }); // inherited method
    f.emit(Inst::HasProp {
        dst: has_get,
        obj,
        name: gn,
    });
    f.emit(Inst::HasProp {
        dst: has_nope,
        obj,
        name: mn,
    });
    f.emit(Inst::Mov {
        dst: Reg(win.0 + 1),
        src: obj,
    });
    f.emit(Inst::DCall {
        dst: win,
        callee: method,
        argc: 1,
    }); // Base.get(obj) -> 41 as dyn
    f.emit(Inst::GetProp {
        dst: res,
        obj,
        name: yn,
    });
    f.emit(Inst::DAdd {
        dst: res,
        lhs: res,
        rhs: win,
        pol: Policy::new(),
    }); // 82
    f.emit(Inst::JmpIfNot {
        cond: has_get,
        target: bytecode_lang::Target(13),
    });
    f.emit(Inst::JmpIf {
        cond: has_nope,
        target: bytecode_lang::Target(13),
    });
    f.ret(res);
    f.emit(Inst::GetProp {
        dst: res,
        obj,
        name: mn,
    }); // UndefinedProperty
    f.ret(res);
    let id = s.m.add_function(f).unwrap();
    let p = Program::load(s.m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Int(82)));
}

#[test]
fn set_prop_converts_to_the_field_type() {
    let mut s = shapes();
    let mut f = s.m.function("main", &[], &[]);
    let xn = f.name_ref(s.x);
    let dt = f.type_ref(s.derived);
    let (obj, v) = (f.reg(D), f.reg(D));
    f.emit(Inst::NewStruct { dst: obj, ty: dt });
    f.emit(Inst::LoadNil { dst: v });
    f.emit(Inst::SetProp {
        obj,
        name: xn,
        src: v,
    }); // nil into i64: TypeError
    f.ret_void();
    let id = s.m.add_function(f).unwrap();
    let p = Program::load(s.m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(id, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            func: id,
            pc: 2
        })
    );
}

#[test]
fn property_instructions_on_maps_and_hooks() {
    // get_prop/set_prop/has_prop on a dyn map use string keys; on an int
    // they go to hooks (here: none bound for get, a host one for has).
    let mut host = Host::new();
    host.register("rt", "has", |_, _| Ok(Value::Bool(true)));
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D, D], &[D]);
    let has = m.import("rt", "has", sig);
    m.hook(Hook::HasProp, Callee::Import(has));
    let name = m.string("color");
    let map_t = m.add_type(TypeDef::Map { key: D, value: D });
    let mut f = m.function("main", &[], &[D]);
    let n = f.name_ref(name);
    let mt = f.type_ref(map_t);
    let (map, v, out, i) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    let (h1, h2) = (f.reg(BOOL), f.reg(BOOL));
    f.emit(Inst::NewMap { dst: map, ty: mt });
    f.emit(Inst::DLoadInt { dst: v, val: 7 });
    f.emit(Inst::SetProp {
        obj: map,
        name: n,
        src: v,
    });
    f.emit(Inst::GetProp {
        dst: out,
        obj: map,
        name: n,
    });
    f.emit(Inst::HasProp {
        dst: h1,
        obj: map,
        name: n,
    });
    f.emit(Inst::DLoadInt { dst: i, val: 1 });
    f.emit(Inst::HasProp {
        dst: h2,
        obj: i,
        name: n,
    }); // hook says true
    f.emit(Inst::BAnd {
        dst: h1,
        lhs: h1,
        rhs: h2,
    });
    f.emit(Inst::JmpIfNot {
        cond: h1,
        target: bytecode_lang::Target(10),
    });
    f.ret(out);
    f.emit(Inst::GetProp {
        dst: out,
        obj: i,
        name: n,
    }); // no get_prop hook
    f.ret(out);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Int(7)));
}

#[test]
fn get_prop_hook_receives_the_name_as_a_string() {
    let mut host = Host::new();
    host.register("rt", "get", |ctx, args| {
        let name = ctx.str_bytes(args[1]).unwrap_or(b"?").to_vec();
        ctx.new_str(&name)
    });
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D, D], &[D]);
    let get = m.import("rt", "get", sig);
    m.hook(Hook::GetProp, Callee::Import(get));
    let name = m.string("magic");
    let mut f = m.function("main", &[D], &[D]);
    let n = f.name_ref(name);
    let out = f.reg(D);
    f.emit(Inst::GetProp {
        dst: out,
        obj: Reg(0),
        name: n,
    });
    f.ret(out);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host).unwrap();
    let mut vm = Vm::new(&p);
    let v = vm.run(id, &[Value::Int(3)]).unwrap();
    assert_eq!(vm.str_bytes(v), Some(&b"magic"[..]));
}

#[test]
fn op_dget_index_and_dset_index() {
    let p = common::load(common::module(&[], &[D], |m, f| {
        let one = m.constant(Const::Int(10));
        let two = m.constant(Const::Int(20));
        let arr = m.constant(Const::Array(vec![one, two]));
        let s = m.constant(Const::Bytes(b"AB".to_vec()));
        let (a, k, v, x, sv, out) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D), f.reg(D), f.reg(D));
        f.emit(Inst::DLoadConst { dst: a, k: arr });
        f.emit(Inst::DLoadInt { dst: k, val: 1 });
        f.emit(Inst::DLoadInt { dst: v, val: 5 });
        f.emit(Inst::DSetIndex {
            obj: a,
            key: k,
            src: v,
        }); // a[1] = 5
        f.emit(Inst::DGetIndex {
            dst: x,
            obj: a,
            key: k,
        }); // 5
        f.emit(Inst::DLoadConst { dst: sv, k: s });
        f.emit(Inst::DGetIndex {
            dst: out,
            obj: sv,
            key: k,
        }); // 'B' = 66
        f.emit(Inst::DAdd {
            dst: out,
            lhs: out,
            rhs: x,
            pol: Policy::new(),
        });
        f.ret(out);
    }));
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(71)));
}

#[test]
fn dget_index_misses_raise_or_call_the_hook() {
    let idx = |obj_const: Const, key: i32| {
        common::eval(&[], &[D], &[], move |m, f| {
            let k = m.constant(obj_const.clone());
            let (o, kk, r) = (f.reg(D), f.reg(D), f.reg(D));
            f.emit(Inst::DLoadConst { dst: o, k });
            f.emit(Inst::DLoadInt { dst: kk, val: key });
            f.emit(Inst::DGetIndex {
                dst: r,
                obj: o,
                key: kk,
            });
            f.ret(r);
        })
    };
    assert_eq!(
        idx(Const::Array(Vec::new()), 0),
        raised(ErrorKind::IndexOutOfBounds, 2)
    );
    assert_eq!(
        idx(Const::Map(Vec::new()), 0),
        raised(ErrorKind::KeyNotFound, 2)
    );
    assert_eq!(
        idx(Const::Bytes(b"a".to_vec()), 1),
        raised(ErrorKind::IndexOutOfBounds, 2)
    );
    assert_eq!(idx(Const::Bool(true), 0), raised(ErrorKind::TypeError, 2));
    // With a get_index hook, a miss returns its result.
    assert_eq!(
        with_hook(
            Hook::GetIndex,
            2,
            |d, o, k| Inst::DGetIndex {
                dst: d,
                obj: o,
                key: k
            },
            &[Value::Int(1), Value::Int(99)]
        ),
        Ok(Value::Int(99))
    );
}

#[test]
fn dset_index_misses_raise_or_call_the_hook() {
    let out = common::eval(&[D, D], &[], &[Value::Int(1), Value::Int(2)], |_, f| {
        f.emit(Inst::DSetIndex {
            obj: Reg(0),
            key: Reg(1),
            src: Reg(1),
        });
        f.ret_void();
    });
    assert_eq!(out, raised(ErrorKind::TypeError, 0));
    // A void set_index hook swallows the store.
    let mut m = ModuleBuilder::new();
    let mut main = m.function("main", &[D, D], &[]);
    let h = m.function("set", &[D, D, D], &[]);
    main.emit(Inst::DSetIndex {
        obj: Reg(0),
        key: Reg(1),
        src: Reg(1),
    });
    main.ret_void();
    let mut h = h;
    h.ret_void();
    let hid = h.id();
    m.add_function(main).unwrap();
    m.add_function(h).unwrap();
    m.hook(Hook::SetIndex, Callee::Func(hid));
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[Value::Int(1), Value::Int(2)]),
        Ok(Value::Nil)
    );
}

#[test]
fn op_dlen_and_len_hook() {
    let p = common::load(common::module(&[], &[I64], |m, f| {
        let s = m.constant(Const::Bytes(b"hello".to_vec()));
        let (v, n) = (f.reg(D), f.reg(I64));
        f.emit(Inst::DLoadConst { dst: v, k: s });
        f.emit(Inst::DLen { dst: n, src: v });
        f.ret(n);
    }));
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(5)));
    let no_hook = common::eval(&[D], &[I64], &[Value::Int(1)], |_, f| {
        let n = f.reg(I64);
        f.emit(Inst::DLen {
            dst: n,
            src: Reg(0),
        });
        f.ret(n);
    });
    assert_eq!(no_hook, raised(ErrorKind::TypeError, 0));
    // A len hook must return an int.
    let mut host = Host::new();
    host.register("rt", "len", |_, _| Ok(Value::Int(123)));
    host.register("rt", "bad", |_, _| Ok(Value::Float(1.0)));
    for (name, expect) in [
        ("len", Ok(Value::Int(123))),
        ("bad", raised(ErrorKind::TypeError, 0)),
    ] {
        let mut m = ModuleBuilder::new();
        let sig = m.func_type(&[D], &[D]);
        let imp = m.import("rt", name, sig);
        m.hook(Hook::Len, Callee::Import(imp));
        let mut f = m.function("main", &[D], &[I64]);
        let n = f.reg(I64);
        f.emit(Inst::DLen {
            dst: n,
            src: Reg(0),
        });
        f.ret(n);
        m.add_function(f).unwrap();
        let p = Program::load(m.finish().unwrap(), &host).unwrap();
        assert_eq!(Vm::new(&p).run(FuncId(0), &[Value::Nil]), expect);
    }
}

#[test]
fn op_dcall_converts_arguments_and_result() {
    let mut m = ModuleBuilder::new();
    let mut add = m.function("add", &[I64, I64], &[I64]);
    let r = add.reg(I64);
    add.emit(Inst::IAdd {
        dst: r,
        lhs: Reg(0),
        rhs: Reg(1),
        op: bytecode_lang::IntOp::new(bytecode_lang::IntTy::I64),
    });
    add.ret(r);
    let add_id = add.id();
    let mut main = m.function("main", &[D, D], &[D]);
    let fnv = main.reg(D);
    let win = main.regs(&[D, D, D]);
    main.emit(Inst::MakeClosure {
        dst: fnv,
        func: add_id,
    });
    main.emit(Inst::Mov {
        dst: Reg(win.0 + 1),
        src: Reg(0),
    });
    main.emit(Inst::Mov {
        dst: Reg(win.0 + 2),
        src: Reg(1),
    });
    main.emit(Inst::DCall {
        dst: win,
        callee: fnv,
        argc: 2,
    });
    main.ret(win);
    m.add_function(add).unwrap();
    let main_id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    assert_eq!(
        vm.run(main_id, &[Value::Int(2), Value::Int(3)]),
        Ok(Value::Int(5))
    );
    // A float argument does not convert to i64.
    assert_eq!(
        vm.run(main_id, &[Value::Float(2.0), Value::Int(3)]),
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            func: main_id,
            pc: 3
        })
    );
}

#[test]
fn dcall_arity_mismatch_non_callable_and_call_hook() {
    let mut host = Host::new();
    host.register("rt", "call", |ctx, args| {
        // (callee, args array) -> the callee
        let _ = ctx.kind(args[1]);
        Ok(args[0])
    });
    let build = |hook: bool, argc: u8| {
        let mut m = ModuleBuilder::new();
        if hook {
            let sig = m.func_type(&[D, D], &[D]);
            let imp = m.import("rt", "call", sig);
            m.hook(Hook::Call, Callee::Import(imp));
        }
        let mut main = m.function("main", &[D], &[D]);
        let win = main.regs(&[D, D]);
        main.emit(Inst::DCall {
            dst: win,
            callee: Reg(0),
            argc,
        });
        main.ret(win);
        m.add_function(main).unwrap();
        Program::load(m.finish().unwrap(), &host).unwrap()
    };
    let p = build(false, 1);
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[Value::Int(5)]),
        raised(ErrorKind::TypeError, 0)
    );
    let p = build(true, 1);
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[Value::Int(5)]),
        Ok(Value::Int(5))
    );
}

#[test]
fn op_diter_new_and_iter_hook() {
    // Iterate a dyn map {1: 10, "k": 20}, summing values.
    let p = common::load(common::module(&[], &[D], |m, f| {
        let k1 = m.constant(Const::Int(1));
        let v1 = m.constant(Const::Int(10));
        let kstr = m.constant(Const::Bytes(b"k".to_vec()));
        let v2 = m.constant(Const::Int(20));
        let map = m.constant(Const::Map(vec![(k1, v1), (kstr, v2)]));
        let (mp, it, v, acc, has, key) = (
            f.reg(D),
            f.reg(D),
            f.reg(D),
            f.reg(D),
            f.reg(BOOL),
            f.reg(D),
        );
        f.emit(Inst::DLoadConst { dst: mp, k: map });
        f.emit(Inst::DIterNew { dst: it, src: mp });
        f.emit(Inst::DLoadInt { dst: acc, val: 0 });
        let (top, done) = (f.label(), f.label());
        f.bind(top);
        f.emit(Inst::IterNext {
            has,
            iter: it,
            val: v,
        });
        f.jmp_if_not(has, done);
        f.emit(Inst::IterKey { dst: key, iter: it });
        f.emit(Inst::DAdd {
            dst: acc,
            lhs: acc,
            rhs: v,
            pol: Policy::new(),
        });
        f.emit(Inst::Safepoint {});
        f.jmp(top);
        f.bind(done);
        f.ret(acc);
    }));
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(30)));
    // Without an iter hook a non-collection is a TypeError; with one, the
    // returned array is wrapped.
    let no_hook = common::eval(&[D], &[D], &[Value::Int(1)], |_, f| {
        let r = f.reg(D);
        f.emit(Inst::DIterNew {
            dst: r,
            src: Reg(0),
        });
        f.ret(r);
    });
    assert_eq!(no_hook, raised(ErrorKind::TypeError, 0));
}

#[test]
fn hook_errors_propagate_and_host_errors_are_catchable() {
    let mut host = Host::new();
    host.register("rt", "add", |_, _| {
        Err(HostError::Raise(ErrorKind::KeyNotFound))
    });
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D, D], &[D]);
    let imp = m.import("rt", "add", sig);
    m.hook(Hook::Add, Callee::Import(imp));
    let mut f = m.function("main", &[D, D], &[D]);
    let r = f.reg(D);
    f.emit(Inst::DAdd {
        dst: r,
        lhs: Reg(0),
        rhs: Reg(1),
        pol: Policy::new(),
    });
    f.ret(r);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host).unwrap();
    assert_eq!(
        Vm::new(&p).run(id, &[Value::Nil, Value::Nil]),
        Err(VmError::Raised {
            kind: ErrorKind::KeyNotFound,
            func: id,
            pc: 0
        })
    );
}

#[test]
fn le_hook_serves_dle_and_dge() {
    // A `le` hook that always says true: dle(nil, 1) and dge(1, nil) both
    // reach it (no built-in rule for nil).
    let mut host = Host::new();
    host.register("rt", "le", |_, _| Ok(Value::Bool(true)));
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D, D], &[D]);
    let le = m.import("rt", "le", sig);
    m.hook(Hook::Le, Callee::Import(le));
    let mut f = m.function("main", &[D, D], &[BOOL]);
    let (a, b, r) = (f.reg(BOOL), f.reg(BOOL), f.reg(BOOL));
    f.emit(Inst::DLe {
        dst: a,
        lhs: Reg(0),
        rhs: Reg(1),
    });
    f.emit(Inst::DGe {
        dst: b,
        lhs: Reg(1),
        rhs: Reg(0),
    });
    f.emit(Inst::BAnd {
        dst: r,
        lhs: a,
        rhs: b,
    });
    f.ret(r);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host).unwrap();
    assert_eq!(
        Vm::new(&p).run(id, &[Value::Nil, Value::Int(1)]),
        Ok(Value::Bool(true))
    );
}

#[test]
fn set_prop_hook_receives_object_name_and_value() {
    // The hook stores what it saw into a global so the test can read it.
    let mut m = ModuleBuilder::new();
    let g = m.global("seen", D, true, None);
    let name = m.string("field");
    let mut h = m.function("set", &[D, D, D], &[]);
    h.emit(Inst::SetGlobal {
        global: g,
        src: Reg(1),
    });
    h.ret_void();
    let hid = h.id();
    m.add_function(h).unwrap();
    m.hook(Hook::SetProp, Callee::Func(hid));
    let mut f = m.function("main", &[D], &[]);
    let n = f.name_ref(name);
    f.emit(Inst::SetProp {
        obj: Reg(0),
        name: n,
        src: Reg(0),
    });
    f.ret_void();
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    assert_eq!(vm.run(id, &[Value::Int(5)]), Ok(Value::Nil));
    let seen = vm.global(g).unwrap();
    assert_eq!(vm.str_bytes(seen), Some(&b"field"[..]));
}

#[test]
fn iter_hook_results_are_wrapped_or_rejected() {
    // An `iter` hook that returns a constant array [7, 8]: diter_new over an
    // int iterates it. A hook returning an int is a TypeError.
    for (returns_array, expect) in [
        (true, Ok(Value::Int(15))),
        (false, raised(ErrorKind::TypeError, 0)),
    ] {
        let mut m = ModuleBuilder::new();
        let seven = m.constant(Const::Int(7));
        let eight = m.constant(Const::Int(8));
        let arr = m.constant(Const::Array(vec![seven, eight]));
        let mut h = m.function("iter", &[D], &[D]);
        let r = h.reg(D);
        if returns_array {
            h.emit(Inst::DLoadConst { dst: r, k: arr });
        } else {
            h.emit(Inst::DLoadInt { dst: r, val: 1 });
        }
        h.ret(r);
        let hid = h.id();
        m.add_function(h).unwrap();
        m.hook(Hook::Iter, Callee::Func(hid));
        let mut f = m.function("main", &[D], &[D]);
        let (it, v, acc, has) = (f.reg(D), f.reg(D), f.reg(D), f.reg(BOOL));
        f.emit(Inst::DIterNew {
            dst: it,
            src: Reg(0),
        });
        f.emit(Inst::DLoadInt { dst: acc, val: 0 });
        let (top, done) = (f.label(), f.label());
        f.bind(top);
        f.emit(Inst::IterNext {
            has,
            iter: it,
            val: v,
        });
        f.jmp_if_not(has, done);
        f.emit(Inst::DAdd {
            dst: acc,
            lhs: acc,
            rhs: v,
            pol: Policy::new(),
        });
        f.emit(Inst::Safepoint {});
        f.jmp(top);
        f.bind(done);
        f.ret(acc);
        let id = m.add_function(f).unwrap();
        let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
        let out = Vm::new(&p).run(id, &[Value::Int(3)]);
        match expect {
            Ok(v) => assert_eq!(out, Ok(v)),
            Err(_) => assert_eq!(
                out,
                Err(VmError::Raised {
                    kind: ErrorKind::TypeError,
                    func: id,
                    pc: 0
                })
            ),
        }
    }
}
