//! The OPS conformance table (OPS §7): every operation × every policy ×
//! the edge values {0, 1, -1, MIN, MAX, MIN+1, MAX-1, and a few more} at
//! every integer type, and every float operation over {±0, ±1, ±MIN_POSITIVE,
//! subnormals, ±MAX, ±inf, NaN, halves}, compared bit for bit (and error
//! code for error) with an independent reference written here in `i128` and
//! IEEE `f64`. Nothing in this file calls the crate's own arithmetic.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::too_many_lines,
    clippy::type_complexity
)]

use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{
    DivZero, ErrorKind, FloatConv, FloatToInt, FloatTy, FuncId, Inst, IntConv, IntOp, IntPair,
    IntTy, ModuleBuilder, Overflow, Policy, Reg, Shift, ValType,
};

const TYPES: [IntTy; 8] = [
    IntTy::I8,
    IntTy::I16,
    IntTy::I32,
    IntTy::I64,
    IntTy::U8,
    IntTy::U16,
    IntTy::U32,
    IntTy::U64,
];

/// The reference outcome: a value, or an error kind and whether it traps.
type Out = Result<i128, (ErrorKind, bool)>;

fn range(ty: IntTy) -> (i128, i128) {
    let bits = ty.bits();
    if ty.is_signed() {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    }
}

fn wrap(ty: IntTy, v: i128) -> i128 {
    let bits = ty.bits();
    let m = 1i128 << bits;
    let low = v.rem_euclid(m);
    if ty.is_signed() && low >= m / 2 {
        low - m
    } else {
        low
    }
}

fn edges(ty: IntTy) -> Vec<i128> {
    let (lo, hi) = range(ty);
    let mut v = vec![0, 1, 2, 7, lo, hi, lo + 1, hi - 1, hi / 2, 63, 64];
    if ty.is_signed() {
        v.extend([-1, -2, -7, lo / 2, -63, -64]);
    }
    v.retain(|x| (lo..=hi).contains(x));
    v.sort_unstable();
    v.dedup();
    v
}

fn overflow(p: Policy, wrapped: i128) -> Out {
    match p.overflow() {
        Overflow::Wrap => Ok(wrapped),
        Overflow::Trap => Err((ErrorKind::ArithOverflow, true)),
        _ => Err((ErrorKind::ArithOverflow, false)),
    }
}

fn div_zero(p: Policy) -> Out {
    Err((ErrorKind::DivByZero, p.div_zero() == DivZero::Trap))
}

fn checked(ty: IntTy, p: Policy, exact: i128) -> Out {
    let (lo, hi) = range(ty);
    if (lo..=hi).contains(&exact) {
        Ok(exact)
    } else {
        overflow(p, wrap(ty, exact))
    }
}

fn floor_div(a: i128, b: i128) -> i128 {
    let q = a / b;
    if a % b != 0 && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    FloorDiv,
    FloorMod,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Min,
    Max,
}

const BIN_OPS: [BinOp; 14] = [
    BinOp::Add,
    BinOp::Sub,
    BinOp::Mul,
    BinOp::Div,
    BinOp::Rem,
    BinOp::FloorDiv,
    BinOp::FloorMod,
    BinOp::And,
    BinOp::Or,
    BinOp::Xor,
    BinOp::Shl,
    BinOp::Shr,
    BinOp::Min,
    BinOp::Max,
];

