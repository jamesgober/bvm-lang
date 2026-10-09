//! LSB conformance, scalar instructions: moves, constants, globals, typed
//! integer and float arithmetic, booleans, chars, references, conversions
//! (LSB §5.1–§5.5). Each test names the instruction(s) it covers; the OPS
//! edge-value table is in `ops_table.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::type_complexity
)]

mod common;

use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{
    Const, ErrorKind, FloatConv, FloatTy, FuncId, Inst, IntConv, IntOp, IntPair, IntTy,
    ModuleBuilder, Overflow, Policy, Reg, ValType,
};
use common::{BOOL, F64, I64, eval};

fn i64op() -> IntOp {
    IntOp::new(IntTy::I64)
}

/// Runs a function `(param types) -> ret` whose body is `insts` followed by
/// `ret r{ret_reg}` over registers `regs` (params first).
fn run_regs(
    regs: &[ValType],
    nparams: usize,
    ret: ValType,
    args: &[Value],
    insts: &[Inst],
    ret_reg: u16,
) -> Result<Value, VmError> {
    eval(&regs[..nparams], &[ret], args, |_, f| {
        for &t in &regs[nparams..] {
            let _ = f.reg(t);
        }
        for &i in insts {
            f.emit(i);
        }
        f.ret(Reg(ret_reg));
    })
}

#[test]
fn op_nop_and_mov() {
    let out = run_regs(
        &[I64, I64],
        1,
        I64,
        &[Value::Int(9)],
        &[
            Inst::Nop {},
            Inst::Mov {
                dst: Reg(1),
                src: Reg(0),
            },
        ],
        1,
    );
    assert_eq!(out, Ok(Value::Int(9)));
}

#[test]
fn op_mov_str_into_dyn_keeps_the_reference() {
    let out = eval(&[], &[ValType::Dyn], &[], |m, f| {
        let hi = m.string("hi");
        let k = m.constant(Const::Str(hi));
        let (s, d) = (f.reg(ValType::Str), f.reg(ValType::Dyn));
        f.emit(Inst::LoadConst { dst: s, k });
        f.emit(Inst::Mov { dst: d, src: s });
        f.ret(d);
    })
    .unwrap();
    assert!(matches!(out, Value::Obj(_)));
}

#[test]
fn op_load_const_typed_scalars() {
    let cases: Vec<(Const, ValType, Value)> = vec![
        (Const::Bool(true), BOOL, Value::Bool(true)),
        (Const::Int(-5), I64, Value::Int(-5)),
        (Const::Int(-5), ValType::I8, Value::Int(-5)),
        (Const::UInt(u64::MAX), ValType::U64, Value::UInt(u64::MAX)),
        (Const::f32(1.5), ValType::F32, Value::F32(1.5)),
        (Const::f64(-0.0), F64, Value::Float(-0.0)),
        (Const::Char('λ'), ValType::Char, Value::Char('λ')),
    ];
    for (c, ty, expect) in cases {
        let out = eval(&[], &[ty], &[], |m, f| {
            let k = m.constant(c.clone());
            let r = f.reg(ty);
            f.emit(Inst::LoadConst { dst: r, k });
            f.ret(r);
        });
        assert_eq!(out, Ok(expect), "{c:?} into {ty}");
    }
    // NaN payloads and the sign of zero survive a typed load.
    let nan = f64::from_bits(0x7FF8_0000_0000_0ABC);
    let out = eval(&[], &[F64], &[], |m, f| {
        let k = m.constant(Const::f64(nan));
        let r = f.reg(F64);
        f.emit(Inst::LoadConst { dst: r, k });
        f.ret(r);
    });
    assert!(matches!(out, Ok(Value::Float(x)) if x.to_bits() == nan.to_bits()));
}

#[test]
fn op_load_const_strings_and_bytes() {
    let p = common::load(common::module(&[], &[ValType::Str], |m, f| {
        let k = m.constant(Const::Bytes(vec![0xFF, 0x00, b'a']));
        let r = f.reg(ValType::Str);
        f.emit(Inst::LoadConst { dst: r, k });
        f.ret(r);
    }));
    let mut vm = Vm::new(&p);
    let s = vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(vm.str_bytes(s), Some(&[0xFF, 0x00, b'a'][..]));
}

