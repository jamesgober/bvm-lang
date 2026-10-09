//! Differential property tests: random straight-line programs and random
//! control-flow graphs (loops included, terminated by fuel) run on the VM
//! and on the reference interpreter in `common/reference.rs`; results,
//! error kinds and pcs, traps, and fuel consumption must agree exactly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

mod common;

use bvm_lang::{Host, Limits, Program, Value, Vm, VmError};
use bytecode_lang::{
    DivZero, FloatConv, FloatToInt, FloatTy, FuncId, Inst, IntConv, IntOp, IntTy, ModuleBuilder,
    Overflow, Policy, Prim, Reg, Shift, Target, ValType,
};
use common::reference::{self, Dv, Outcome, Rv};
use proptest::prelude::*;

/// Register file layout of generated functions.
const I64S: [u16; 4] = [0, 1, 2, 3]; // r0 (param), r1..r3
const I32S: [u16; 2] = [4, 5];
const U8S: [u16; 1] = [6];
const BOOLS: [u16; 2] = [7, 8];
const F64S: [u16; 2] = [9, 10];
const DYNS: [u16; 3] = [11, 12, 13];

fn types() -> Vec<ValType> {
    vec![
        ValType::I64,
        ValType::I64,
        ValType::I64,
        ValType::I64,
        ValType::I32,
        ValType::I32,
        ValType::U8,
        ValType::Bool,
        ValType::Bool,
        ValType::F64,
        ValType::F64,
        ValType::Dyn,
        ValType::Dyn,
        ValType::Dyn,
    ]
}

fn reg(set: &'static [u16]) -> impl Strategy<Value = Reg> {
    proptest::sample::select(set).prop_map(Reg)
}

fn policy() -> impl Strategy<Value = Policy> {
    (0u8..3, any::<bool>(), 0u8..3, any::<bool>()).prop_map(|(o, d, s, f)| {
        Policy::new()
            .with_overflow([Overflow::Error, Overflow::Wrap, Overflow::Trap][usize::from(o)])
            .with_div_zero(if d { DivZero::Trap } else { DivZero::Error })
            .with_shift([Shift::Error, Shift::Mask, Shift::Saturate][usize::from(s)])
            .with_float_to_int(if f {
                FloatToInt::Saturate
            } else {
                FloatToInt::Error
            })
    })
}

fn dyn_policy() -> impl Strategy<Value = Policy> {
    (0u8..4, any::<bool>(), 0u8..3).prop_map(|(o, d, s)| {
        Policy::new()
            .with_overflow(
                [
                    Overflow::Error,
                    Overflow::Wrap,
                    Overflow::Trap,
                    Overflow::Promote,
                ][usize::from(o)],
            )
            .with_div_zero(if d { DivZero::Trap } else { DivZero::Error })
            .with_shift([Shift::Error, Shift::Mask, Shift::Saturate][usize::from(s)])
    })
}

/// Interesting immediates: small values and boundaries of every width.
fn imm() -> impl Strategy<Value = i32> {
    prop_oneof![
        -8i32..8,
        Just(i32::MAX),
        Just(i32::MIN),
        Just(127),
        Just(-128),
        Just(255),
        Just(64),
        Just(63),
        any::<i32>(),
    ]
}

/// One non-branching instruction over well-typed registers.
fn straight() -> impl Strategy<Value = Inst> {
    let int_bin = (0usize..15, 0usize..3, policy()).prop_flat_map(|(which, width, p)| {
        let (set, ty): (&'static [u16], IntTy) = match width {
            0 => (&I64S, IntTy::I64),
            1 => (&I32S, IntTy::I32),
            _ => (&U8S, IntTy::U8),
        };
        (reg(set), reg(set), reg(set)).prop_map(move |(dst, lhs, rhs)| {
            let op = IntOp::new(ty).with_policy(p);
            match which {
                0 => Inst::IAdd { dst, lhs, rhs, op },
                1 => Inst::ISub { dst, lhs, rhs, op },
                2 => Inst::IMul { dst, lhs, rhs, op },
                3 => Inst::IDiv { dst, lhs, rhs, op },
                4 => Inst::IRem { dst, lhs, rhs, op },
                5 => Inst::IFloorDiv { dst, lhs, rhs, op },
                6 => Inst::IFloorMod { dst, lhs, rhs, op },
                7 => Inst::IAnd { dst, lhs, rhs, op },
                8 => Inst::IOr { dst, lhs, rhs, op },
                9 => Inst::IXor { dst, lhs, rhs, op },
                10 => Inst::IShl { dst, lhs, rhs, op },
                11 => Inst::IShr { dst, lhs, rhs, op },
                12 => Inst::IMin { dst, lhs, rhs, op },
                13 => Inst::IPow { dst, lhs, rhs, op },
                _ => Inst::IMax { dst, lhs, rhs, op },
            }
        })
    });
    let int_un = (0usize..3, policy(), reg(&I64S), reg(&I64S)).prop_map(|(w, p, dst, src)| {
        let op = IntOp::new(IntTy::I64).with_policy(p);
        match w {
            0 => Inst::INeg { dst, src, op },
            1 => Inst::IBitNot { dst, src, op },
            _ => Inst::IAbs { dst, src, op },
        }
    });
    let load = prop_oneof![
        (reg(&I64S), imm()).prop_map(|(dst, val)| Inst::LoadInt {
            dst,
            val,
            ty: IntTy::I64
        }),
        (reg(&I32S), imm()).prop_map(|(dst, val)| Inst::LoadInt {
            dst,
            val,
            ty: IntTy::I32
        }),
        (reg(&U8S), 0i32..256).prop_map(|(dst, val)| Inst::LoadInt {
            dst,
            val,
            ty: IntTy::U8
        }),
        (reg(&BOOLS), any::<bool>()).prop_map(|(dst, val)| Inst::LoadBool { dst, val }),
        (reg(&DYNS), imm()).prop_map(|(dst, val)| Inst::DLoadInt { dst, val }),
    ];
    let cmp =
        (0usize..3, reg(&BOOLS), reg(&I64S), reg(&I64S)).prop_map(|(w, dst, lhs, rhs)| match w {
            0 => Inst::IEq {
                dst,
                lhs,
                rhs,
                ty: IntTy::I64,
            },
            1 => Inst::ILt {
                dst,
                lhs,
                rhs,
                ty: IntTy::I64,
            },
            _ => Inst::IGe {
                dst,
                lhs,
                rhs,
                ty: IntTy::I64,
            },
        });
    let casts = prop_oneof![
        (reg(&I32S), reg(&I64S), 0u8..3).prop_map(|(dst, src, o)| Inst::IntCast {
            dst,
            src,
            conv: IntConv::new(
                IntTy::I64,
                IntTy::I32,
                [Overflow::Error, Overflow::Wrap, Overflow::Trap][usize::from(o)]
            ),
        }),
        (reg(&U8S), reg(&I32S), 0u8..3).prop_map(|(dst, src, o)| Inst::IntCast {
            dst,
            src,
            conv: IntConv::new(
                IntTy::I32,
                IntTy::U8,
                [Overflow::Error, Overflow::Wrap, Overflow::Trap][usize::from(o)]
            ),
        }),
        (reg(&I64S), reg(&U8S)).prop_map(|(dst, src)| Inst::IntCast {
            dst,
            src,
            conv: IntConv::new(IntTy::U8, IntTy::I64, Overflow::Error)
        }),
        (reg(&F64S), reg(&I64S)).prop_map(|(dst, src)| Inst::IntToF64 {
            dst,
            src,
            ty: IntTy::I64
        }),
        (reg(&I64S), reg(&F64S), policy()).prop_map(|(dst, src, p)| Inst::F64ToInt {
            dst,
            src,
            conv: FloatConv::new(IntTy::I64).with_policy(p)
        }),
        (reg(&I32S), reg(&F64S), policy()).prop_map(|(dst, src, p)| Inst::F64ToInt {
            dst,
            src,
            conv: FloatConv::new(IntTy::I32).with_policy(p)
        }),
    ];
    let floats = (0usize..6, reg(&F64S), reg(&F64S), reg(&F64S), reg(&BOOLS)).prop_map(
        |(w, dst, lhs, rhs, b)| {
            let ty = FloatTy::F64;
            match w {
                0 => Inst::FAdd { dst, lhs, rhs, ty },
                1 => Inst::FSub { dst, lhs, rhs, ty },
                2 => Inst::FMul { dst, lhs, rhs, ty },
                3 => Inst::FDiv { dst, lhs, rhs, ty },
                4 => Inst::FPow { dst, lhs, rhs, ty },
                _ => Inst::FLt {
                    dst: b,
                    lhs,
                    rhs,
                    ty,
                },
            }
        },
    );
    let bools =
        (any::<bool>(), reg(&BOOLS), reg(&BOOLS), reg(&BOOLS)).prop_map(|(w, dst, a, b)| {
            if w {
                Inst::BNot { dst, src: a }
            } else {
                Inst::BAnd {
                    dst,
                    lhs: a,
                    rhs: b,
                }
            }
        });
    let dyns = prop_oneof![
        (reg(&DYNS), reg(&I64S)).prop_map(|(dst, src)| Inst::ToDyn {
            dst,
            src,
            from: Prim::I64
        }),
        (reg(&DYNS), reg(&F64S)).prop_map(|(dst, src)| Inst::ToDyn {
            dst,
            src,
            from: Prim::F64
        }),
        (reg(&I64S), reg(&DYNS)).prop_map(|(dst, src)| Inst::FromDyn {
            dst,
            src,
            to: Prim::I64
        }),
        (0usize..11, dyn_policy(), reg(&DYNS), reg(&DYNS), reg(&DYNS)).prop_map(
            |(w, pol, dst, lhs, rhs)| match w {
                0 => Inst::DAdd { dst, lhs, rhs, pol },
                1 => Inst::DSub { dst, lhs, rhs, pol },
                2 => Inst::DMul { dst, lhs, rhs, pol },
                3 => Inst::DDiv { dst, lhs, rhs, pol },
                4 => Inst::DRem { dst, lhs, rhs, pol },
                5 => Inst::DFloorDiv { dst, lhs, rhs, pol },
                6 => Inst::DPow { dst, lhs, rhs, pol },
                7 => Inst::DShl { dst, lhs, rhs, pol },
                8 => Inst::DShr { dst, lhs, rhs, pol },
                9 => Inst::DAbs { dst, src: lhs, pol },
                _ => Inst::DFloorMod { dst, lhs, rhs, pol },
            }
        ),
        (any::<bool>(), reg(&BOOLS), reg(&DYNS), reg(&DYNS)).prop_map(|(w, dst, lhs, rhs)| {
            if w {
                Inst::DLt { dst, lhs, rhs }
            } else {
                Inst::DEq { dst, lhs, rhs }
            }
        }),
    ];
    let movs = prop_oneof![
        (reg(&I64S), reg(&I64S)).prop_map(|(dst, src)| Inst::Mov { dst, src }),
        (reg(&DYNS), reg(&DYNS)).prop_map(|(dst, src)| Inst::Mov { dst, src }),
        Just(Inst::Safepoint {}),
        Just(Inst::Nop {}),
    ];
    prop_oneof![
        4 => int_bin,
        1 => int_un,
        3 => load,
        1 => cmp,
        1 => casts,
        1 => floats,
        1 => bools,
        2 => dyns,
        1 => movs,
    ]
}

