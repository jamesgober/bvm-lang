//! Differential property tests: random straight-line programs and random
//! control-flow graphs (loops included, terminated by fuel) run on the VM
//! and on the reference interpreter in `common/reference.rs`; results,
//! error kinds and pcs, traps, and fuel consumption must agree exactly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

mod common;

use bvm_lang::{Host, Limits, Program, Value, Vm, VmError};
use bytecode_lang::{
    DivZero, FloatToInt, FloatTy, FuncId, Inst, IntConv, IntOp, IntTy, ModuleBuilder, Overflow,
    Policy, Prim, Reg, Shift, Target, ValType,
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
    (0u8..3, any::<bool>(), any::<bool>(), any::<bool>()).prop_map(|(o, d, s, f)| {
        Policy::new()
            .with_overflow([Overflow::Error, Overflow::Wrap, Overflow::Trap][usize::from(o)])
            .with_div_zero(if d { DivZero::Trap } else { DivZero::Error })
            .with_shift(if s { Shift::Mask } else { Shift::Error })
            .with_float_to_int(if f {
                FloatToInt::Saturate
            } else {
                FloatToInt::Error
            })
    })
}

fn dyn_policy() -> impl Strategy<Value = Policy> {
    (0u8..4, any::<bool>(), any::<bool>()).prop_map(|(o, d, s)| {
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
            .with_shift(if s { Shift::Mask } else { Shift::Error })
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
    let int_bin = (0usize..14, 0usize..3, policy()).prop_flat_map(|(which, width, p)| {
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
                _ => Inst::IMax { dst, lhs, rhs, op },
            }
        })
    });
    let int_un = (0usize..3, policy(), reg(&I64S), reg(&I64S)).prop_map(|(w, p, dst, src)| {
        let op = IntOp::new(IntTy::I64).with_policy(p);
        match w {
            0 => Inst::INeg { dst, src, op },
            1 => Inst::INot { dst, src, op },
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
            op: IntOp::new(IntTy::I64).with_policy(p)
        }),
        (reg(&I32S), reg(&F64S), policy()).prop_map(|(dst, src, p)| Inst::F64ToInt {
            dst,
            src,
            op: IntOp::new(IntTy::I32).with_policy(p)
        }),
    ];
    let floats = (0usize..5, reg(&F64S), reg(&F64S), reg(&F64S), reg(&BOOLS)).prop_map(
        |(w, dst, lhs, rhs, b)| {
            let ty = FloatTy::F64;
            match w {
                0 => Inst::FAdd { dst, lhs, rhs, ty },
                1 => Inst::FSub { dst, lhs, rhs, ty },
                2 => Inst::FMul { dst, lhs, rhs, ty },
                3 => Inst::FDiv { dst, lhs, rhs, ty },
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
        (0usize..7, dyn_policy(), reg(&DYNS), reg(&DYNS), reg(&DYNS)).prop_map(
            |(w, pol, dst, lhs, rhs)| match w {
                0 => Inst::DAdd { dst, lhs, rhs, pol },
                1 => Inst::DSub { dst, lhs, rhs, pol },
                2 => Inst::DMul { dst, lhs, rhs, pol },
                3 => Inst::DDiv { dst, lhs, rhs, pol },
                4 => Inst::DRem { dst, lhs, rhs, pol },
                5 => Inst::DFloorDiv { dst, lhs, rhs, pol },
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