#[test]
fn op_load_const_typed_aggregate() {
    let mut m = ModuleBuilder::new();
    let arr_t = m.add_type(bytecode_lang::TypeDef::Array(I64));
    let items: Vec<_> = (1..=3).map(|i| m.constant(Const::Int(i))).collect();
    let k = m.constant(Const::Array(items));
    let mut f = m.function("f", &[], &[ValType::Ref(arr_t)]);
    let r = f.reg(ValType::Ref(arr_t));
    f.emit(Inst::LoadConst { dst: r, k });
    f.ret(r);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    let v = vm.run(id, &[]).unwrap();
    assert_eq!(
        vm.elements(v),
        Some(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
}

#[test]
fn op_dload_const_kinds() {
    let big = 1i64 << 60;
    let cases: Vec<(Const, Value)> = vec![
        (Const::Bool(false), Value::Bool(false)),
        (Const::Int(big), Value::Int(big)),
        (Const::Int(i64::MIN), Value::Int(i64::MIN)),
        (Const::UInt(7), Value::Int(7)),
        (Const::f32(0.5), Value::Float(0.5)),
        (Const::Char('x'), Value::Char('x')),
    ];
    for (c, expect) in cases {
        let out = eval(&[], &[ValType::Dyn], &[], |m, f| {
            let k = m.constant(c.clone());
            let r = f.reg(ValType::Dyn);
            f.emit(Inst::DLoadConst { dst: r, k });
            f.ret(r);
        });
        assert_eq!(out, Ok(expect), "{c:?}");
    }
    // A uint above i64::MAX has no dyn int: ArithOverflow at the load.
    let out = eval(&[], &[ValType::Dyn], &[], |m, f| {
        let k = m.constant(Const::UInt(u64::MAX));
        let r = f.reg(ValType::Dyn);
        f.emit(Inst::DLoadConst { dst: r, k });
        f.ret(r);
    });
    assert_eq!(
        out,
        Err(VmError::Raised {
            kind: ErrorKind::ArithOverflow,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 0
        })
    );
}

#[test]
fn op_dload_const_aggregates_are_copy_on_write_per_load() {
    // Load the same constant array twice, push onto the first: the second
    // load must be unaffected (each load is a fresh identity).
    let p = common::load(common::module(&[], &[ValType::Dyn], |m, f| {
        let one = m.constant(Const::Int(1));
        let k = m.constant(Const::Array(vec![one]));
        let d = f.reg(ValType::Dyn);
        let (a, b, x) = (
            f.reg(ValType::Dyn),
            f.reg(ValType::Dyn),
            f.reg(ValType::Dyn),
        );
        let _ = d;
        f.emit(Inst::DLoadConst { dst: a, k });
        f.emit(Inst::DLoadInt { dst: x, val: 9 });
        f.emit(Inst::ArrayPush { arr: a, src: x });
        f.emit(Inst::DLoadConst { dst: b, k });
        f.ret(b);
    }));
    let mut vm = Vm::new(&p);
    let v = vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(vm.elements(v), Some(vec![Value::Int(1)]));
}

#[test]
fn op_dload_const_nested_constant_is_immutable_without_dup() {
    let out = eval(&[], &[ValType::Dyn], &[], |m, f| {
        let one = m.constant(Const::Int(1));
        let inner = m.constant(Const::Array(vec![one]));
        let outer = m.constant(Const::Array(vec![inner]));
        let (o, i, zero, x) = (
            f.reg(ValType::Dyn),
            f.reg(ValType::Dyn),
            f.reg(I64),
            f.reg(ValType::Dyn),
        );
        f.emit(Inst::DLoadConst { dst: o, k: outer });
        f.emit(Inst::ArrayGet {
            dst: i,
            arr: o,
            idx: zero,
        });
        f.emit(Inst::ArrayPush { arr: i, src: x });
        f.ret(o);
    });
    assert_eq!(
        out,
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 2
        })
    );
}

#[test]
fn op_load_int_dload_int_load_bool_load_nil() {
    assert_eq!(
        run_regs(
            &[ValType::I8],
            0,
            ValType::I8,
            &[],
            &[Inst::LoadInt {
                dst: Reg(0),
                val: -1,
                ty: IntTy::I8
            }],
            0
        ),
        Ok(Value::Int(-1))
    );
    assert_eq!(
        run_regs(
            &[ValType::U8],
            0,
            ValType::U8,
            &[],
            &[Inst::LoadInt {
                dst: Reg(0),
                val: 255,
                ty: IntTy::U8
            }],
            0
        ),
        Ok(Value::UInt(255))
    );
    assert_eq!(
        run_regs(
            &[ValType::Dyn],
            0,
            ValType::Dyn,
            &[],
            &[Inst::DLoadInt {
                dst: Reg(0),
                val: i32::MIN
            }],
            0
        ),
        Ok(Value::Int(i64::from(i32::MIN)))
    );
    assert_eq!(
        run_regs(
            &[BOOL],
            0,
            BOOL,
            &[],
            &[Inst::LoadBool {
                dst: Reg(0),
                val: true
            }],
            0
        ),
        Ok(Value::Bool(true))
    );
    assert_eq!(
        run_regs(
            &[ValType::Dyn],
            0,
            ValType::Dyn,
            &[],
            &[
                Inst::DLoadInt {
                    dst: Reg(0),
                    val: 3
                },
                Inst::LoadNil { dst: Reg(0) }
            ],
            0
        ),
        Ok(Value::Nil)
    );
}