/// A generated function: its code, jump tables, and the register returned.
#[derive(Clone, Debug)]
struct Gen {
    code: Vec<Inst>,
    tables: Vec<(Vec<u32>, u32)>,
}

/// Straight-line code ending in `ret r0`.
fn straight_program() -> impl Strategy<Value = Gen> {
    proptest::collection::vec(straight(), 0..60).prop_map(|mut code| {
        code.push(Inst::Ret { src: Reg(0) });
        Gen {
            code,
            tables: Vec::new(),
        }
    })
}

/// Code with branches anywhere (forward and backward), a switch, ending in
/// `ret r0`; targets are resolved modulo the final length.
fn cfg_program() -> impl Strategy<Value = Gen> {
    let item = prop_oneof![
        6 => straight().prop_map(|i| (i, 0u32, 0u8)),
        1 => (any::<u32>(), 0u8..3).prop_map(|(t, kind)| (Inst::Nop {}, t, kind + 1)),
        1 => (reg(&BOOLS), any::<u32>()).prop_map(|(c, t)| (Inst::JmpIf { cond: c, target: Target(0) }, t, 10)),
    ];
    (
        proptest::collection::vec(item, 1..60),
        proptest::collection::vec(any::<u32>(), 1..4),
        any::<u32>(),
    )
        .prop_map(|(items, table_targets, default)| {
            let len = u32::try_from(items.len() + 1).unwrap_or(u32::MAX);
            let mut code = Vec::with_capacity(items.len() + 1);
            for (inst, t, kind) in items {
                let target = Target(t % len);
                code.push(match kind {
                    0 => inst,
                    1 => Inst::Jmp { target },
                    2 => Inst::JmpIfNot {
                        cond: Reg(BOOLS[0]),
                        target,
                    },
                    3 => Inst::Switch {
                        src: Reg(I64S[1]),
                        table: bytecode_lang::TableId(0),
                        ty: IntTy::I64,
                    },
                    _ => match inst {
                        Inst::JmpIf { cond, .. } => Inst::JmpIf { cond, target },
                        other => other,
                    },
                });
            }
            code.push(Inst::Ret { src: Reg(0) });
            let tables = vec![(
                table_targets.iter().map(|t| t % len).collect(),
                default % len,
            )];
            Gen { code, tables }
        })
}

/// Initial values written to every register before the generated code, so
/// operations see varied operands rather than defaults.
#[derive(Clone, Debug)]
struct Init {
    ints: [i32; 7],
    bools: [bool; 2],
    floats: [i32; 2],
    dyns: [i32; 3],
}

fn init() -> impl Strategy<Value = Init> {
    (
        proptest::array::uniform7(imm()),
        proptest::array::uniform2(any::<bool>()),
        proptest::array::uniform2(imm()),
        proptest::array::uniform3(imm()),
    )
        .prop_map(|(ints, bools, floats, dyns)| Init {
            ints,
            bools,
            floats,
            dyns,
        })
}

/// The prologue writing `init` (r0 keeps the argument).
fn prologue(i: &Init) -> Vec<Inst> {
    let mut out = Vec::new();
    for (k, &r) in I64S.iter().enumerate().skip(1) {
        out.push(Inst::LoadInt {
            dst: Reg(r),
            val: i.ints[k],
            ty: IntTy::I64,
        });
    }
    for (k, &r) in I32S.iter().enumerate() {
        out.push(Inst::LoadInt {
            dst: Reg(r),
            val: i.ints[4 + k],
            ty: IntTy::I32,
        });
    }
    out.push(Inst::LoadInt {
        dst: Reg(U8S[0]),
        val: i.ints[6] & 0xFF,
        ty: IntTy::U8,
    });
    for (k, &r) in BOOLS.iter().enumerate() {
        out.push(Inst::LoadBool {
            dst: Reg(r),
            val: i.bools[k],
        });
    }
    for (k, &r) in F64S.iter().enumerate() {
        out.push(Inst::LoadInt {
            dst: Reg(I64S[3]),
            val: i.floats[k],
            ty: IntTy::I64,
        });
        out.push(Inst::IntToF64 {
            dst: Reg(r),
            src: Reg(I64S[3]),
            ty: IntTy::I64,
        });
    }
    out.push(Inst::LoadInt {
        dst: Reg(I64S[3]),
        val: i.ints[3],
        ty: IntTy::I64,
    });
    for (k, &r) in DYNS.iter().enumerate() {
        out.push(Inst::DLoadInt {
            dst: Reg(r),
            val: i.dyns[k],
        });
    }
    out
}

const NREGS: usize = 14;

/// Assembles prologue + generated code + an epilogue storing every register
/// into its global (so the whole register file is compared) + `ret r0`.
/// Generated branch targets (relative to the generated code) are shifted
/// past the prologue.
fn build(init: &Init, g: &Gen) -> Program {
    let mut m = ModuleBuilder::new();
    let globals: Vec<_> = types()
        .iter()
        .enumerate()
        .map(|(i, &t)| m.global(&format!("r{i}"), t, true, None))
        .collect();
    let mut f = m.function("main", &[ValType::I64], &[ValType::I64]);
    for t in &types()[1..] {
        let _ = f.reg(*t);
    }
    let pro = prologue(init);
    let shift = u32::try_from(pro.len()).unwrap_or(0);
    for inst in pro {
        f.emit(inst);
    }
    // The generated trailing `ret r0` is replaced by the epilogue.
    let body_len = g.code.len() - 1;
    let mut labels = Vec::new();
    for _ in 0..=body_len {
        labels.push(f.label());
    }
    for (pc, inst) in g.code[..body_len].iter().enumerate() {
        f.bind(labels[pc]);
        match *inst {
            Inst::Switch { src, ty, .. } => {
                let (targets, default) = &g.tables[0];
                let ls: Vec<_> = targets.iter().map(|&t| labels[t as usize]).collect();
                let _ = f.switch(ty, src, &ls, labels[*default as usize]);
            }
            Inst::Jmp { target } => {
                f.emit(Inst::Jmp {
                    target: Target(target.0 + shift),
                });
            }
            Inst::JmpIf { cond, target } => {
                f.emit(Inst::JmpIf {
                    cond,
                    target: Target(target.0 + shift),
                });
            }
            Inst::JmpIfNot { cond, target } => {
                f.emit(Inst::JmpIfNot {
                    cond,
                    target: Target(target.0 + shift),
                });
            }
            other => {
                f.emit(other);
            }
        }
    }
    f.bind(labels[body_len]);
    for (i, g) in globals.iter().enumerate() {
        f.emit(Inst::SetGlobal {
            global: *g,
            src: Reg(u16::try_from(i).unwrap_or(0)),
        });
    }
    f.ret(Reg(0));
    m.add_function(f).unwrap();
    Program::load(m.finish().unwrap(), &Host::new()).expect("generated programs load")
}

fn to_rv(ty: ValType, v: Value) -> Rv {
    match (ty, v) {
        (ValType::Bool, Value::Bool(b)) => Rv::Bool(b),
        (ValType::F64, Value::Float(f)) => Rv::Float(f),
        (ValType::Dyn, Value::Nil) => Rv::Dyn(Dv::Nil),
        (ValType::Dyn, Value::Bool(b)) => Rv::Dyn(Dv::Bool(b)),
        (ValType::Dyn, Value::Int(i)) => Rv::Dyn(Dv::Int(i)),
        (ValType::Dyn, Value::Float(f)) => Rv::Dyn(Dv::Float(f)),
        (_, Value::Int(i)) => Rv::Int(i128::from(i)),
        (_, Value::UInt(u)) => Rv::Int(i128::from(u)),
        (t, v) => panic!("unexpected {v:?} for {t}"),
    }
}

/// Equality that treats any two NaNs as equal (payloads are unspecified).
fn same(a: &[Rv], b: &[Rv]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| match (x, y) {
            (Rv::Float(p), Rv::Float(q)) | (Rv::Dyn(Dv::Float(p)), Rv::Dyn(Dv::Float(q))) => {
                (p.is_nan() && q.is_nan()) || p.to_bits() == q.to_bits()
            }
            _ => x == y,
        })
}

type Run = (Outcome, u64, Vec<Rv>);

fn vm_outcome(p: &Program, arg: i64, fuel: u64) -> Run {
    let mut vm = Vm::new(p);
    let out = vm.run_with(FuncId(0), &[Value::Int(arg)], Limits::new().with_fuel(fuel));
    let o = match out {
        Ok(Value::Int(i)) => Outcome::Ret(Rv::Int(i128::from(i))),
        Ok(other) => panic!("unexpected result {other:?}"),
        Err(VmError::Raised { kind, pc, .. }) => Outcome::Raised(kind, pc),
        Err(VmError::Trap { kind, pc, .. }) => Outcome::Trapped(kind, pc),
        Err(e) => panic!("unexpected error {e}"),
    };
    let globals = if matches!(o, Outcome::Ret(_)) {
        types()
            .iter()
            .enumerate()
            .map(|(i, &t)| {
                let g = bytecode_lang::GlobalId(u32::try_from(i).unwrap_or(0));
                to_rv(t, vm.global(g).unwrap_or(Value::Nil))
            })
            .collect()
    } else {
        Vec::new()
    };
    (o, vm.fuel_used(), globals)
}