fn reference_bin(op: BinOp, ty: IntTy, p: Policy, a: i128, b: i128) -> Out {
    let bits = i128::from(ty.bits());
    match op {
        BinOp::Add => checked(ty, p, a + b),
        BinOp::Sub => checked(ty, p, a - b),
        // u64 x u64 can exceed i128; such a product is out of range anyway.
        BinOp::Mul => match a.checked_mul(b) {
            Some(exact) => checked(ty, p, exact),
            None => overflow(p, wrap(ty, a.wrapping_mul(b))),
        },
        BinOp::Div => {
            if b == 0 {
                div_zero(p)
            } else {
                checked(ty, p, a / b)
            }
        }
        BinOp::Rem => {
            if b == 0 {
                div_zero(p)
            } else {
                Ok(a % b)
            }
        }
        BinOp::FloorDiv => {
            if b == 0 {
                div_zero(p)
            } else {
                checked(ty, p, floor_div(a, b))
            }
        }
        BinOp::FloorMod => {
            if b == 0 {
                div_zero(p)
            } else {
                Ok(a - b * floor_div(a, b))
            }
        }
        BinOp::And => Ok(wrap(ty, a & b)),
        BinOp::Or => Ok(wrap(ty, a | b)),
        BinOp::Xor => Ok(wrap(ty, a ^ b)),
        BinOp::Shl | BinOp::Shr => {
            let n = if b < 0 || b >= bits {
                if p.shift() == Shift::Mask {
                    b.rem_euclid(bits)
                } else {
                    return Err((ErrorKind::ShiftOutOfRange, false));
                }
            } else {
                b
            };
            if op == BinOp::Shl {
                Ok(wrap(ty, ((a as u128) << n) as i128))
            } else {
                Ok(a >> n)
            }
        }
        BinOp::Min => Ok(a.min(b)),
        BinOp::Max => Ok(a.max(b)),
    }
}