#[test]
fn op_globals_initialise_update_and_persist() {
    let mut m = ModuleBuilder::new();
    let k = m.constant(Const::Int(10));
    let g = m.global("counter", I64, true, Some(k));
    let mut f = m.function("bump", &[], &[I64]);
    let (v, one) = (f.reg(I64), f.reg(I64));
    f.emit(Inst::GetGlobal { dst: v, global: g });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::IAdd {
        dst: v,
        lhs: v,
        rhs: one,
        op: i64op(),
    });
    f.emit(Inst::SetGlobal { global: g, src: v });
    f.ret(v);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    assert_eq!(vm.run(id, &[]), Ok(Value::Int(11)));
    assert_eq!(vm.run(id, &[]), Ok(Value::Int(12)));
    assert_eq!(vm.global(g), Some(Value::Int(12)));
}

#[test]
fn op_global_without_initialiser_is_default() {
    let mut m = ModuleBuilder::new();
    let g = m.global("s", ValType::Str, true, None);
    let mut f = m.function("f", &[], &[ValType::Str]);
    let v = f.reg(ValType::Str);
    f.emit(Inst::GetGlobal { dst: v, global: g });
    f.ret(v);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Nil));
}

fn ibin(
    make: fn(Reg, Reg, Reg, IntOp) -> Inst,
    op: IntOp,
    a: Value,
    b: Value,
) -> Result<Value, VmError> {
    let t = ValType::int(op.ty());
    run_regs(
        &[t, t, t],
        2,
        t,
        &[a, b],
        &[make(Reg(2), Reg(0), Reg(1), op)],
        2,
    )
}