fn reference_outcome(p: &Program, arg: i64, fuel: u64) -> Run {
    let f = &p.module().functions()[0];
    let tables = f.tables().to_vec();
    let args = [Rv::Int(i128::from(arg))];
    let (o, used, globals) = reference::run(f.code(), &types(), &tables, &args, fuel, NREGS);
    let globals = if matches!(o, Outcome::Ret(_)) {
        globals
    } else {
        Vec::new()
    };
    (o, used, globals)
}

fn agree(a: &Run, b: &Run) -> bool {
    a.0 == b.0 && a.1 == b.1 && same(&a.2, &b.2)
}

fn arg() -> impl Strategy<Value = i64> {
    prop_oneof![-5i64..5, Just(i64::MAX), Just(i64::MIN), any::<i64>()]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    /// Straight-line programs: same result or same error at the same pc.
    #[test]
    fn prop_straight_line_programs_match_the_reference(i in init(), g in straight_program(), a in arg()) {
        let p = build(&i, &g);
        let (v, r) = (vm_outcome(&p, a, 1_000), reference_outcome(&p, a, 1_000));
        prop_assert!(agree(&v, &r), "vm {:?} ref {:?}", v, r);
    }

    /// Programs with arbitrary branches and loops: same outcome and the same
    /// fuel consumed, whether they return, fail, or run out of fuel.
    #[test]
    fn prop_cfg_programs_match_the_reference(i in init(), g in cfg_program(), a in arg(), fuel in 0u64..300) {
        let p = build(&i, &g);
        let (v, r) = (vm_outcome(&p, a, fuel), reference_outcome(&p, a, fuel));
        prop_assert!(agree(&v, &r), "vm {:?} ref {:?}", v, r);
    }
}

#[test]
fn reference_and_vm_agree_on_a_known_loop() {
    // r1 = 0; loop: r1 += 1; if r1 < 10 goto loop (with a safepoint).
    let code = vec![
        Inst::LoadInt {
            dst: Reg(1),
            val: 0,
            ty: IntTy::I64,
        },
        Inst::LoadInt {
            dst: Reg(2),
            val: 10,
            ty: IntTy::I64,
        },
        Inst::LoadInt {
            dst: Reg(3),
            val: 1,
            ty: IntTy::I64,
        },
        Inst::IAdd {
            dst: Reg(1),
            lhs: Reg(1),
            rhs: Reg(3),
            op: IntOp::new(IntTy::I64),
        },
        Inst::Safepoint {},
        Inst::ILt {
            dst: Reg(7),
            lhs: Reg(1),
            rhs: Reg(2),
            ty: IntTy::I64,
        },
        Inst::JmpIf {
            cond: Reg(7),
            target: Target(3),
        },
        Inst::Mov {
            dst: Reg(0),
            src: Reg(1),
        },
        Inst::Ret { src: Reg(0) },
    ];
    let init = Init {
        ints: [0; 7],
        bools: [false; 2],
        floats: [0; 2],
        dyns: [0; 3],
    };
    let p = build(
        &init,
        &Gen {
            code,
            tables: Vec::new(),
        },
    );
    let (v, r) = (vm_outcome(&p, 0, 1_000), reference_outcome(&p, 0, 1_000));
    assert_eq!(v.0, Outcome::Ret(Rv::Int(10)));
    assert_eq!(v.1, 19);
    assert!(agree(&v, &r));
    // With 5 units the run traps at the 6th charge.
    assert!(agree(&vm_outcome(&p, 0, 5), &reference_outcome(&p, 0, 5)));
}

// ===========================================================================
// Whole programs: calls, closures, heap objects, exceptions with `finally`,
// and coroutines, against the module reference in `common/full.rs`.
// ===========================================================================

mod whole {
    use super::*;
    use bytecode_lang::{
        ArgKind, Const, ConstId, ErrorKind, Field, FieldIdx, FunctionBuilder, GlobalId, Kind,
        Label, NameRef, Param, ParamKind, ParamList, ShapeId, StrId, StructDef, TypeDef, TypeRef,
    };
    use common::full::{self, End, Shape, V, float_bits};

    // Register file of every generated function (params r0, r1):
    // r0..r7 dyn (r2, r3 generic; r4 arrays; r5 maps; r6, r7 coroutines),
    // r8/r9 and r19/r20 loop counter and bound per nesting level (i64),
    // r10 bool, r11/r12 and r21/r22 `finally` completion kind (i8) and
    // value (dyn) per level, r13 catch, r14..r16 call window, r17 u8,
    // r18 u32, r23 callee, r24 i64 scratch, r25 iterator, r26 bool, r27 i64
    // constant 1, r28..r32 a dynamic-call window (dst r28, up to four
    // arguments), r33 i64 position, r34 bool, r35 dyn key.
    pub(super) fn regs() -> Vec<ValType> {
        use ValType::{Bool as B, Dyn as Dy, I8, I64 as I, U8, U32};
        vec![
            Dy, Dy, Dy, Dy, Dy, Dy, Dy, Dy, I, I, B, I8, Dy, Dy, Dy, Dy, Dy, U8, U32, I, I, I8, Dy,
            Dy, I, Dy, B, I, Dy, Dy, Dy, Dy, Dy, I, B, Dy,
        ]
    }
    const W2: Reg = Reg(28);
    const POS: Reg = Reg(33);
    const FLAG: Reg = Reg(34);
    const KEYR: Reg = Reg(35);
    const COND: Reg = Reg(10);
    const CATCH: Reg = Reg(13);
    const WIN: Reg = Reg(14);
    const A0: Reg = Reg(15);
    const A1: Reg = Reg(16);
    const U8R: Reg = Reg(17);
    const U32R: Reg = Reg(18);
    const CALLEE: Reg = Reg(23);
    const SCRATCH: Reg = Reg(24);
    const ITER: Reg = Reg(25);
    const HAS: Reg = Reg(26);
    const ONE: Reg = Reg(27);

    /// One generated item: an instruction group or a structure marker.
    #[derive(Clone, Debug)]
    pub(super) enum Op {
        Load(u8, i32),
        LoadStr(u8, u8),
        Arith(u8, u8, u8, u8, Policy),
        Mov(u8, u8),
        GetG(u8, u8),
        SetG(u8, u8),
        NewArr(u8, i32),
        Push(u8, u8),
        AGet(u8, u8, i32),
        ASet(u8, i32, u8),
        ALen(u8, u8),
        APop(u8, u8),
        NewMap(u8),
        MSet(u8, u8, u8),
        MGet(u8, u8, u8, bool),
        MHas(u8, u8, u8),
        MDel(u8, u8),
        MPush(u8, u8),
        MLen(u8, u8),
        NewSt(u8),
        GetF(u8, u8, u8),
        SetF(u8, u8, u8),
        Concat(u8, u8, u8),
        SLen(u8, u8),
        Call(u8, u8, u8, u8),
        Closure(u8, u8, u8, u8, u8, u8),
        Tail(u8, u8, u8),
        Throw(u8),
        ErrCode(u8, u8),
        Ret(u8),
        Safepoint,
        CoroNew(u8, u8, u8, u8),
        Resume(u8, u8, u8),
        ResumeThrow(u8, u8, u8),
        Close(u8, u8, u8),
        Yield(u8, u8),
        YieldKv(u8, u8, u8),
        Await(u8, u8),
        Status(u8, u8),
        Key(u8, u8),
        Result(u8, u8),
        Current(u8),
        Spawn(u8, u8),
        ForGen(u8, u8, u8, u8, bool),
        // Format 2.
        Pow(u8, u8, u8, Policy),
        Abs(u8, u8, Policy),
        Shift(u8, u8, u8, Policy, bool),
        Dup(u8, u8),
        DGet(u8, u8, u8, i32),
        DSet(u8, u8, i32, u8),
        Sep(u8, u8, u8, i32),
        GetP(u8, u8, u8),
        SetP(u8, u8, u8),
        SepP(u8, u8, u8),
        NewRef(u8, u8),
        CellGet(u8, u8),
        CellSet(u8, u8),
        RefIdx(u8, u8, u8, i32),
        BindIdx(u8, u8, i32, u8),
        UnrefIdx(u8, u8, i32),
        RefP(u8, u8, u8),
        BindP(u8, u8, u8),
        UnrefP(u8, u8),
        Raise(u8, u8),
        Payload(u8, u8),
        DCallN(u8, u8, u8, [u8; 4]),
        DCallS(u8, u8, u8, [u8; 4]),
        ParamRef(u8, u8, i32),
        ParamRefNamed(u8, u8, u8),
        OpenIf(u8, u8),
        OpenLoop(i32, bool),
        OpenTry,
        OpenFinally(bool),
        Else,
        End,
    }