fn bin_inst(op: BinOp, dst: Reg, lhs: Reg, rhs: Reg, o: IntOp) -> Inst {
    match op {
        BinOp::Add => Inst::IAdd {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Sub => Inst::ISub {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Mul => Inst::IMul {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Div => Inst::IDiv {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Rem => Inst::IRem {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::FloorDiv => Inst::IFloorDiv {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::FloorMod => Inst::IFloorMod {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::And => Inst::IAnd {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Or => Inst::IOr {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Xor => Inst::IXor {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Shl => Inst::IShl {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Shr => Inst::IShr {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Min => Inst::IMin {
            dst,
            lhs,
            rhs,
            op: o,
        },
        BinOp::Max => Inst::IMax {
            dst,
            lhs,
            rhs,
            op: o,
        },
    }
}

/// A program `main(a: t1, b: t2) -> t3` running one instruction.
fn one_inst(params: &[ValType], result: ValType, inst: impl Fn(Reg, Reg, Reg) -> Inst) -> Program {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("main", params, &[result]);
    let r = f.reg(result);
    let rhs = if params.len() > 1 { Reg(1) } else { Reg(0) };
    f.emit(inst(r, Reg(0), rhs));
    f.ret(r);
    m.add_function(f).unwrap();
    Program::load(m.finish().unwrap(), &Host::new()).unwrap()
}

fn int_value(ty: IntTy, v: i128) -> Value {
    if ty.is_signed() {
        Value::Int(v as i64)
    } else {
        Value::UInt(v as u64)
    }
}

fn as_out(r: Result<Value, VmError>) -> Out {
    match r {
        Ok(Value::Int(i)) => Ok(i128::from(i)),
        Ok(Value::UInt(u)) => Ok(i128::from(u)),
        Ok(Value::Bool(b)) => Ok(i128::from(b)),
        Ok(other) => panic!("unexpected value {other:?}"),
        Err(VmError::Raised { kind, .. }) => Err((kind, false)),
        Err(VmError::Trap { kind, .. }) => Err((kind, true)),
        Err(other) => panic!("unexpected error {other:?}"),
    }
}

/// Every policy combination an integer instruction can carry (promote is
/// refused by the loader on typed instructions; see `load.rs`).
fn policies() -> Vec<Policy> {
    let mut out = Vec::new();
    for o in [Overflow::Error, Overflow::Wrap, Overflow::Trap] {
        for d in [DivZero::Error, DivZero::Trap] {
            for s in [Shift::Error, Shift::Mask] {
                out.push(
                    Policy::new()
                        .with_overflow(o)
                        .with_div_zero(d)
                        .with_shift(s),
                );
            }
        }
    }
    out
}

#[test]
fn integer_binary_ops_every_type_policy_and_edge() {
    let mut checked_cases = 0usize;
    for ty in TYPES {
        let t = ValType::int(ty);
        let vals = edges(ty);
        for op in BIN_OPS {
            for p in policies() {
                let o = IntOp::new(ty).with_policy(p);
                let prog = one_inst(&[t, t], t, |d, l, r| bin_inst(op, d, l, r, o));
                let mut vm = Vm::new(&prog);
                for &a in &vals {
                    for &b in &vals {
                        let got = as_out(vm.run(FuncId(0), &[int_value(ty, a), int_value(ty, b)]));
                        let want = reference_bin(op, ty, p, a, b);
                        assert_eq!(got, want, "{op:?} {ty} {p:?} {a} {b}");
                        checked_cases += 1;
                    }
                }
            }
        }
    }
    assert!(checked_cases > 100_000, "{checked_cases}");
}

#[test]
fn integer_unary_ops_every_type_policy_and_edge() {
    for ty in TYPES {
        let t = ValType::int(ty);
        let (lo, hi) = range(ty);
        for p in policies() {
            let o = IntOp::new(ty).with_policy(p);
            let neg = one_inst(&[t], t, |d, s, _| Inst::INeg {
                dst: d,
                src: s,
                op: o,
            });
            let not = one_inst(&[t], t, |d, s, _| Inst::IBitNot {
                dst: d,
                src: s,
                op: o,
            });
            let abs = one_inst(&[t], t, |d, s, _| Inst::IAbs {
                dst: d,
                src: s,
                op: o,
            });
            for a in edges(ty) {
                let arg = [int_value(ty, a)];
                assert_eq!(
                    as_out(Vm::new(&neg).run(FuncId(0), &arg)),
                    checked(ty, p, -a),
                    "neg {ty} {a}"
                );
                assert_eq!(
                    as_out(Vm::new(&not).run(FuncId(0), &arg)),
                    Ok(wrap(ty, !a)),
                    "not {ty} {a}"
                );
                let want_abs = if ty.is_signed() {
                    checked(ty, p, a.abs())
                } else {
                    Ok(a)
                };
                assert_eq!(
                    as_out(Vm::new(&abs).run(FuncId(0), &arg)),
                    want_abs,
                    "abs {ty} {a}"
                );
                let _ = (lo, hi);
            }
        }
    }
}

#[test]
fn integer_comparisons_every_type_and_edge() {
    type Mk = fn(Reg, Reg, Reg, IntTy) -> Inst;
    let cmps: [(Mk, fn(i128, i128) -> bool); 6] = [
        (
            |d, l, r, ty| Inst::IEq {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a == b,
        ),
        (
            |d, l, r, ty| Inst::INe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a != b,
        ),
        (
            |d, l, r, ty| Inst::ILt {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a < b,
        ),
        (
            |d, l, r, ty| Inst::ILe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a <= b,
        ),
        (
            |d, l, r, ty| Inst::IGt {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a > b,
        ),
        (
            |d, l, r, ty| Inst::IGe {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a >= b,
        ),
    ];
    for ty in TYPES {
        let t = ValType::int(ty);
        for (make, f) in cmps {
            let prog = one_inst(&[t, t], ValType::Bool, |d, l, r| make(d, l, r, ty));
            let mut vm = Vm::new(&prog);
            for a in edges(ty) {
                for b in edges(ty) {
                    let got = vm.run(FuncId(0), &[int_value(ty, a), int_value(ty, b)]);
                    assert_eq!(got, Ok(Value::Bool(f(a, b))), "{ty} {a} {b}");
                }
            }
        }
    }
}

#[test]
fn int_cast_every_pair_policy_and_edge() {
    for from in TYPES {
        for to in TYPES {
            for ov in [Overflow::Error, Overflow::Wrap, Overflow::Trap] {
                let conv = IntConv::new(from, to, ov);
                let prog = one_inst(&[ValType::int(from)], ValType::int(to), |d, s, _| {
                    Inst::IntCast {
                        dst: d,
                        src: s,
                        conv,
                    }
                });
                let mut vm = Vm::new(&prog);
                for a in edges(from) {
                    let want = checked(to, Policy::new().with_overflow(ov), a);
                    let got = as_out(vm.run(FuncId(0), &[int_value(from, a)]));
                    assert_eq!(got, want, "{from}->{to} {ov} {a}");
                }
            }
        }
    }
}

#[test]
fn zext_sext_trunc_every_pair_and_edge() {
    for from in TYPES {
        for to in TYPES {
            let pair = IntPair::new(from, to);
            let (ft, tt) = (ValType::int(from), ValType::int(to));
            let zext = one_inst(&[ft], tt, |d, s, _| Inst::Zext {
                dst: d,
                src: s,
                pair,
            });
            let sext = one_inst(&[ft], tt, |d, s, _| Inst::Sext {
                dst: d,
                src: s,
                pair,
            });
            let trunc = one_inst(&[ft], tt, |d, s, _| Inst::Trunc {
                dst: d,
                src: s,
                pair,
            });
            for a in edges(from) {
                let arg = [int_value(from, a)];
                let bits = 1i128 << from.bits();
                let unsigned_src = a.rem_euclid(bits);
                let signed_src = if unsigned_src >= bits / 2 {
                    unsigned_src - bits
                } else {
                    unsigned_src
                };
                if to.bits() >= from.bits() {
                    assert_eq!(
                        as_out(Vm::new(&zext).run(FuncId(0), &arg)),
                        Ok(wrap(to, unsigned_src)),
                        "zext {from}->{to} {a}"
                    );
                    assert_eq!(
                        as_out(Vm::new(&sext).run(FuncId(0), &arg)),
                        Ok(wrap(to, signed_src)),
                        "sext {from}->{to} {a}"
                    );
                }
                if to.bits() <= from.bits() {
                    assert_eq!(
                        as_out(Vm::new(&trunc).run(FuncId(0), &arg)),
                        Ok(wrap(to, a)),
                        "trunc {from}->{to} {a}"
                    );
                }
            }
        }
    }
}

fn f64_edges() -> Vec<f64> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        1.5,
        -1.5,
        2.5,
        -2.5,
        3.0,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        5e-324,
        -5e-324,
        f64::from_bits(0x000F_FFFF_FFFF_FFFF), // largest subnormal
        f64::MAX,
        f64::MIN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        1e300,
        -1e-300,
        9_007_199_254_740_993.0,
        4_503_599_627_370_495.5,
    ]
}

fn f32_edges() -> Vec<f32> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -1.5,
        2.5,
        f32::MIN_POSITIVE,
        1e-45,
        -1e-45,
        f32::MAX,
        f32::MIN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        16_777_217.0,
        8_388_607.5,
    ]
}

fn same_f64(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

fn ref_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}

fn ref_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_positive() { a } else { b }
    } else {
        a.max(b)
    }
}

/// IEEE `remainder` from the exact `fmod` (Rust's `%`): the truncated
/// remainder, adjusted by one |y| toward zero when it exceeds half of |y|
/// (or equals it with an odd truncated quotient, whose parity comes from
/// `fmod(x, 2|y|)`).
fn ref_remainder(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() || x.is_infinite() || y == 0.0 {
        return f64::NAN;
    }
    if y.is_infinite() || x == 0.0 {
        return x;
    }
    // r = fmod(x, y) is exact; then round to nearest with ties to even
    // using the parity of trunc(x / y), recovered from fmod(x, 2y).
    let ay = y.abs();
    let r = x % ay; // exact, sign of x
    let two_y = 2.0 * ay;
    // When 2|y| overflows, |x| < 2|y| and the quotient is 0 or 1.
    let odd = if two_y.is_finite() {
        (x % two_y).abs() >= ay
    } else {
        x.abs() >= ay
    };
    let ar = r.abs();
    // 2|r| is exact (or overflows, which still compares correctly).
    let two_r = 2.0 * ar;
    let flip = two_r > ay || (two_r == ay && odd);
    if flip {
        if r > 0.0 { r - ay } else { r + ay }
    } else {
        r
    }
}

#[test]
fn float_binary_ops_f64_every_edge() {
    type Mk = fn(Reg, Reg, Reg, FloatTy) -> Inst;
    let ops: Vec<(Mk, fn(f64, f64) -> f64, &str)> = vec![
        (
            |d, l, r, ty| Inst::FAdd {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a + b,
            "fadd",
        ),
        (
            |d, l, r, ty| Inst::FSub {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a - b,
            "fsub",
        ),
        (
            |d, l, r, ty| Inst::FMul {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a * b,
            "fmul",
        ),
        (
            |d, l, r, ty| Inst::FDiv {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a / b,
            "fdiv",
        ),
        (
            |d, l, r, ty| Inst::FRem {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a % b,
            "frem",
        ),
        (
            |d, l, r, ty| Inst::FIeeeRem {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            ref_remainder,
            "fieee_rem",
        ),
        (
            |d, l, r, ty| Inst::FMin {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            ref_min,
            "fmin",
        ),
        (
            |d, l, r, ty| Inst::FMax {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            ref_max,
            "fmax",
        ),
    ];
    for (make, f, name) in ops {
        let prog = one_inst(&[ValType::F64, ValType::F64], ValType::F64, |d, l, r| {
            make(d, l, r, FloatTy::F64)
        });
        let mut vm = Vm::new(&prog);
        for a in f64_edges() {
            for b in f64_edges() {
                let got = vm
                    .run(FuncId(0), &[Value::Float(a), Value::Float(b)])
                    .unwrap()
                    .as_float()
                    .unwrap();
                assert!(
                    same_f64(got, f(a, b)),
                    "{name} {a:e} {b:e}: {got:e} vs {:e}",
                    f(a, b)
                );
            }
        }
    }
}

#[test]
fn float_binary_ops_f32_every_edge() {
    type Mk = fn(Reg, Reg, Reg, FloatTy) -> Inst;
    let ops: Vec<(Mk, fn(f32, f32) -> f32, &str)> = vec![
        (
            |d, l, r, ty| Inst::FAdd {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a + b,
            "fadd",
        ),
        (
            |d, l, r, ty| Inst::FMul {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a * b,
            "fmul",
        ),
        (
            |d, l, r, ty| Inst::FDiv {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a / b,
            "fdiv",
        ),
        (
            |d, l, r, ty| Inst::FRem {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| a % b,
            "frem",
        ),
        (
            |d, l, r, ty| Inst::FIeeeRem {
                dst: d,
                lhs: l,
                rhs: r,
                ty,
            },
            |a, b| ref_remainder(f64::from(a), f64::from(b)) as f32,
            "fieee_rem",
        ),
    ];
    for (make, f, name) in ops {
        let prog = one_inst(&[ValType::F32, ValType::F32], ValType::F32, |d, l, r| {
            make(d, l, r, FloatTy::F32)
        });
        let mut vm = Vm::new(&prog);
        for a in f32_edges() {
            for b in f32_edges() {
                let Value::F32(got) = vm.run(FuncId(0), &[Value::F32(a), Value::F32(b)]).unwrap()
                else {
                    panic!("not f32");
                };
                let want = f(a, b);
                assert!(
                    (got.is_nan() && want.is_nan()) || got.to_bits() == want.to_bits(),
                    "{name} {a:e} {b:e}"
                );
            }
        }
    }
}

#[test]
fn float_unary_ops_and_fma_every_edge() {
    type Mk = fn(Reg, Reg, FloatTy) -> Inst;
    let ops: Vec<(Mk, fn(f64) -> f64, &str)> = vec![
        (|d, s, ty| Inst::FNeg { dst: d, src: s, ty }, |a| -a, "fneg"),
        (
            |d, s, ty| Inst::FAbs { dst: d, src: s, ty },
            f64::abs,
            "fabs",
        ),
        (
            |d, s, ty| Inst::FSqrt { dst: d, src: s, ty },
            f64::sqrt,
            "fsqrt",
        ),
        (
            |d, s, ty| Inst::FFloor { dst: d, src: s, ty },
            f64::floor,
            "ffloor",
        ),
        (
            |d, s, ty| Inst::FCeil { dst: d, src: s, ty },
            f64::ceil,
            "fceil",
        ),
        (
            |d, s, ty| Inst::FTrunc { dst: d, src: s, ty },
            f64::trunc,
            "ftrunc",
        ),
        (
            |d, s, ty| Inst::FRound { dst: d, src: s, ty },
            f64::round,
            "fround",
        ),
        (
            |d, s, ty| Inst::FRoundEven { dst: d, src: s, ty },
            f64::round_ties_even,
            "fround_even",
        ),
    ];
    for (make, f, name) in ops {
        let prog = one_inst(&[ValType::F64], ValType::F64, |d, s, _| {
            make(d, s, FloatTy::F64)
        });
        let mut vm = Vm::new(&prog);
        for a in f64_edges() {
            let got = vm
                .run(FuncId(0), &[Value::Float(a)])
                .unwrap()
                .as_float()
                .unwrap();
            assert!(same_f64(got, f(a)), "{name} {a:e}");
        }
    }
    // fma over a 3-way edge product (dst is the addend).
    let mut m = ModuleBuilder::new();
    let mut f = m.function(
        "main",
        &[ValType::F64, ValType::F64, ValType::F64],
        &[ValType::F64],
    );
    f.emit(Inst::FFma {
        dst: Reg(2),
        lhs: Reg(0),
        rhs: Reg(1),
        ty: FloatTy::F64,
    });
    f.ret(Reg(2));
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    let e = f64_edges();
    for &a in &e {
        for &b in &e {
            for &c in &e {
                let got = vm
                    .run(
                        FuncId(0),
                        &[Value::Float(a), Value::Float(b), Value::Float(c)],
                    )
                    .unwrap()
                    .as_float()
                    .unwrap();
                assert!(same_f64(got, a.mul_add(b, c)), "fma {a:e} {b:e} {c:e}");
            }
        }
    }
}

#[test]
fn float_to_int_every_type_policy_and_edge() {
    for ty in TYPES {
        let (lo, hi) = range(ty);
        for fti in [FloatToInt::Error, FloatToInt::Saturate] {
            let o = FloatConv::new(ty).with_float_to_int(fti);
            let prog = one_inst(&[ValType::F64], ValType::int(ty), |d, s, _| {
                Inst::F64ToInt {
                    dst: d,
                    src: s,
                    conv: o,
                }
            });
            let mut vm = Vm::new(&prog);
            let mut inputs = f64_edges();
            inputs.extend([
                lo as f64,
                hi as f64,
                (lo as f64) - 1.0,
                (hi as f64) + 1.0,
                255.9,
                -128.9,
                65_535.5,
            ]);
            for x in inputs {
                let t = x.trunc();
                let want: Out = if !x.is_nan()
                    && t >= lo as f64
                    && t <= hi as f64
                    && (t as i128) >= lo
                    && (t as i128) <= hi
                {
                    Ok(t as i128)
                } else if fti == FloatToInt::Saturate {
                    Ok(if x.is_nan() {
                        0
                    } else if t < lo as f64 {
                        lo
                    } else {
                        hi
                    })
                } else {
                    Err((ErrorKind::InvalidConversion, false))
                };
                let got = as_out(vm.run(FuncId(0), &[Value::Float(x)]));
                assert_eq!(got, want, "{ty} {fti} {x:e}");
            }
        }
    }
}

#[test]
fn int_to_float_every_type_and_edge() {
    for ty in TYPES {
        let p64 = one_inst(&[ValType::int(ty)], ValType::F64, |d, s, _| {
            Inst::IntToF64 { dst: d, src: s, ty }
        });
        let p32 = one_inst(&[ValType::int(ty)], ValType::F32, |d, s, _| {
            Inst::IntToF32 { dst: d, src: s, ty }
        });
        for a in edges(ty) {
            let arg = [int_value(ty, a)];
            assert_eq!(
                Vm::new(&p64).run(FuncId(0), &arg),
                Ok(Value::Float(a as f64)),
                "{ty} {a}"
            );
            assert_eq!(
                Vm::new(&p32).run(FuncId(0), &arg),
                Ok(Value::F32(a as f32)),
                "{ty} {a}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The dynamic instructions' numeric fast path, every policy including
// promote.
// ---------------------------------------------------------------------------

/// The correctly rounded quotient, by long division to 66 significant bits
/// plus a sticky bit and an independent round-to-nearest-even.
fn ref_quotient(a: i64, b: i64) -> f64 {
    let neg = (a < 0) != (b < 0);
    let (n, d) = (u128::from(a.unsigned_abs()), u128::from(b.unsigned_abs()));
    if n == 0 {
        return if neg { -0.0 } else { 0.0 };
    }
    // Integer part and fraction bits, one at a time.
    let mut q = n / d;
    let mut r = n % d;
    let mut exp = 0i32; // value = q * 2^exp
    while q < (1u128 << 66) {
        q <<= 1;
        r <<= 1;
        if r >= d {
            q |= 1;
            r -= d;
        }
        exp -= 1;
    }
    let sticky = r != 0;
    // Keep 53 bits.
    let len = 128 - q.leading_zeros() as i32;
    let drop = len - 53;
    let mut m = q >> drop;
    let rest = q & ((1u128 << drop) - 1);
    let half = 1u128 << (drop - 1);
    if rest > half || (rest == half && (sticky || m & 1 == 1)) {
        m += 1;
    }
    let v = (m as f64) * 2f64.powi(exp + drop);
    if neg { -v } else { v }
}

fn ref_dyn_int(op: BinOp, p: Policy, a: i64, b: i64) -> Result<Value, (ErrorKind, bool)> {
    let (a1, b1) = (i128::from(a), i128::from(b));
    if p.overflow() == Overflow::Promote {
        let promoted = |exact: i128| {
            if i64::try_from(exact).is_ok() {
                Value::Int(exact as i64)
            } else {
                Value::Float(exact as f64)
            }
        };
        match op {
            BinOp::Add => return Ok(promoted(a1 + b1)),
            BinOp::Sub => return Ok(promoted(a1 - b1)),
            BinOp::Mul => return Ok(promoted(a1 * b1)),
            BinOp::Div => {
                if b == 0 {
                    return div_zero(p).map(|_| Value::Nil);
                }
                if a1 % b1 == 0 && i64::try_from(a1 / b1).is_ok() {
                    return Ok(Value::Int((a1 / b1) as i64));
                }
                return Ok(Value::Float(ref_quotient(a, b)));
            }
            BinOp::FloorDiv if b != 0 && i64::try_from(floor_div(a1, b1)).is_err() => {
                return Ok(Value::Float(floor_div(a1, b1) as f64));
            }
            _ => {}
        }
    }
    reference_bin(op, IntTy::I64, p, a1, b1).map(|v| Value::Int(v as i64))
}

fn dyn_inst(op: BinOp, dst: Reg, lhs: Reg, rhs: Reg, pol: Policy) -> Option<Inst> {
    Some(match op {
        BinOp::Add => Inst::DAdd { dst, lhs, rhs, pol },
        BinOp::Sub => Inst::DSub { dst, lhs, rhs, pol },
        BinOp::Mul => Inst::DMul { dst, lhs, rhs, pol },
        BinOp::Div => Inst::DDiv { dst, lhs, rhs, pol },
        BinOp::Rem => Inst::DRem { dst, lhs, rhs, pol },
        BinOp::FloorDiv => Inst::DFloorDiv { dst, lhs, rhs, pol },
        BinOp::FloorMod => Inst::DFloorMod { dst, lhs, rhs, pol },
        BinOp::And => Inst::DAnd { dst, lhs, rhs, pol },
        BinOp::Or => Inst::DOr { dst, lhs, rhs, pol },
        BinOp::Xor => Inst::DXor { dst, lhs, rhs, pol },
        BinOp::Shl => Inst::DShl { dst, lhs, rhs, pol },
        BinOp::Shr => Inst::DShr { dst, lhs, rhs, pol },
        BinOp::Min | BinOp::Max => return None,
    })
}

#[test]
fn dynamic_int_path_every_policy_including_promote() {
    let mut pols = policies();
    pols.extend(
        [Overflow::Promote]
            .iter()
            .map(|&o| Policy::new().with_overflow(o)),
    );
    pols.push(
        Policy::new()
            .with_overflow(Overflow::Promote)
            .with_div_zero(DivZero::Trap),
    );
    let vals: Vec<i64> = edges(IntTy::I64)
        .into_iter()
        .map(|v| v as i64)
        .chain([
            1 << 48,
            -(1 << 48),
            (1 << 48) - 1,
            -(1 << 48) - 1,
            3,
            1 << 53,
            (1 << 53) + 1,
        ])
        .collect();
    for op in BIN_OPS {
        for &p in &pols {
            let Some(_) = dyn_inst(op, Reg(0), Reg(0), Reg(0), p) else {
                continue;
            };
            let prog = one_inst(&[ValType::Dyn, ValType::Dyn], ValType::Dyn, |d, l, r| {
                dyn_inst(op, d, l, r, p).unwrap_or(Inst::Nop {})
            });
            let mut vm = Vm::new(&prog);
            for &a in &vals {
                for &b in &vals {
                    let got = match vm.run(FuncId(0), &[Value::Int(a), Value::Int(b)]) {
                        Ok(v) => Ok(v),
                        Err(VmError::Raised { kind, .. }) => Err((kind, false)),
                        Err(VmError::Trap { kind, .. }) => Err((kind, true)),
                        Err(e) => panic!("{e:?}"),
                    };
                    let want = ref_dyn_int(op, p, a, b);
                    let same = match (&got, &want) {
                        (Ok(Value::Float(x)), Ok(Value::Float(y))) => x.to_bits() == y.to_bits(),
                        _ => got == want,
                    };
                    assert!(same, "{op:?} {p:?} {a} {b}: {got:?} vs {want:?}");
                }
            }
        }
    }
}

/// CPython's float floor division/modulo, transcribed from LSB §5.6.
fn ref_py(a: f64, b: f64) -> (f64, f64) {
    let mut m = a % b;
    let mut q = (a - m) / b;
    if m != 0.0 || m.is_nan() {
        if (b < 0.0) != (m < 0.0) {
            m += b;
            q -= 1.0;
        }
    } else {
        m = 0f64.copysign(b);
    }
    let fq = if q != 0.0 || q.is_nan() {
        let mut fq = q.floor();
        if q - fq > 0.5 {
            fq += 1.0;
        }
        fq
    } else {
        0f64.copysign(a / b)
    };
    (fq, m)
}

#[test]
fn dynamic_float_path_every_edge() {
    let vals = f64_edges();
    let ops: Vec<(BinOp, fn(f64, f64) -> f64)> = vec![
        (BinOp::Add, |a, b| a + b),
        (BinOp::Sub, |a, b| a - b),
        (BinOp::Mul, |a, b| a * b),
        (BinOp::Div, |a, b| a / b),
        (BinOp::Rem, |a, b| a % b),
        (BinOp::FloorDiv, |a, b| ref_py(a, b).0),
        (BinOp::FloorMod, |a, b| ref_py(a, b).1),
    ];
    for (op, f) in ops {
        let prog = one_inst(&[ValType::Dyn, ValType::Dyn], ValType::Dyn, |d, l, r| {
            dyn_inst(op, d, l, r, Policy::new()).unwrap_or(Inst::Nop {})
        });
        let mut vm = Vm::new(&prog);
        for &a in &vals {
            for &b in &vals {
                let got = vm
                    .run(FuncId(0), &[Value::Float(a), Value::Float(b)])
                    .unwrap();
                let want = f(a, b);
                assert!(
                    matches!(got, Value::Float(g) if same_f64(g, want)),
                    "{op:?} {a:e} {b:e}: {got:?} vs {want:e}"
                );
            }
            // Mixed int/float: the int converts to f64 first.
            for b in [0i64, 1, -1, 7, i64::MAX, i64::MIN] {
                let got = vm
                    .run(FuncId(0), &[Value::Float(a), Value::Int(b)])
                    .unwrap();
                let want = f(a, b as f64);
                assert!(
                    matches!(got, Value::Float(g) if same_f64(g, want)),
                    "{op:?} {a:e} int {b}"
                );
            }
        }
    }
    // CPython's documented results from the spec.
    let cases: [(f64, f64, f64, f64); 6] = [
        (-7.0, 2.0, -4.0, 1.0),
        (7.0, -2.0, -4.0, -1.0),
        (-0.0, 1.0, -0.0, 0.0),
        (0.0, -1.0, -0.0, -0.0),
        (1.0, f64::INFINITY, 0.0, 1.0),
        (-1.0, f64::INFINITY, -1.0, f64::INFINITY),
    ];
    for (a, b, q, r) in cases {
        let (gq, gr) = ref_py(a, b);
        assert_eq!(
            (gq.to_bits(), gr.to_bits()),
            (q.to_bits(), r.to_bits()),
            "{a} {b}"
        );
    }
}

#[test]
fn reference_quotient_agrees_with_ieee_division_on_small_operands() {
    // Sanity check of the reference itself: below 2^53 both operands are
    // exact, so IEEE division is correctly rounded.
    for a in [-7i64, 1, 2, 3, 10, 1 << 40, (1 << 53) - 1] {
        for b in [3i64, -7, 10, 11, 1 << 20] {
            assert_eq!(
                ref_quotient(a, b).to_bits(),
                (a as f64 / b as f64).to_bits(),
                "{a}/{b}"
            );
        }
    }
}