#[test]
fn op_integer_binary_instructions() {
    let w = |p: Policy| IntOp::new(IntTy::I64).with_policy(p);
    let p = Policy::new();
    let i = Value::Int;
    let cases: Vec<(fn(Reg, Reg, Reg, IntOp) -> Inst, i64, i64, i64)> = vec![
        (
            |d, l, r, op| Inst::IAdd {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            2,
            3,
            5,
        ),
        (
            |d, l, r, op| Inst::ISub {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            2,
            3,
            -1,
        ),
        (
            |d, l, r, op| Inst::IMul {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            -4,
            3,
            -12,
        ),
        (
            |d, l, r, op| Inst::IDiv {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            -7,
            2,
            -3,
        ),
        (
            |d, l, r, op| Inst::IRem {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            -7,
            2,
            -1,
        ),
        (
            |d, l, r, op| Inst::IFloorDiv {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            -7,
            2,
            -4,
        ),
        (
            |d, l, r, op| Inst::IFloorMod {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            -7,
            2,
            1,
        ),
        (
            |d, l, r, op| Inst::IAnd {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            0b1100,
            0b1010,
            0b1000,
        ),
        (
            |d, l, r, op| Inst::IOr {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            0b1100,
            0b1010,
            0b1110,
        ),
        (
            |d, l, r, op| Inst::IXor {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            0b1100,
            0b1010,
            0b0110,
        ),
        (
            |d, l, r, op| Inst::IShl {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            3,
            4,
            48,
        ),
        (
            |d, l, r, op| Inst::IShr {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            -16,
            2,
            -4,
        ),
        (
            |d, l, r, op| Inst::IMin {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            -1,
            5,
            -1,
        ),
        (
            |d, l, r, op| Inst::IMax {
                dst: d,
                lhs: l,
                rhs: r,
                op,
            },
            -1,
            5,
            5,
        ),
    ];
    for (make, a, b, expect) in cases {
        assert_eq!(
            ibin(make, w(p), i(a), i(b)),
            Ok(i(expect)),
            "{:?}",
            make(Reg(0), Reg(0), Reg(0), w(p))
        );
    }
}

#[test]
fn op_integer_errors_raise_at_their_pc_and_trap_under_trap() {
    let add = |d, l, r, op| Inst::IAdd {
        dst: d,
        lhs: l,
        rhs: r,
        op,
    };
    let max = Value::Int(i64::MAX);
    assert_eq!(
        ibin(add, i64op(), max, Value::Int(1)),
        Err(VmError::Raised {
            kind: ErrorKind::ArithOverflow,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 0
        })
    );
    let trap = i64op().with_policy(Policy::new().with_overflow(Overflow::Trap));
    assert_eq!(
        ibin(add, trap, max, Value::Int(1)),
        Err(VmError::Trap {
            kind: ErrorKind::ArithOverflow,
            func: FuncId(0),
            pc: 0
        })
    );
}

#[test]
fn op_integer_unary_instructions() {
    let t = I64;
    let un = |make: fn(Reg, Reg, IntOp) -> Inst, a: i64| {
        run_regs(
            &[t, t],
            1,
            t,
            &[Value::Int(a)],
            &[make(Reg(1), Reg(0), i64op())],
            1,
        )
    };
    assert_eq!(
        un(|d, s, op| Inst::INeg { dst: d, src: s, op }, 5),
        Ok(Value::Int(-5))
    );
    assert_eq!(
        un(|d, s, op| Inst::IBitNot { dst: d, src: s, op }, 0),
        Ok(Value::Int(-1))
    );
    assert_eq!(
        un(|d, s, op| Inst::IAbs { dst: d, src: s, op }, -8),
        Ok(Value::Int(8))
    );
    assert_eq!(
        un(|d, s, op| Inst::IAbs { dst: d, src: s, op }, i64::MIN),
        Err(VmError::Raised {
            kind: ErrorKind::ArithOverflow,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 0
        })
    );
}

#[test]
fn op_integer_comparisons_respect_signedness() {
    type Mk = fn(Reg, Reg, Reg, IntTy) -> Inst;
    let cmps: [(Mk, [bool; 3]); 6] = [
        (
            |d, l, r, ty| Inst::IEq {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [false, true, false],
        ),
        (
            |d, l, r, ty| Inst::INe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [true, false, true],
        ),
        (
            |d, l, r, ty| Inst::ILt {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [true, false, false],
        ),
        (
            |d, l, r, ty| Inst::ILe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [true, true, false],
        ),
        (
            |d, l, r, ty| Inst::IGt {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [false, false, true],
        ),
        (
            |d, l, r, ty| Inst::IGe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [false, true, true],
        ),
    ];
    for (make, expect) in cmps {
        for (k, (a, b)) in [(1i64, 2i64), (2, 2), (3, 2)].into_iter().enumerate() {
            let out = run_regs(
                &[I64, I64, BOOL],
                2,
                BOOL,
                &[Value::Int(a), Value::Int(b)],
                &[make(Reg(2), Reg(0), Reg(1), IntTy::I64)],
                2,
            );
            assert_eq!(out, Ok(Value::Bool(expect[k])));
        }
    }
    // u64: 2^63 is above 1; as i64 it would be below.
    let big = Value::UInt(1 << 63);
    let lt = |ty: IntTy, vt: ValType, a: Value, b: Value| {
        run_regs(
            &[vt, vt, BOOL],
            2,
            BOOL,
            &[a, b],
            &[Inst::ILt {
                dst: Reg(2),
                lhs: Reg(0),
                rhs: Reg(1),
                ty,
            }],
            2,
        )
    };
    assert_eq!(
        lt(IntTy::U64, ValType::U64, Value::UInt(1), big),
        Ok(Value::Bool(true))
    );
    assert_eq!(
        lt(IntTy::I64, I64, Value::Int(1), Value::Int(i64::MIN)),
        Ok(Value::Bool(false))
    );
}

fn fbin(make: fn(Reg, Reg, Reg, FloatTy) -> Inst, ty: FloatTy, a: f64, b: f64) -> Value {
    let (vt, va, vb) = match ty {
        FloatTy::F32 => (ValType::F32, Value::F32(a as f32), Value::F32(b as f32)),
        FloatTy::F64 => (F64, Value::Float(a), Value::Float(b)),
    };
    run_regs(
        &[vt, vt, vt],
        2,
        vt,
        &[va, vb],
        &[make(Reg(2), Reg(0), Reg(1), ty)],
        2,
    )
    .unwrap()
}

#[test]
fn op_float_binary_instructions() {
    type Mk = fn(Reg, Reg, Reg, FloatTy) -> Inst;
    let cases: Vec<(Mk, f64, f64, f64)> = vec![
        (
            |d, l, r, ty| Inst::FAdd {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            1.5,
            2.25,
            3.75,
        ),
        (
            |d, l, r, ty| Inst::FSub {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            1.5,
            2.25,
            -0.75,
        ),
        (
            |d, l, r, ty| Inst::FMul {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            1.5,
            -2.0,
            -3.0,
        ),
        (
            |d, l, r, ty| Inst::FDiv {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            1.0,
            0.0,
            f64::INFINITY,
        ),
        (
            |d, l, r, ty| Inst::FRem {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            -7.0,
            2.0,
            -1.0,
        ),
        (
            |d, l, r, ty| Inst::FIeeeRem {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            7.0,
            2.0,
            -1.0,
        ),
        (
            |d, l, r, ty| Inst::FMin {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            0.0,
            -0.0,
            -0.0,
        ),
        (
            |d, l, r, ty| Inst::FMax {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            -0.0,
            0.0,
            0.0,
        ),
    ];
    for (make, a, b, expect) in cases {
        for ty in [FloatTy::F32, FloatTy::F64] {
            let got = fbin(make, ty, a, b).as_float().unwrap();
            assert_eq!(
                got.to_bits(),
                expect.to_bits(),
                "{:?} {ty}",
                make(Reg(0), Reg(0), Reg(0), ty)
            );
        }
    }
    let nan = fbin(
        |d, l, r, ty| Inst::FMin {
            dst: d,
            lhs: l,
            rhs: r,
            ty,
        },
        FloatTy::F64,
        f64::NAN,
        1.0,
    );
    assert!(nan.as_float().unwrap().is_nan());
}

#[test]
fn op_ffma_uses_dst_as_addend_with_one_rounding() {
    // (1 + 2^-52) * (1 - 2^-52) + (-1) = -2^-104 exactly with one rounding;
    // two roundings would give 0.
    let a = 1.0 + f64::EPSILON;
    let b = 1.0 - f64::EPSILON;
    let out = run_regs(
        &[F64, F64, F64],
        3,
        F64,
        &[Value::Float(a), Value::Float(b), Value::Float(-1.0)],
        &[Inst::FFma {
            dst: Reg(2),
            lhs: Reg(0),
            rhs: Reg(1),
            ty: FloatTy::F64,
        }],
        2,
    );
    assert_eq!(out, Ok(Value::Float(-(2f64.powi(-104)))));
}

#[test]
fn op_float_unary_instructions() {
    type Mk = fn(Reg, Reg, FloatTy) -> Inst;
    let cases: Vec<(Mk, f64, f64)> = vec![
        (|d, s, ty| Inst::FNeg { dst: d, src: s, ty }, 0.0, -0.0),
        (|d, s, ty| Inst::FAbs { dst: d, src: s, ty }, -2.5, 2.5),
        (|d, s, ty| Inst::FSqrt { dst: d, src: s, ty }, 2.25, 1.5),
        (|d, s, ty| Inst::FFloor { dst: d, src: s, ty }, -1.5, -2.0),
        (|d, s, ty| Inst::FCeil { dst: d, src: s, ty }, -1.5, -1.0),
        (|d, s, ty| Inst::FTrunc { dst: d, src: s, ty }, -1.5, -1.0),
        (|d, s, ty| Inst::FRound { dst: d, src: s, ty }, 2.5, 3.0),
        (|d, s, ty| Inst::FRoundEven { dst: d, src: s, ty }, 2.5, 2.0),
    ];
    for (make, a, expect) in cases {
        for (ty, vt, va) in [
            (FloatTy::F32, ValType::F32, Value::F32(a as f32)),
            (FloatTy::F64, F64, Value::Float(a)),
        ] {
            let got = run_regs(&[vt, vt], 1, vt, &[va], &[make(Reg(1), Reg(0), ty)], 1).unwrap();
            assert_eq!(
                got.as_float().unwrap().to_bits(),
                expect.to_bits(),
                "{:?}",
                make(Reg(0), Reg(0), ty)
            );
        }
    }
    // fneg and fabs keep NaN payloads (sign-bit operations).
    let nan = f64::from_bits(0x7FF8_0000_0000_1234);
    let got = run_regs(
        &[F64, F64],
        1,
        F64,
        &[Value::Float(nan)],
        &[Inst::FNeg {
            dst: Reg(1),
            src: Reg(0),
            ty: FloatTy::F64,
        }],
        1,
    )
    .unwrap();
    assert_eq!(got.as_float().unwrap().to_bits(), nan.to_bits() ^ (1 << 63));
}

#[test]
fn op_float_comparisons_and_total_cmp() {
    type Mk = fn(Reg, Reg, Reg, FloatTy) -> Inst;
    // (instruction, 1 vs 2, 2 vs 2, NaN vs 2)
    let cmps: [(Mk, [bool; 3]); 6] = [
        (
            |d, l, r, ty| Inst::FEq {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [false, true, false],
        ),
        (
            |d, l, r, ty| Inst::FNe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [true, false, true],
        ),
        (
            |d, l, r, ty| Inst::FLt {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [true, false, false],
        ),
        (
            |d, l, r, ty| Inst::FLe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [true, true, false],
        ),
        (
            |d, l, r, ty| Inst::FGt {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [false, false, false],
        ),
        (
            |d, l, r, ty| Inst::FGe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            [false, true, false],
        ),
    ];
    for (make, expect) in cmps {
        for (k, a) in [1.0, 2.0, f64::NAN].into_iter().enumerate() {
            let out = run_regs(
                &[F64, F64, BOOL],
                2,
                BOOL,
                &[Value::Float(a), Value::Float(2.0)],
                &[make(Reg(2), Reg(0), Reg(1), FloatTy::F64)],
                2,
            );
            assert_eq!(out, Ok(Value::Bool(expect[k])));
        }
    }
    let tc = |a: f64, b: f64| {
        run_regs(
            &[F64, F64, ValType::I8],
            2,
            ValType::I8,
            &[Value::Float(a), Value::Float(b)],
            &[Inst::FTotalCmp {
                dst: Reg(2),
                lhs: Reg(0),
                rhs: Reg(1),
                ty: FloatTy::F64,
            }],
            2,
        )
    };
    assert_eq!(tc(-0.0, 0.0), Ok(Value::Int(-1)));
    assert_eq!(tc(f64::NAN, f64::INFINITY), Ok(Value::Int(1)));
    assert_eq!(tc(1.0, 1.0), Ok(Value::Int(0)));
}

#[test]
fn op_booleans() {
    let b = Value::Bool;
    let bin = |make: fn(Reg, Reg, Reg) -> Inst, x: bool, y: bool| {
        run_regs(
            &[BOOL, BOOL, BOOL],
            2,
            BOOL,
            &[b(x), b(y)],
            &[make(Reg(2), Reg(0), Reg(1))],
            2,
        )
        .unwrap()
    };
    for (x, y) in [(false, false), (false, true), (true, false), (true, true)] {
        assert_eq!(
            bin(
                |d, l, r| Inst::BAnd {
                    dst: d,
                    lhs: l,
                    rhs: r
                },
                x,
                y
            ),
            b(x && y)
        );
        assert_eq!(
            bin(
                |d, l, r| Inst::BOr {
                    dst: d,
                    lhs: l,
                    rhs: r
                },
                x,
                y
            ),
            b(x || y)
        );
        assert_eq!(
            bin(
                |d, l, r| Inst::BXor {
                    dst: d,
                    lhs: l,
                    rhs: r
                },
                x,
                y
            ),
            b(x != y)
        );
    }
    let not = run_regs(
        &[BOOL, BOOL],
        1,
        BOOL,
        &[b(true)],
        &[Inst::BNot {
            dst: Reg(1),
            src: Reg(0),
        }],
        1,
    );
    assert_eq!(not, Ok(b(false)));
}

#[test]
fn op_char_comparisons() {
    type Mk = fn(Reg, Reg, Reg) -> Inst;
    let c = ValType::Char;
    let cmps: [(Mk, [bool; 3]); 6] = [
        (
            |d, l, r| Inst::CEq {
                dst: d,
                lhs: l,
                rhs: r,
            },
            [false, true, false],
        ),
        (
            |d, l, r| Inst::CNe {
                dst: d,
                lhs: l,
                rhs: r,
            },
            [true, false, true],
        ),
        (
            |d, l, r| Inst::CLt {
                dst: d,
                lhs: l,
                rhs: r,
            },
            [true, false, false],
        ),
        (
            |d, l, r| Inst::CLe {
                dst: d,
                lhs: l,
                rhs: r,
            },
            [true, true, false],
        ),
        (
            |d, l, r| Inst::CGt {
                dst: d,
                lhs: l,
                rhs: r,
            },
            [false, false, true],
        ),
        (
            |d, l, r| Inst::CGe {
                dst: d,
                lhs: l,
                rhs: r,
            },
            [false, true, true],
        ),
    ];
    for (make, expect) in cmps {
        for (k, a) in ['a', 'b', '€'].into_iter().enumerate() {
            let out = run_regs(
                &[c, c, BOOL],
                2,
                BOOL,
                &[Value::Char(a), Value::Char('b')],
                &[make(Reg(2), Reg(0), Reg(1))],
                2,
            );
            assert_eq!(out, Ok(Value::Bool(expect[k])));
        }
    }
}

#[test]
fn op_ref_eq_is_identity() {
    let out = eval(&[], &[BOOL], &[], |m, f| {
        let k1 = m.constant(Const::Bytes(b"same".to_vec()));
        let (a, b, c, r1, r2) = (
            f.reg(ValType::Str),
            f.reg(ValType::Str),
            f.reg(ValType::Str),
            f.reg(BOOL),
            f.reg(BOOL),
        );
        f.emit(Inst::LoadConst { dst: a, k: k1 });
        f.emit(Inst::StrConcat {
            dst: b,
            lhs: a,
            rhs: c,
        }); // nil rhs: NullReference
        f.emit(Inst::RefEq {
            dst: r1,
            lhs: a,
            rhs: a,
        });
        f.emit(Inst::RefEq {
            dst: r2,
            lhs: r1,
            rhs: r1,
        });
        f.ret(r1);
    });
    // The concat with a nil operand raises before the comparison.
    assert_eq!(
        out,
        Err(VmError::Raised {
            kind: ErrorKind::NullReference,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 1
        })
    );

    let out = eval(&[], &[BOOL], &[], |m, f| {
        let k = m.constant(Const::Bytes(b"x".to_vec()));
        let (a, b, n1, n2, eq_ab, eq_nil, both) = (
            f.reg(ValType::Str),
            f.reg(ValType::Str),
            f.reg(ValType::Str),
            f.reg(ValType::Str),
            f.reg(BOOL),
            f.reg(BOOL),
            f.reg(BOOL),
        );
        f.emit(Inst::LoadConst { dst: a, k });
        f.emit(Inst::StrConcat {
            dst: b,
            lhs: a,
            rhs: a,
        });
        f.emit(Inst::StrSlice {
            dst: b,
            s: b,
            range: n1,
            utf8: false,
        }); // "" from 0..0 (nil regs read as 0)
        f.emit(Inst::RefEq {
            dst: eq_ab,
            lhs: a,
            rhs: b,
        });
        f.emit(Inst::RefEq {
            dst: eq_nil,
            lhs: n1,
            rhs: n2,
        });
        f.emit(Inst::BXor {
            dst: both,
            lhs: eq_ab,
            rhs: eq_nil,
        });
        f.ret(both);
    });
    // a != b (different objects), nil == nil: xor is true.
    assert_eq!(out, Ok(Value::Bool(true)));
}

#[test]
fn op_int_cast_zext_sext_trunc() {
    let cast = |conv: IntConv, from: ValType, to: ValType, v: Value| {
        run_regs(
            &[from, to],
            1,
            to,
            &[v],
            &[Inst::IntCast {
                dst: Reg(1),
                src: Reg(0),
                conv,
            }],
            1,
        )
    };
    assert_eq!(
        cast(
            IntConv::new(IntTy::I64, IntTy::U8, Overflow::Error),
            I64,
            ValType::U8,
            Value::Int(200)
        ),
        Ok(Value::UInt(200))
    );
    assert_eq!(
        cast(
            IntConv::new(IntTy::I64, IntTy::U8, Overflow::Wrap),
            I64,
            ValType::U8,
            Value::Int(-1)
        ),
        Ok(Value::UInt(255))
    );
    assert_eq!(
        cast(
            IntConv::new(IntTy::I64, IntTy::I8, Overflow::Error),
            I64,
            ValType::I8,
            Value::Int(128)
        ),
        Err(VmError::Raised {
            kind: ErrorKind::ArithOverflow,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 0
        })
    );
    assert_eq!(
        cast(
            IntConv::new(IntTy::I64, IntTy::I8, Overflow::Trap),
            I64,
            ValType::I8,
            Value::Int(128)
        ),
        Err(VmError::Trap {
            kind: ErrorKind::ArithOverflow,
            func: FuncId(0),
            pc: 0
        })
    );
    let pair =
        |make: fn(Reg, Reg, IntPair) -> Inst, p: IntPair, from: ValType, to: ValType, v: Value| {
            run_regs(&[from, to], 1, to, &[v], &[make(Reg(1), Reg(0), p)], 1)
        };
    let i8_to_i32 = IntPair::new(IntTy::I8, IntTy::I32);
    assert_eq!(
        pair(
            |d, s, pair| Inst::Zext {
                dst: d,
                src: s,
                pair
            },
            i8_to_i32,
            ValType::I8,
            ValType::I32,
            Value::Int(-1)
        ),
        Ok(Value::Int(255))
    );
    assert_eq!(
        pair(
            |d, s, pair| Inst::Sext {
                dst: d,
                src: s,
                pair
            },
            i8_to_i32,
            ValType::I8,
            ValType::I32,
            Value::Int(-1)
        ),
        Ok(Value::Int(-1))
    );
    assert_eq!(
        pair(
            |d, s, pair| Inst::Trunc {
                dst: d,
                src: s,
                pair
            },
            IntPair::new(IntTy::I64, IntTy::U8),
            I64,
            ValType::U8,
            Value::Int(0x1_23)
        ),
        Ok(Value::UInt(0x23))
    );
}

#[test]
fn op_int_to_float_rounds_to_nearest_even() {
    let to64 = run_regs(
        &[I64, F64],
        1,
        F64,
        &[Value::Int((1 << 53) + 1)],
        &[Inst::IntToF64 {
            dst: Reg(1),
            src: Reg(0),
            ty: IntTy::I64,
        }],
        1,
    );
    assert_eq!(to64, Ok(Value::Float(9_007_199_254_740_992.0)));
    let to32 = run_regs(
        &[ValType::U64, ValType::F32],
        1,
        ValType::F32,
        &[Value::UInt(u64::MAX)],
        &[Inst::IntToF32 {
            dst: Reg(1),
            src: Reg(0),
            ty: IntTy::U64,
        }],
        1,
    );
    assert_eq!(to32, Ok(Value::F32(18_446_744_073_709_551_616.0)));
}

#[test]
fn op_float_to_int_truncates_errors_and_saturates() {
    let conv = |x: f64, op: FloatConv| {
        let t = ValType::int(op.ty());
        run_regs(
            &[F64, t],
            1,
            t,
            &[Value::Float(x)],
            &[Inst::F64ToInt {
                dst: Reg(1),
                src: Reg(0),
                conv: op,
            }],
            1,
        )
    };
    let i32op = FloatConv::new(IntTy::I32);
    let sat = i32op.with_float_to_int(bytecode_lang::FloatToInt::Saturate);
    assert_eq!(conv(-2.9, i32op), Ok(Value::Int(-2)));
    assert_eq!(
        conv(f64::NAN, i32op),
        Err(VmError::Raised {
            kind: ErrorKind::InvalidConversion,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 0
        })
    );
    assert_eq!(
        conv(3e9, i32op),
        Err(VmError::Raised {
            kind: ErrorKind::InvalidConversion,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 0
        })
    );
    assert_eq!(conv(3e9, sat), Ok(Value::Int(i64::from(i32::MAX))));
    assert_eq!(conv(-3e9, sat), Ok(Value::Int(i64::from(i32::MIN))));
    assert_eq!(conv(f64::NAN, sat), Ok(Value::Int(0)));
    // -0.9 truncates to 0, which fits an unsigned type.
    assert_eq!(conv(-0.9, FloatConv::new(IntTy::U8)), Ok(Value::UInt(0)));
    let f32conv = run_regs(
        &[ValType::F32, I64],
        1,
        I64,
        &[Value::F32(-7.5)],
        &[Inst::F32ToInt {
            dst: Reg(1),
            src: Reg(0),
            conv: FloatConv::new(IntTy::I64),
        }],
        1,
    );
    assert_eq!(f32conv, Ok(Value::Int(-7)));
}

#[test]
fn op_float_width_conversions_and_bit_casts() {
    let widen = run_regs(
        &[ValType::F32, F64],
        1,
        F64,
        &[Value::F32(0.1)],
        &[Inst::F32ToF64 {
            dst: Reg(1),
            src: Reg(0),
        }],
        1,
    );
    assert_eq!(widen, Ok(Value::Float(f64::from(0.1f32))));
    let narrow = run_regs(
        &[F64, ValType::F32],
        1,
        ValType::F32,
        &[Value::Float(0.1)],
        &[Inst::F64ToF32 {
            dst: Reg(1),
            src: Reg(0),
        }],
        1,
    );
    assert_eq!(narrow, Ok(Value::F32(0.1)));
    let bits = run_regs(
        &[ValType::F32, ValType::I32],
        1,
        ValType::I32,
        &[Value::F32(-1.0)],
        &[Inst::FloatToBits {
            dst: Reg(1),
            src: Reg(0),
            ty: IntTy::I32,
        }],
        1,
    );
    assert_eq!(bits, Ok(Value::Int(i64::from((-1.0f32).to_bits() as i32))));
    let back = run_regs(
        &[ValType::U64, F64],
        1,
        F64,
        &[Value::UInt(2.5f64.to_bits())],
        &[Inst::BitsToFloat {
            dst: Reg(1),
            src: Reg(0),
            ty: IntTy::U64,
        }],
        1,
    );
    assert_eq!(back, Ok(Value::Float(2.5)));
}

#[test]
fn op_char_conversions_and_bool_to_int() {
    let from = |v: u64| {
        run_regs(
            &[ValType::U32, ValType::Char],
            1,
            ValType::Char,
            &[Value::UInt(v)],
            &[Inst::CharFromU32 {
                dst: Reg(1),
                src: Reg(0),
            }],
            1,
        )
    };
    assert_eq!(from(0x41), Ok(Value::Char('A')));
    assert_eq!(
        from(0xD800),
        Err(VmError::Raised {
            kind: ErrorKind::InvalidChar,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 0
        })
    );
    assert_eq!(
        from(0x11_0000),
        Err(VmError::Raised {
            kind: ErrorKind::InvalidChar,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 0
        })
    );
    let to = run_regs(
        &[ValType::Char, ValType::U32],
        1,
        ValType::U32,
        &[Value::Char('€')],
        &[Inst::CharToU32 {
            dst: Reg(1),
            src: Reg(0),
        }],
        1,
    );
    assert_eq!(to, Ok(Value::UInt(0x20AC)));
    let b = run_regs(
        &[BOOL, ValType::U16],
        1,
        ValType::U16,
        &[Value::Bool(true)],
        &[Inst::BoolToInt {
            dst: Reg(1),
            src: Reg(0),
            ty: IntTy::U16,
        }],
        1,
    );
    assert_eq!(b, Ok(Value::UInt(1)));
}

#[test]
fn every_failing_instruction_leaves_its_destination_unwritten() {
    // idiv by zero inside a try: the handler reads the destination, which
    // must still hold its earlier value (LSB §4.3).
    let out = eval(&[], &[I64], &[], |_, f| {
        let (q, zero, err) = (f.reg(I64), f.reg(I64), f.reg(ValType::Dyn));
        let (start, end, handler) = (f.label(), f.label(), f.label());
        f.emit(Inst::LoadInt {
            dst: q,
            val: 77,
            ty: IntTy::I64,
        });
        f.bind(start);
        f.emit(Inst::IDiv {
            dst: q,
            lhs: q,
            rhs: zero,
            op: i64op(),
        });
        f.bind(end);
        f.ret(q);
        f.bind(handler);
        f.ret(q);
        f.try_region(start, end, handler, err);
    });
    assert_eq!(out, Ok(Value::Int(77)));
}