    fn make_op(k: u8, x: [u8; 6], i: i32, p: Policy) -> Op {
        let [a, b, c, d, e, f] = x;
        match k {
            0..6 => Op::Load(a, i),
            6..9 => Op::LoadStr(a, b),
            9..15 => Op::Arith(a, b, c, d, p),
            15..20 => Op::Resume(a, b, c),
            20..23 => Op::Mov(a, b),
            23..25 => Op::GetG(a, b),
            25..27 => Op::SetG(a, b),
            27..29 => Op::NewArr(a, i % 4),
            29..31 => Op::Push(a, b),
            31 => Op::AGet(a, b, i % 4),
            32 => Op::ASet(a, i % 4, b),
            33 => Op::ALen(a, b),
            34 => Op::APop(a, b),
            35..37 => Op::NewMap(a),
            37..39 => Op::MSet(a, b, c),
            39 => Op::MGet(a, b, c, false),
            40 => Op::MGet(a, b, c, true),
            41 => Op::MHas(a, b, c),
            42 => Op::MDel(a, b),
            43 => Op::MPush(a, b),
            44 => Op::MLen(a, b),
            45 => Op::NewSt(a),
            46 => Op::GetF(a, b, c),
            47 => Op::SetF(a, b, c),
            48 => Op::Concat(a, b, c),
            49 => Op::SLen(a, b),
            50..53 => Op::Call(a, b, c, d),
            53..55 => Op::Closure(a, b, c, d, e, f),
            55 => Op::Tail(a, b, c),
            56..58 => Op::Throw(a),
            58 => Op::ErrCode(a, b),
            59 => Op::Ret(a),
            60 => Op::Safepoint,
            61..64 => Op::CoroNew(a, b, c, d),
            64..68 => Op::Resume(a, b, c),
            68 => Op::ResumeThrow(a, b, c),
            69..71 => Op::Close(a, b, c),
            71..75 => Op::Yield(a, b),
            75 => Op::YieldKv(a, b, c),
            76 => Op::Await(a, b),
            77 => Op::Status(a, b),
            78 => Op::Key(a, b),
            79 if x[5] % 2 == 0 => Op::YieldKv(a, b, c),
            79 => Op::Result(a, b),
            80 => Op::Current(a),
            81 => Op::Spawn(a, b),
            82..84 => Op::ForGen(a, b, c, d, e % 2 == 0),
            84..87 => Op::OpenIf(a, b),
            87..89 => Op::OpenLoop(i.rem_euclid(4), a % 2 == 0),
            89..92 => Op::OpenTry,
            92..95 => Op::OpenFinally(a % 3 == 0),
            95..97 => Op::Else,
            97..100 => Op::End,
            100 => Op::Pow(a, b, c, p),
            101 => Op::Abs(a, b, p),
            102 => Op::Shift(a, b, c, p, d % 2 == 0),
            103..105 => Op::Dup(a, b),
            105 => Op::DGet(a, b, c, i % 4),
            106 => Op::DSet(a, b, i % 4, c),
            107..109 => Op::Sep(a, b, c, i % 4),
            109 => Op::GetP(a, b, c),
            110 => Op::SetP(a, b, c),
            111 => Op::SepP(a, b, c),
            112..114 => Op::NewRef(a, b),
            114 => Op::CellGet(a, b),
            115 => Op::CellSet(a, b),
            116..118 => Op::RefIdx(a, b, c, i % 4),
            118 => Op::BindIdx(a, b, i % 4, c),
            119 => Op::UnrefIdx(a, b, i % 4),
            120 => Op::RefP(a, b, c),
            121 => Op::BindP(a, b, c),
            122 => Op::UnrefP(a, b),
            123 => Op::Raise(a, b),
            124 => Op::Payload(a, b),
            125..128 => Op::DCallN(a, b, c, [d, e, f, a ^ b]),
            128..131 => Op::DCallS(a, b, c, [d, e, f, a ^ c]),
            131 => Op::ParamRef(a, b, i % 6 - 1),
            _ => Op::ParamRefNamed(a, b, c),
        }
    }

    /// An op and whether it gets its own catch-all region (most do, so a
    /// run continues past the errors random code raises).
    pub(super) fn op() -> impl Strategy<Value = (Op, bool)> {
        (0u8..133, any::<[u8; 6]>(), -3i32..12, dyn_policy(), 0u8..10)
            .prop_map(|(k, x, i, p, g)| (make_op(k, x, i, p), g < 6))
    }

    /// The op kinds of PHP-style container code: loads and moves, typed and
    /// dynamic container access, `dup`, separation, references, dynamic
    /// calls with shapes and by-reference decisions, `raise`.
    const PHP_KINDS: &[u8] = &[
        0, 20, 21, 27, 29, 30, 31, 32, 34, 35, 37, 39, 40, 42, 43, 45, 46, 47, 100, 101, 103, 104,
        105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122,
        123, 124, 125, 126, 128, 129, 131, 132,
    ];

    /// An op of [`PHP_KINDS`], always guarded (so errors never end a run
    /// early and the final state is compared after every op ran).
    pub(super) fn php_op() -> impl Strategy<Value = (Op, bool)> {
        (
            proptest::sample::select(PHP_KINDS),
            any::<[u8; 6]>(),
            -1i32..5,
            dyn_policy(),
        )
            .prop_map(|(k, x, i, p)| (make_op(k, x, i, p), true))
    }

    /// A generic destination: r0..r3, mostly r2/r3.
    fn dst(v: u8) -> Reg {
        Reg([2, 3, 2, 3, 0, 1][usize::from(v % 6)])
    }
    /// A generic source: any dyn register or the catch register.
    fn src(v: u8) -> Reg {
        Reg([0, 1, 2, 3, 4, 5, 6, 7, 13][usize::from(v % 9)])
    }
    /// A register of a role (`r`), sometimes another one (error paths).
    fn role(r: u16, v: u8) -> Reg {
        if v < 224 { Reg(r) } else { src(v) }
    }
    fn coro(v: u8) -> Reg {
        role(6 + u16::from(v % 2), v)
    }
    fn func(v: u8) -> FuncId {
        FuncId(u32::from(v % 5))
    }
    /// A container role: the array, the map, the struct, or a generic.
    fn cont(v: u8) -> Reg {
        Reg([4, 5, 3, 4, 5, 2][usize::from(v % 6)])
    }
    /// The property names the generator uses: the struct's two fields, an
    /// absent name, and a parameter name.
    const NAMES: [&str; 4] = ["a", "b", "x", "p"];
    /// Dynamic-call targets without captures: three generated functions and
    /// the two with parameter lists (f6, f7).
    const CALLEES: [u32; 7] = [0, 1, 2, 3, 4, 6, 7];

    enum Open {
        If {
            else_l: Label,
            end_l: Label,
            in_else: bool,
        },
        Loop {
            top: Label,
            done: Label,
            level: u16,
        },
        Try {
            start: Label,
            end: Label,
            h: Label,
            after: Label,
            in_catch: bool,
        },
        Fin {
            start: Label,
            end: Label,
            h: Label,
            fin: Label,
            after: Label,
            level: u16,
            in_fin: bool,
            over: bool,
        },
    }

    struct Gen<'a> {
        f: &'a mut FunctionBuilder,
        arr: TypeRef,
        map: TypeRef,
        st: TypeRef,
        strs: [ConstId; 3],
        closure_fn: FuncId,
        open: Vec<Open>,
        names: [NameRef; 4],
        shapes: Vec<(ShapeId, Vec<ArgKind>)>,
    }

    impl Gen<'_> {
        fn e(&mut self, i: Inst) {
            let _ = self.f.emit(i);
        }
        fn mov(&mut self, d: Reg, s: Reg) {
            let _ = self.f.mov(d, s);
        }
        fn loops(&self) -> u16 {
            let n = self
                .open
                .iter()
                .filter(|o| matches!(o, Open::Loop { .. }))
                .count();
            u16::try_from(n).unwrap_or(0)
        }
        fn fins(&self) -> u16 {
            let n = self
                .open
                .iter()
                .filter(|o| matches!(o, Open::Fin { .. }))
                .count();
            u16::try_from(n).unwrap_or(0)
        }
        fn in_try(&self) -> bool {
            self.open
                .iter()
                .any(|o| matches!(o, Open::Try { .. } | Open::Fin { .. }))
        }
        fn args(&mut self, a: u8, b: u8) {
            self.mov(A0, src(a));
            self.mov(A1, src(b));
        }
        /// A key: a small int in `KEYR`, or any register.
        fn key(&mut self, k: u8, i: i32) -> Reg {
            if k % 3 == 0 {
                src(k)
            } else {
                self.e(Inst::DLoadInt { dst: KEYR, val: i });
                KEYR
            }
        }
        /// A callee in `CALLEE`: a function value, or (sometimes) any
        /// register, a non-callable included.
        fn callee(&mut self, v: u8) -> Reg {
            if v % 8 == 7 {
                return src(v / 8);
            }
            let f = FuncId(CALLEES[usize::from(v % 7)]);
            self.e(Inst::MakeClosure {
                dst: CALLEE,
                func: f,
            });
            CALLEE
        }
        fn box_dyn(&mut self, d: Reg, s: Reg, from: Prim) {
            self.e(Inst::ToDyn {
                dst: d,
                src: s,
                from,
            });
        }

        /// Emits `op`, inside its own catch-all region when `guard`ed: the
        /// handler is the next instruction, so an error is swallowed (the
        /// catch register keeps it) and execution continues.
        fn guarded(&mut self, op: &Op, guard: bool) {
            let plain = !matches!(
                op,
                Op::Tail(..)
                    | Op::OpenIf(..)
                    | Op::OpenLoop(..)
                    | Op::OpenTry
                    | Op::OpenFinally(..)
                    | Op::Else
                    | Op::End
            );
            if !(guard && plain) {
                self.op(op);
                return;
            }
            let (s, e) = (self.f.label(), self.f.label());
            self.f.bind(s);
            self.op(op);
            self.f.bind(e);
            self.f.try_region(s, e, e, CATCH);
        }

        #[allow(clippy::too_many_lines)]
        fn op(&mut self, op: &Op) {
            match *op {
                Op::Load(d, v) => self.e(Inst::DLoadInt {
                    dst: dst(d),
                    val: v,
                }),
                Op::LoadStr(d, w) => {
                    let k = self.strs[usize::from(w % 3)];
                    self.e(Inst::DLoadConst { dst: dst(d), k });
                }
                Op::Arith(w, d, a, b, pol) => {
                    let (d, lhs, rhs) = (dst(d), src(a), src(b));
                    match w % 7 {
                        0 => self.e(Inst::DAdd {
                            dst: d,
                            lhs,
                            rhs,
                            pol,
                        }),
                        1 => self.e(Inst::DSub {
                            dst: d,
                            lhs,
                            rhs,
                            pol,
                        }),
                        2 => self.e(Inst::DMul {
                            dst: d,
                            lhs,
                            rhs,
                            pol,
                        }),
                        3 => self.e(Inst::DDiv {
                            dst: d,
                            lhs,
                            rhs,
                            pol,
                        }),
                        4 => self.e(Inst::DConcat { dst: d, lhs, rhs }),
                        5 => {
                            self.e(Inst::DLt {
                                dst: COND,
                                lhs,
                                rhs,
                            });
                            self.box_dyn(d, COND, Prim::Bool);
                        }
                        _ => {
                            self.e(Inst::DEq {
                                dst: COND,
                                lhs,
                                rhs,
                            });
                            self.box_dyn(d, COND, Prim::Bool);
                        }
                    }
                }
                Op::Mov(d, s) => self.mov(Reg([2, 3, 4, 5, 6, 7][usize::from(d % 6)]), src(s)),
                Op::GetG(d, g) => self.e(Inst::GetGlobal {
                    dst: dst(d),
                    global: GlobalId(u32::from(g % 2)),
                }),
                Op::SetG(g, s) => self.e(Inst::SetGlobal {
                    global: GlobalId(u32::from(g % 2)),
                    src: src(s),
                }),
                Op::NewArr(d, len) => {
                    self.e(Inst::LoadInt {
                        dst: SCRATCH,
                        val: len,
                        ty: IntTy::I64,
                    });
                    let ty = self.arr;
                    self.e(Inst::NewArray {
                        dst: role(4, d),
                        len: SCRATCH,
                        ty,
                    });
                }
                Op::Push(a, v) => self.e(Inst::ArrayPush {
                    arr: role(4, a),
                    src: src(v),
                }),
                Op::AGet(d, a, i) => {
                    self.e(Inst::LoadInt {
                        dst: SCRATCH,
                        val: i,
                        ty: IntTy::I64,
                    });
                    self.e(Inst::ArrayGet {
                        dst: dst(d),
                        arr: role(4, a),
                        idx: SCRATCH,
                    });
                }
                Op::ASet(a, i, v) => {
                    self.e(Inst::LoadInt {
                        dst: SCRATCH,
                        val: i,
                        ty: IntTy::I64,
                    });
                    self.e(Inst::ArraySet {
                        arr: role(4, a),
                        idx: SCRATCH,
                        src: src(v),
                    });
                }
                Op::ALen(d, a) => {
                    self.e(Inst::ArrayLen {
                        dst: SCRATCH,
                        arr: role(4, a),
                    });
                    self.box_dyn(dst(d), SCRATCH, Prim::I64);
                }
                Op::APop(d, a) => self.e(Inst::ArrayPop {
                    dst: dst(d),
                    arr: role(4, a),
                }),
                Op::NewMap(d) => {
                    let ty = self.map;
                    self.e(Inst::NewMap {
                        dst: role(5, d),
                        ty,
                    });
                }
                Op::MSet(m, k, v) => self.e(Inst::MapSet {
                    map: role(5, m),
                    key: src(k),
                    src: src(v),
                }),
                Op::MGet(d, m, k, find) => {
                    let (dst, map, key) = (dst(d), role(5, m), src(k));
                    if find {
                        self.e(Inst::MapFind { dst, map, key });
                    } else {
                        self.e(Inst::MapGet { dst, map, key });
                    }
                }
                Op::MHas(d, m, k) => {
                    self.e(Inst::MapHas {
                        dst: COND,
                        map: role(5, m),
                        key: src(k),
                    });
                    self.box_dyn(dst(d), COND, Prim::Bool);
                }
                Op::MDel(m, k) => self.e(Inst::MapDel {
                    map: role(5, m),
                    key: src(k),
                }),
                Op::MPush(m, v) => self.e(Inst::MapPush {
                    map: role(5, m),
                    src: src(v),
                }),
                Op::MLen(d, m) => {
                    self.e(Inst::MapLen {
                        dst: SCRATCH,
                        map: role(5, m),
                    });
                    self.box_dyn(dst(d), SCRATCH, Prim::I64);
                }
                Op::NewSt(d) => {
                    let ty = self.st;
                    self.e(Inst::NewStruct {
                        dst: role(3, d),
                        ty,
                    });
                }
                Op::GetF(d, o, fl) => self.e(Inst::GetField {
                    dst: dst(d),
                    obj: role(3, o),
                    field: FieldIdx(u16::from(fl % 2)),
                }),
                Op::SetF(o, fl, v) => self.e(Inst::SetField {
                    obj: role(3, o),
                    field: FieldIdx(u16::from(fl % 2)),
                    src: src(v),
                }),
                Op::Concat(d, a, b) => self.e(Inst::StrConcat {
                    dst: dst(d),
                    lhs: src(a),
                    rhs: src(b),
                }),
                Op::SLen(d, s) => {
                    self.e(Inst::StrLen {
                        dst: SCRATCH,
                        s: src(s),
                    });
                    self.box_dyn(dst(d), SCRATCH, Prim::I64);
                }
                Op::Call(d, fv, a, b) => {
                    self.args(a, b);
                    self.e(Inst::Call {
                        dst: WIN,
                        func: func(fv),
                        argc: 2,
                    });
                    self.mov(dst(d), WIN);
                }
                Op::Closure(d, ca, cb, a, b, mode) => {
                    self.args(ca, cb);
                    let cf = self.closure_fn;
                    self.e(Inst::MakeClosure { dst: WIN, func: cf });
                    self.mov(CALLEE, WIN);
                    self.args(a, b);
                    match mode % 3 {
                        0 => self.e(Inst::CallIndirect {
                            dst: WIN,
                            callee: CALLEE,
                            argc: 2,
                        }),
                        1 => self.e(Inst::DCall {
                            dst: WIN,
                            callee: CALLEE,
                            argc: 2,
                        }),
                        _ => self.e(Inst::CoroNewIndirect {
                            dst: WIN,
                            callee: CALLEE,
                            argc: 2,
                        }),
                    }
                    let out = if mode % 3 == 2 { coro(d) } else { dst(d) };
                    self.mov(out, WIN);
                }
                Op::Tail(fv, a, b) => {
                    // No tail call inside a try region (LSB V-CF4).
                    if !self.in_try() {
                        self.args(a, b);
                        self.e(Inst::TailCall {
                            func: func(fv),
                            args: A0,
                            argc: 2,
                        });
                    }
                }
                Op::Throw(s) => self.e(Inst::Throw { src: src(s) }),
                Op::ErrCode(d, s) => {
                    self.e(Inst::ErrCode {
                        dst: U32R,
                        src: src(s),
                    });
                    self.box_dyn(dst(d), U32R, Prim::U32);
                }
                Op::Ret(s) => self.ret(src(s)),
                Op::Safepoint => self.e(Inst::Safepoint {}),
                Op::CoroNew(d, fv, a, b) => {
                    self.args(a, b);
                    self.e(Inst::CoroNew {
                        dst: WIN,
                        func: func(fv),
                        argc: 2,
                    });
                    self.mov(coro(d), WIN);
                }
                Op::Resume(d, c, v) => self.e(Inst::Resume {
                    dst: dst(d),
                    coro: coro(c),
                    src: src(v),
                }),
                Op::ResumeThrow(d, c, v) => self.e(Inst::ResumeThrow {
                    dst: dst(d),
                    coro: coro(c),
                    src: src(v),
                }),
                Op::Close(d, c, v) => self.e(Inst::CoroClose {
                    dst: dst(d),
                    coro: coro(c),
                    src: src(v),
                }),
                Op::Yield(d, v) => self.e(Inst::Yield {
                    dst: dst(d),
                    src: src(v),
                }),
                Op::YieldKv(d, k, v) => self.e(Inst::YieldKv {
                    dst: dst(d),
                    key: src(k),
                    src: src(v),
                }),
                Op::Await(d, v) => self.e(Inst::Await {
                    dst: dst(d),
                    src: src(v),
                }),
                Op::Status(d, c) => {
                    self.e(Inst::CoroStatus {
                        dst: U8R,
                        coro: coro(c),
                    });
                    self.box_dyn(dst(d), U8R, Prim::U8);
                }
                Op::Key(d, c) => self.e(Inst::CoroKey {
                    dst: dst(d),
                    coro: coro(c),
                }),
                Op::Result(d, c) => self.e(Inst::CoroResult {
                    dst: dst(d),
                    coro: coro(c),
                }),
                Op::Current(d) => self.e(Inst::CoroCurrent { dst: coro(d) }),
                Op::Spawn(d, fv) => {
                    self.e(Inst::MakeClosure {
                        dst: CALLEE,
                        func: func(fv),
                    });
                    self.e(Inst::Spawn {
                        dst: WIN,
                        callee: CALLEE,
                        argc: 2,
                    });
                    self.mov(dst(d), WIN);
                }
                Op::ForGen(acc, fv, a, b, keys) => {
                    let acc = dst(acc);
                    self.e(Inst::DLoadInt { dst: acc, val: 0 });
                    self.args(a, b);
                    self.e(Inst::CoroNew {
                        dst: WIN,
                        func: func(fv),
                        argc: 2,
                    });
                    self.e(Inst::DIterNew {
                        dst: ITER,
                        src: WIN,
                    });
                    let (top, done) = (self.f.label(), self.f.label());
                    self.f.bind(top);
                    self.e(Inst::IterNext {
                        has: HAS,
                        iter: ITER,
                        val: WIN,
                    });
                    let _ = self.f.jmp_if_not(HAS, done);
                    if keys {
                        self.e(Inst::IterKey {
                            dst: WIN,
                            iter: ITER,
                        });
                    }
                    self.e(Inst::DAdd {
                        dst: acc,
                        lhs: acc,
                        rhs: WIN,
                        pol: Policy::new(),
                    });
                    self.e(Inst::Safepoint {});
                    let _ = self.f.jmp(top);
                    self.f.bind(done);
                }
                Op::Pow(d, a, b, pol) => self.e(Inst::DPow {
                    dst: dst(d),
                    lhs: src(a),
                    rhs: src(b),
                    pol,
                }),
                Op::Abs(d, a, pol) => self.e(Inst::DAbs {
                    dst: dst(d),
                    src: src(a),
                    pol,
                }),
                Op::Shift(d, a, b, pol, left) => {
                    let (dst, lhs, rhs) = (dst(d), src(a), src(b));
                    if left {
                        self.e(Inst::DShl { dst, lhs, rhs, pol });
                    } else {
                        self.e(Inst::DShr { dst, lhs, rhs, pol });
                    }
                }
                Op::Dup(d, s) => self.e(Inst::Dup {
                    dst: Reg([2, 3, 4, 5, 2, 3][usize::from(d % 6)]),
                    src: src(s),
                }),
                Op::DGet(d, c, k, i) => {
                    let key = self.key(k, i);
                    self.e(Inst::DGetIndex {
                        dst: dst(d),
                        obj: cont(c),
                        key,
                    });
                }
                Op::DSet(c, k, i, s) => {
                    let key = self.key(k, i);
                    self.e(Inst::DSetIndex {
                        obj: cont(c),
                        key,
                        src: src(s),
                    });
                }
                Op::Sep(d, c, k, i) => {
                    let key = self.key(k, i);
                    self.e(Inst::DSepIndex {
                        dst: Reg([2, 3, 4, 5][usize::from(d % 4)]),
                        obj: cont(c),
                        key,
                    });
                }
                Op::GetP(d, c, n) => {
                    let name = self.names[usize::from(n % 4)];
                    self.e(Inst::GetProp {
                        dst: dst(d),
                        obj: cont(c),
                        name,
                    });
                }
                Op::SetP(c, n, s) => {
                    let name = self.names[usize::from(n % 4)];
                    self.e(Inst::SetProp {
                        obj: cont(c),
                        name,
                        src: src(s),
                    });
                }
                Op::SepP(d, c, n) => {
                    let name = self.names[usize::from(n % 4)];
                    self.e(Inst::DSepProp {
                        dst: Reg([2, 3, 4, 5][usize::from(d % 4)]),
                        obj: cont(c),
                        name,
                    });
                }
                Op::NewRef(d, s) => self.e(Inst::NewRef {
                    dst: dst(d),
                    src: src(s),
                }),
                Op::CellGet(d, c) => self.e(Inst::CellGet {
                    dst: dst(d),
                    cell: src(c),
                }),
                Op::CellSet(c, s) => self.e(Inst::CellSet {
                    cell: src(c),
                    src: src(s),
                }),
                Op::RefIdx(d, c, k, i) => {
                    let key = self.key(k, i);
                    self.e(Inst::DRefIndex {
                        dst: dst(d),
                        obj: cont(c),
                        key,
                    });
                }
                Op::BindIdx(c, k, i, s) => {
                    let key = self.key(k, i);
                    self.e(Inst::DBindIndex {
                        obj: cont(c),
                        key,
                        src: src(s),
                    });
                }
                Op::UnrefIdx(c, k, i) => {
                    let key = self.key(k, i);
                    self.e(Inst::DUnrefIndex { obj: cont(c), key });
                }
                Op::RefP(d, c, n) => {
                    let name = self.names[usize::from(n % 4)];
                    self.e(Inst::DRefProp {
                        dst: dst(d),
                        obj: cont(c),
                        name,
                    });
                }
                Op::BindP(c, n, s) => {
                    let name = self.names[usize::from(n % 4)];
                    self.e(Inst::DBindProp {
                        obj: cont(c),
                        name,
                        src: src(s),
                    });
                }
                Op::UnrefP(c, n) => {
                    let name = self.names[usize::from(n % 4)];
                    self.e(Inst::DUnrefProp { obj: cont(c), name });
                }
                Op::Raise(k, s) => {
                    let kind = [
                        ErrorKind::NoMatch,
                        ErrorKind::TypeError,
                        ErrorKind::ArgumentError,
                        ErrorKind::KeyNotFound,
                        ErrorKind::NegativeExponent,
                    ][usize::from(k % 5)];
                    self.e(Inst::Raise { src: src(s), kind });
                }
                Op::Payload(d, s) => self.e(Inst::ErrPayload {
                    dst: dst(d),
                    src: src(s),
                }),
                Op::DCallN(d, cv, n, a) => {
                    let callee = self.callee(cv);
                    let n = n % 5;
                    for (i, &x) in a.iter().take(usize::from(n)).enumerate() {
                        self.mov(Reg(29 + i as u16), src(x));
                    }
                    self.e(Inst::DCall {
                        dst: W2,
                        callee,
                        argc: n,
                    });
                    self.mov(dst(d), W2);
                }
                Op::DCallS(d, cv, sh, a) => {
                    let callee = self.callee(cv);
                    let (shape, kinds) = self.shapes[usize::from(sh) % self.shapes.len()].clone();
                    for (i, (k, &x)) in kinds.iter().zip(a.iter()).enumerate() {
                        let r = match k {
                            ArgKind::Spread | ArgKind::SpreadNamed if x % 4 != 3 => cont(x % 2),
                            _ => src(x),
                        };
                        self.mov(Reg(29 + i as u16), r);
                    }
                    self.e(Inst::DCallShape {
                        dst: W2,
                        callee,
                        shape,
                    });
                    self.mov(dst(d), W2);
                }
                Op::ParamRef(d, cv, pos) => {
                    let callee = self.callee(cv);
                    self.e(Inst::LoadInt {
                        dst: POS,
                        val: pos,
                        ty: IntTy::I64,
                    });
                    self.e(Inst::DParamRef {
                        dst: FLAG,
                        callee,
                        pos: POS,
                    });
                    self.box_dyn(dst(d), FLAG, Prim::Bool);
                }
                Op::ParamRefNamed(d, cv, n) => {
                    let callee = self.callee(cv);
                    let name = self.names[usize::from(n % 4)];
                    self.e(Inst::DParamRefNamed {
                        dst: FLAG,
                        callee,
                        name,
                    });
                    self.box_dyn(dst(d), FLAG, Prim::Bool);
                }
                Op::OpenIf(a, b) => {
                    if self.open.len() < 2 {
                        let (else_l, end_l) = (self.f.label(), self.f.label());
                        self.e(Inst::DLt {
                            dst: COND,
                            lhs: src(a),
                            rhs: src(b),
                        });
                        let _ = self.f.jmp_if_not(COND, else_l);
                        self.open.push(Open::If {
                            else_l,
                            end_l,
                            in_else: false,
                        });
                    }
                }
                Op::OpenLoop(n, sp) => {
                    if self.open.len() < 2 {
                        let level = self.loops();
                        let (ctr, bound) = if level == 0 {
                            (Reg(8), Reg(9))
                        } else {
                            (Reg(19), Reg(20))
                        };
                        self.e(Inst::LoadInt {
                            dst: ctr,
                            val: 0,
                            ty: IntTy::I64,
                        });
                        self.e(Inst::LoadInt {
                            dst: bound,
                            val: n,
                            ty: IntTy::I64,
                        });
                        let (top, done) = (self.f.label(), self.f.label());
                        self.f.bind(top);
                        self.e(Inst::ILt {
                            dst: COND,
                            lhs: ctr,
                            rhs: bound,
                            ty: IntTy::I64,
                        });
                        let _ = self.f.jmp_if_not(COND, done);
                        if sp {
                            self.e(Inst::Safepoint {});
                        }
                        self.open.push(Open::Loop { top, done, level });
                    }
                }
                Op::OpenTry => {
                    if self.open.len() < 2 {
                        let (start, end, h, after) = (
                            self.f.label(),
                            self.f.label(),
                            self.f.label(),
                            self.f.label(),
                        );
                        self.f.bind(start);
                        self.e(Inst::Nop {});
                        self.open.push(Open::Try {
                            start,
                            end,
                            h,
                            after,
                            in_catch: false,
                        });
                    }
                }
                Op::OpenFinally(over) => {
                    if self.open.len() < 2 {
                        let level = self.fins();
                        let (start, end, h, fin, after) = (
                            self.f.label(),
                            self.f.label(),
                            self.f.label(),
                            self.f.label(),
                            self.f.label(),
                        );
                        self.f.bind(start);
                        self.e(Inst::Nop {});
                        self.open.push(Open::Fin {
                            start,
                            end,
                            h,
                            fin,
                            after,
                            level,
                            in_fin: false,
                            over,
                        });
                    }
                }
                Op::Else => self.else_(),
                Op::End => self.close(),
            }
        }

        /// The entry function's final state, observable through global 1:
        /// `[r0, ..., r7, key(r6), key(r7)]` (the coroutines' states show in
        /// their shapes).
        fn epilogue(&mut self) {
            let arr = self.arr;
            self.e(Inst::LoadInt {
                dst: SCRATCH,
                val: 0,
                ty: IntTy::I64,
            });
            self.e(Inst::NewArray {
                dst: WIN,
                len: SCRATCH,
                ty: arr,
            });
            for r in 0..8 {
                self.e(Inst::ArrayPush {
                    arr: WIN,
                    src: Reg(r),
                });
            }
            for c in [6u16, 7] {
                let (s, e) = (self.f.label(), self.f.label());
                self.e(Inst::LoadNil { dst: A0 });
                self.f.bind(s);
                self.e(Inst::CoroKey {
                    dst: A0,
                    coro: Reg(c),
                });
                self.f.bind(e);
                self.f.try_region(s, e, e, CATCH);
                self.e(Inst::ArrayPush { arr: WIN, src: A0 });
            }
            self.e(Inst::SetGlobal {
                global: GlobalId(1),
                src: WIN,
            });
        }

        fn fin_regs(level: u16) -> (Reg, Reg) {
            if level == 0 {
                (Reg(11), Reg(12))
            } else {
                (Reg(21), Reg(22))
            }
        }

        /// `return s`: through the innermost pending `finally` (canonical
        /// lowering, LSB §4.3), else directly.
        fn ret(&mut self, s: Reg) {
            let pending = self.open.iter().rev().find_map(|o| match o {
                Open::Fin {
                    fin,
                    level,
                    in_fin: false,
                    ..
                } => Some((*fin, *level)),
                _ => None,
            });
            match pending {
                Some((fin, level)) => {
                    let (kind, val) = Self::fin_regs(level);
                    self.mov(val, s);
                    self.e(Inst::LoadInt {
                        dst: kind,
                        val: 1,
                        ty: IntTy::I8,
                    });
                    let _ = self.f.jmp(fin);
                }
                None => {
                    let _ = self.f.ret(s);
                }
            }
        }

        fn else_(&mut self) {
            let Some(top) = self.open.pop() else { return };
            let top = match top {
                Open::If {
                    else_l,
                    end_l,
                    in_else: false,
                } => {
                    let _ = self.f.jmp(end_l);
                    self.f.bind(else_l);
                    Open::If {
                        else_l,
                        end_l,
                        in_else: true,
                    }
                }
                Open::Try {
                    start,
                    end,
                    h,
                    after,
                    in_catch: false,
                } => {
                    let _ = self.f.jmp(after);
                    self.f.bind(end);
                    self.f.bind(h);
                    Open::Try {
                        start,
                        end,
                        h,
                        after,
                        in_catch: true,
                    }
                }
                Open::Fin {
                    start,
                    end,
                    h,
                    fin,
                    after,
                    level,
                    in_fin: false,
                    over,
                } => {
                    let (kind, val) = Self::fin_regs(level);
                    self.e(Inst::LoadInt {
                        dst: kind,
                        val: 0,
                        ty: IntTy::I8,
                    });
                    let _ = self.f.jmp(fin);
                    self.f.bind(end);
                    self.f.bind(h);
                    self.mov(val, CATCH);
                    self.e(Inst::LoadInt {
                        dst: kind,
                        val: 2,
                        ty: IntTy::I8,
                    });
                    self.f.bind(fin);
                    Open::Fin {
                        start,
                        end,
                        h,
                        fin,
                        after,
                        level,
                        in_fin: true,
                        over,
                    }
                }
                other => other,
            };
            self.open.push(top);
        }

        fn close(&mut self) {
            // Finish the first part of a two-part construct first.
            let needs_else = matches!(
                self.open.last(),
                Some(
                    Open::Try {
                        in_catch: false,
                        ..
                    } | Open::Fin { in_fin: false, .. }
                )
            );
            if needs_else {
                self.else_();
            }
            let Some(top) = self.open.pop() else { return };
            match top {
                Open::If {
                    else_l,
                    end_l,
                    in_else,
                } => {
                    if !in_else {
                        self.f.bind(else_l);
                    }
                    self.f.bind(end_l);
                }
                Open::Loop { top, done, level } => {
                    let ctr = if level == 0 { Reg(8) } else { Reg(19) };
                    self.e(Inst::IAdd {
                        dst: ctr,
                        lhs: ctr,
                        rhs: ONE,
                        op: IntOp::new(IntTy::I64),
                    });
                    let _ = self.f.jmp(top);
                    self.f.bind(done);
                }
                Open::Try {
                    start,
                    end,
                    h,
                    after,
                    ..
                } => {
                    self.f.bind(after);
                    self.f.try_region(start, end, h, CATCH);
                }
                Open::Fin {
                    start,
                    end,
                    h,
                    after,
                    level,
                    over,
                    ..
                } => {
                    let (kind, val) = Self::fin_regs(level);
                    if over {
                        // `return` in `finally` overrides the pending
                        // completion (LSB §4.3 rule 3).
                        let _ = self.f.ret(Reg(2));
                    } else {
                        let (l_ret, l_throw) = (self.f.label(), self.f.label());
                        let _ = self
                            .f
                            .switch(IntTy::I8, kind, &[after, l_ret, l_throw], after);
                        self.f.bind(l_ret);
                        let _ = self.f.ret(val);
                        self.f.bind(l_throw);
                        self.e(Inst::Throw { src: val });
                    }
                    self.f.bind(after);
                    self.f.try_region(start, end, h, CATCH);
                }
            }
        }
    }

    /// Builds the module: five functions `(dyn, dyn) -> dyn` (0 is the
    /// entry) and a closure body with two captures, each from its op list.
    pub(super) fn build(bodies: &[Vec<(Op, bool)>; 6]) -> Program {
        let mut m = ModuleBuilder::new();
        let at = m.add_type(TypeDef::Array(ValType::Dyn));
        let mt = m.add_type(TypeDef::Map {
            key: ValType::Dyn,
            value: ValType::Dyn,
        });
        let (sa, sb, sname) = (m.string("a"), m.string("b"), m.string("S"));
        let st = m.add_type(TypeDef::Struct(StructDef {
            name: sname,
            fields: vec![
                Field {
                    name: sa,
                    ty: ValType::Dyn,
                },
                Field {
                    name: sb,
                    ty: ValType::Dyn,
                },
            ],
            ..Default::default()
        }));
        let strs = [
            m.constant(Const::Bytes(b"a".to_vec())),
            m.constant(Const::Bytes(b"bc".to_vec())),
            m.constant(Const::Bytes(Vec::new())),
        ];
        let _g0 = m.global("g0", ValType::Dyn, true, None);
        let _g1 = m.global("g1", ValType::Dyn, true, None);
        let d = ValType::Dyn;
        let names: Vec<StrId> = NAMES.iter().map(|n| m.string(n)).collect();
        let (sq, sr) = (m.string("q"), m.string("r"));
        let mut builders: Vec<FunctionBuilder> = (0..6)
            .map(|i| m.function(&format!("f{i}"), &[d, d], &[d]))
            .collect();
        // Every call shape the generator uses: positional, named (known,
        // unknown, a positional-only parameter's name), spreads, mixes.
        let shape_kinds: Vec<Vec<ArgKind>> = {
            use ArgKind::{Named, Positional as P, Spread, SpreadNamed};
            let (a, b, x, p) = (names[0], names[1], names[2], names[3]);
            vec![
                vec![P, P],
                vec![P, Named(b)],
                vec![Named(b), Named(a)],
                vec![Spread],
                vec![P, SpreadNamed],
                vec![P, P, P, Named(sq)],
                vec![Named(p)],
                vec![Spread, Named(sr)],
                vec![P, Named(x), Named(sr)],
                vec![Named(sq), Named(a)],
                vec![],
                vec![P, P, P, P],
            ]
        };
        let closure_fn = builders[5].id();
        for (i, fb) in builders.iter_mut().enumerate() {
            if i == 5 {
                let _ = fb.capture(d);
                let _ = fb.capture(d);
            }
            for &t in &regs()[2..] {
                let _ = fb.reg(t);
            }
            let (arr, map, sty) = (fb.type_ref(at), fb.type_ref(mt), fb.type_ref(st));
            let fnames = [
                fb.name_ref(names[0]),
                fb.name_ref(names[1]),
                fb.name_ref(names[2]),
                fb.name_ref(names[3]),
            ];
            let shapes = shape_kinds
                .iter()
                .map(|k| (fb.call_shape(k), k.clone()))
                .collect();
            let _ = fb.emit(Inst::LoadInt {
                dst: ONE,
                val: 1,
                ty: IntTy::I64,
            });
            if i == 5 {
                let _ = fb.emit(Inst::GetUpval {
                    dst: Reg(2),
                    idx: bytecode_lang::UpvalIdx(0),
                });
                let _ = fb.emit(Inst::GetUpval {
                    dst: Reg(3),
                    idx: bytecode_lang::UpvalIdx(1),
                });
            }
            // Role registers start as an array, a map, and (in the entry
            // function) two coroutines, so most operations reach their
            // interesting cases rather than `NullReference`.
            let _ = fb.emit(Inst::LoadInt {
                dst: SCRATCH,
                val: 0,
                ty: IntTy::I64,
            });
            let _ = fb.emit(Inst::NewArray {
                dst: Reg(4),
                len: SCRATCH,
                ty: arr,
            });
            let _ = fb.emit(Inst::NewMap {
                dst: Reg(5),
                ty: map,
            });
            // Seed them with nested containers: r4 = [r0, r1, [r0]],
            // r5 = [0 => r1, 'a' => [0 => r0]], so separation, references,
            // and copy-on-write meet shared inner containers from the start.
            let _ = fb.emit(Inst::ArrayPush {
                arr: Reg(4),
                src: Reg(0),
            });
            let _ = fb.emit(Inst::ArrayPush {
                arr: Reg(4),
                src: Reg(1),
            });
            let _ = fb.emit(Inst::NewArray {
                dst: Reg(2),
                len: SCRATCH,
                ty: arr,
            });
            let _ = fb.emit(Inst::ArrayPush {
                arr: Reg(2),
                src: Reg(0),
            });
            let _ = fb.emit(Inst::ArrayPush {
                arr: Reg(4),
                src: Reg(2),
            });
            let _ = fb.emit(Inst::MapPush {
                map: Reg(5),
                src: Reg(1),
            });
            let _ = fb.emit(Inst::NewMap {
                dst: Reg(3),
                ty: map,
            });
            let _ = fb.emit(Inst::MapPush {
                map: Reg(3),
                src: Reg(0),
            });
            let _ = fb.emit(Inst::DLoadConst {
                dst: Reg(2),
                k: strs[0],
            });
            let _ = fb.emit(Inst::MapSet {
                map: Reg(5),
                key: Reg(2),
                src: Reg(3),
            });
            let _ = fb.emit(Inst::LoadNil { dst: Reg(2) });
            let _ = fb.emit(Inst::LoadNil { dst: Reg(3) });
            if i == 0 {
                for (r, body) in [(6u16, 1u32), (7, 2)] {
                    let _ = fb.mov(A0, Reg(0));
                    let _ = fb.mov(A1, Reg(1));
                    let _ = fb.emit(Inst::CoroNew {
                        dst: WIN,
                        func: FuncId(body),
                        argc: 2,
                    });
                    let _ = fb.mov(Reg(r), WIN);
                }
            }
            let mut g = Gen {
                f: fb,
                arr,
                map,
                st: sty,
                strs,
                closure_fn,
                open: Vec::new(),
                names: fnames,
                shapes,
            };
            for (op, guard) in &bodies[i] {
                g.guarded(op, *guard);
            }
            while !g.open.is_empty() {
                g.close();
            }
            if i == 0 {
                g.epilogue();
            }
            let _ = g.f.ret(Reg(2));
        }
        for fb in builders {
            let _ = m.add_function(fb).expect("generated functions build");
        }
        add_param_list_functions(&mut m, at, &names, (sq, sr));
        Program::load(m.finish().expect("module builds"), &Host::new())
            .expect("generated programs load")
    }

    /// f6 and f7, fixed bodies with parameter lists (LSB §5.15):
    ///
    /// - f6 is PHP's `function f6($a, &$b = ?, ...$rest)`: when `$b` was
    ///   passed (presence bit 1) it increments it through the reference,
    ///   then returns `[$a, $b, $rest, mask]`.
    /// - f7 is Python's `def f7(p, /, q, *, r=?, **kw)`, returning
    ///   `[p, q, r, kw, mask]`.
    fn add_param_list_functions(
        m: &mut ModuleBuilder,
        at: bytecode_lang::TypeId,
        names: &[StrId],
        (sq, sr): (StrId, StrId),
    ) {
        let d = ValType::Dyn;
        let (a, b, p) = (names[0], names[1], names[3]);
        let mut f6 = m.function("f6", &[d, d, d, ValType::I64], &[d]);
        f6.set_params(ParamList::new(vec![
            Param::normal(a),
            Param::normal(b).by_ref().with_default(),
            Param::new(ParamKind::RestMap, None),
        ]));
        let (k, t, c, v, one, out) = (
            f6.reg(ValType::I64),
            f6.reg(ValType::I64),
            f6.reg(ValType::Bool),
            f6.reg(d),
            f6.reg(d),
            f6.reg(d),
        );
        let arr = f6.type_ref(at);
        let skip = f6.label();
        let _ = f6.emit(Inst::LoadInt {
            dst: k,
            val: 2,
            ty: IntTy::I64,
        });
        let _ = f6.emit(Inst::IAnd {
            dst: t,
            lhs: Reg(3),
            rhs: k,
            op: IntOp::new(IntTy::I64),
        });
        let _ = f6.emit(Inst::LoadInt {
            dst: k,
            val: 0,
            ty: IntTy::I64,
        });
        let _ = f6.emit(Inst::IEq {
            dst: c,
            lhs: t,
            rhs: k,
            ty: IntTy::I64,
        });
        let _ = f6.jmp_if(c, skip);
        let _ = f6.emit(Inst::CellGet {
            dst: v,
            cell: Reg(1),
        });
        let _ = f6.emit(Inst::DLoadInt { dst: one, val: 1 });
        let _ = f6.emit(Inst::DAdd {
            dst: v,
            lhs: v,
            rhs: one,
            pol: Policy::new(),
        });
        let _ = f6.emit(Inst::CellSet {
            cell: Reg(1),
            src: v,
        });
        f6.bind(skip);
        let _ = f6.emit(Inst::NewArray {
            dst: out,
            len: k,
            ty: arr,
        });
        for r in 0..3 {
            let _ = f6.emit(Inst::ArrayPush {
                arr: out,
                src: Reg(r),
            });
        }
        let _ = f6.emit(Inst::ToDyn {
            dst: v,
            src: Reg(3),
            from: Prim::I64,
        });
        let _ = f6.emit(Inst::ArrayPush { arr: out, src: v });
        let _ = f6.ret(out);
        let _ = m.add_function(f6).expect("f6 builds");

        let mut f7 = m.function("f7", &[d, d, d, d, ValType::I64], &[d]);
        f7.set_params(ParamList::new(vec![
            Param::new(ParamKind::PositionalOnly, Some(p)),
            Param::normal(sq),
            Param::new(ParamKind::NamedOnly, Some(sr)).with_default(),
            Param::new(ParamKind::RestNamed, None),
        ]));
        let (k, v, out) = (f7.reg(ValType::I64), f7.reg(d), f7.reg(d));
        let arr = f7.type_ref(at);
        let _ = f7.emit(Inst::LoadInt {
            dst: k,
            val: 0,
            ty: IntTy::I64,
        });
        let _ = f7.emit(Inst::NewArray {
            dst: out,
            len: k,
            ty: arr,
        });
        for r in 0..4 {
            let _ = f7.emit(Inst::ArrayPush {
                arr: out,
                src: Reg(r),
            });
        }
        let _ = f7.emit(Inst::ToDyn {
            dst: v,
            src: Reg(4),
            from: Prim::I64,
        });
        let _ = f7.emit(Inst::ArrayPush { arr: out, src: v });
        let _ = f7.ret(out);
        let _ = m.add_function(f7).expect("f7 builds");
    }

    pub(super) fn vm_shape(vm: &Vm<'_>, v: Value, depth: usize) -> Shape {
        if depth == 0 {
            return Shape::Deep;
        }
        match v {
            Value::Nil => Shape::Nil,
            Value::Bool(b) => Shape::Bool(b),
            Value::Int(i) => Shape::Int(i),
            Value::UInt(u) => Shape::Int(u as i64),
            Value::Float(f) => Shape::Float(float_bits(f)),
            Value::Obj(_) => match vm.kind(v) {
                Kind::Str => Shape::Str(vm.str_bytes(v).unwrap_or_default().to_vec()),
                Kind::Array => Shape::Arr(
                    vm.elements(v)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|x| vm_shape(vm, x, depth - 1))
                        .collect(),
                ),
                Kind::Map => Shape::Map(
                    vm.entries(v)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(k, x)| (vm_shape(vm, k, depth - 1), vm_shape(vm, x, depth - 1)))
                        .collect(),
                ),
                Kind::Object => Shape::Struct(
                    (0..)
                        .map_while(|i| vm.field(v, i))
                        .map(|x| vm_shape(vm, x, depth - 1))
                        .collect(),
                ),
                Kind::Function => Shape::Func,
                Kind::Error => Shape::Err(
                    vm.error_code(v).unwrap_or(0),
                    Box::new(vm_shape(
                        vm,
                        vm.error_payload(v).unwrap_or(Value::Nil),
                        depth - 1,
                    )),
                ),
                Kind::Reference => Shape::Ref(Box::new(vm_shape(
                    vm,
                    vm.ref_value(v).unwrap_or(Value::Nil),
                    depth - 1,
                ))),
                Kind::Iter => Shape::Iter,
                Kind::Coroutine => {
                    Shape::Coro(vm.coro_state(v).map_or(255, bytecode_lang::CoroState::code))
                }
                Kind::Nil => Shape::Nil,
                other => panic!("unexpected kind {other:?}"),
            },
            other => panic!("unexpected value {other:?}"),
        }
    }

    pub(super) type WRun = (End, u64, Vec<Shape>);

    /// The VM's outcome, or `None` when it collected (when dropped
    /// coroutines are closed is the VM's own timing; the reference never
    /// collects).
    pub(super) fn vm_run(p: &Program, args: (i32, i32), fuel: u64, depth: usize) -> Option<WRun> {
        let mut vm = Vm::new(p);
        let limits = Limits::new().with_fuel(fuel).with_depth(depth);
        let argv = [Value::Int(args.0.into()), Value::Int(args.1.into())];
        let out = vm.run_with(FuncId(0), &argv, limits);
        if vm.collections() > 0 {
            return None;
        }
        let end = match out {
            Ok(v) => End::Ret(vm_shape(&vm, v, full::SHAPE_DEPTH)),
            Err(VmError::Raised {
                kind,
                payload,
                func,
                pc,
            }) => End::Raised(kind, func.0, pc, vm_shape(&vm, payload, full::SHAPE_DEPTH)),
            Err(VmError::Thrown { value, func, pc }) => {
                End::Thrown(vm_shape(&vm, value, full::SHAPE_DEPTH), func.0, pc)
            }
            Err(VmError::Trap { kind, func, pc }) => End::Trapped(kind, func.0, pc),
            Err(e) => panic!("unexpected {e}"),
        };
        let globals = (0..2)
            .map(|i| {
                let g = vm.global(GlobalId(i)).unwrap_or(Value::Nil);
                vm_shape(&vm, g, full::SHAPE_DEPTH)
            })
            .collect();
        Some((end, vm.fuel_used(), globals))
    }

    pub(super) fn ref_run(p: &Program, args: (i32, i32), fuel: u64, depth: usize) -> WRun {
        full::run(
            p.module(),
            FuncId(0),
            &[V::Int(args.0.into()), V::Int(args.1.into())],
            fuel,
            depth,
        )
    }
}

fn bodies() -> impl Strategy<Value = [Vec<(whole::Op, bool)>; 6]> {
    let one = || proptest::collection::vec(whole::op(), 0..24);
    (one(), one(), one(), one(), one(), one()).prop_map(|(a, b, c, d, e, f)| [a, b, c, d, e, f])
}

/// PHP-style container programs: a long entry function of container,
/// reference, separation, and dynamic-call ops, short helpers.
fn php_bodies() -> impl Strategy<Value = [Vec<(whole::Op, bool)>; 6]> {
    let short = || proptest::collection::vec(whole::php_op(), 0..6);
    (
        proptest::collection::vec(whole::php_op(), 8..64),
        short(),
        short(),
        short(),
        short(),
        short(),
    )
        .prop_map(|(a, b, c, d, e, f)| [a, b, c, d, e, f])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    /// Random multi-function programs with calls (direct, indirect, dynamic,
    /// tail), closures, arrays, maps, structs, strings, try/catch,
    /// try/finally (including `return` in `finally` overriding a pending
    /// completion), and coroutines (generators, `resume`/`resume_throw`/
    /// `coro_close`, keys, iteration): the VM and the reference agree on the
    /// outcome (value, error kind and location, trap), the fuel used, and the
    /// globals, under random fuel and call-depth limits.
    /// PHP-style programs (LSB format 2): nested containers shared by
    /// `dup` and copy-on-write, separation, references in slots, struct
    /// fields and map entries (`dref_*`, `dbind_*`, `dunref_*`, transparent
    /// reads and writes), and dynamic calls binding positional, named, and
    /// spread arguments to parameter lists with by-reference parameters,
    /// defaults, and rest collections: the VM and the reference agree on
    /// the final state, the outcome, and the fuel used.
    #[test]
    fn prop_php_programs_match_the_reference(
        b in php_bodies(),
        args in (-3i32..5, -3i32..5),
        fuel in 200u64..3_000,
    ) {
        let p = whole::build(&b);
        if let Some(vm) = whole::vm_run(&p, args, fuel, 40) {
            let r = whole::ref_run(&p, args, fuel, 40);
            prop_assert_eq!(vm, r);
        }
    }

    #[test]
    fn prop_whole_programs_match_the_reference(
        b in bodies(),
        args in (-3i32..5, -3i32..5),
        fuel in 0u64..3_000,
        depth in 2usize..40,
    ) {
        let p = whole::build(&b);
        if let Some(vm) = whole::vm_run(&p, args, fuel, depth) {
            let r = whole::ref_run(&p, args, fuel, depth);
            prop_assert_eq!(vm, r);
        }
    }
}
