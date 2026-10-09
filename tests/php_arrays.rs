//! PHP array semantics of LSB maps (LSB §2.4, §5.10): insertion order,
//! in-place update, the next-integer-key rule, deletion, iteration that
//! skips deleted and visits appended entries, kind-sensitive keys, and value
//! semantics through `dup`. Checked against a reference model.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::type_complexity
)]

mod common;

use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{
    Const, ErrorKind, FuncId, FunctionBuilder, Inst, IntTy, ModuleBuilder, Reg, TypeDef, ValType,
};
use common::{BOOL, D, I64};
use proptest::prelude::*;

/// A dyn program: `main() -> dyn` building a map with `ops`.
#[derive(Clone, Debug)]
enum Op {
    /// `$a[k] = v`
    Set(i32, i32),
    /// `$a[] = v`
    Push(i32),
    /// `unset($a[k])`
    Del(i32),
}

fn run_ops(ops: &[Op]) -> (Program, Vec<(Value, Value)>) {
    let ops = ops.to_vec();
    let p = common::load(common::module(&[], &[D], move |m, f| {
        let mt = m.add_type(TypeDef::Map { key: D, value: D });
        let tr = f.type_ref(mt);
        let (map, k, v) = (f.reg(D), f.reg(D), f.reg(D));
        f.emit(Inst::NewMap { dst: map, ty: tr });
        for op in &ops {
            match *op {
                Op::Set(key, val) => {
                    f.emit(Inst::DLoadInt { dst: k, val: key });
                    f.emit(Inst::DLoadInt { dst: v, val });
                    f.emit(Inst::MapSet {
                        map,
                        key: k,
                        src: v,
                    });
                }
                Op::Push(val) => {
                    f.emit(Inst::DLoadInt { dst: v, val });
                    f.emit(Inst::MapPush { map, src: v });
                }
                Op::Del(key) => {
                    f.emit(Inst::DLoadInt { dst: k, val: key });
                    f.emit(Inst::MapDel { map, key: k });
                }
            }
        }
        f.ret(map);
    }));
    let entries = {
        let mut vm = Vm::new(&p);
        let out = vm.run(FuncId(0), &[]).unwrap();
        vm.entries(out).unwrap()
    };
    (p, entries)
}

/// The reference model: PHP 8.3 array semantics for int keys.
fn model(ops: &[Op]) -> Vec<(i64, i64)> {
    let mut entries: Vec<(i64, i64)> = Vec::new();
    let mut next: Option<i64> = None;
    for op in ops {
        match *op {
            Op::Set(k, v) => {
                let (k, v) = (i64::from(k), i64::from(v));
                match entries.iter_mut().find(|(ek, _)| *ek == k) {
                    Some(e) => e.1 = v,
                    None => entries.push((k, v)),
                }
                next = Some(next.map_or(k + 1, |n| n.max(k + 1)));
            }
            Op::Push(v) => {
                let k = next.unwrap_or(0);
                entries.push((k, i64::from(v)));
                next = Some(k + 1);
            }
            Op::Del(k) => entries.retain(|(ek, _)| *ek != i64::from(k)),
        }
    }
    entries
}

fn ints(entries: &[(Value, Value)]) -> Vec<(i64, i64)> {
    entries
        .iter()
        .map(|(k, v)| {
            (
                k.as_int().unwrap_or(i64::MIN),
                v.as_int().unwrap_or(i64::MIN),
            )
        })
        .collect()
}

#[test]
fn push_after_explicit_keys_uses_largest_plus_one() {
    let ops = [Op::Set(5, 1), Op::Push(2), Op::Set(3, 3), Op::Push(4)];
    let (_, e) = run_ops(&ops);
    assert_eq!(ints(&e), vec![(5, 1), (6, 2), (3, 3), (7, 4)]);
}

#[test]
fn deleting_never_lowers_the_next_key() {
    let ops = [
        Op::Push(1),
        Op::Push(2),
        Op::Del(1),
        Op::Del(0),
        Op::Push(3),
    ];
    let (_, e) = run_ops(&ops);
    assert_eq!(ints(&e), vec![(2, 3)]);
}

#[test]
fn negative_keys_follow_php_8_3() {
    let ops = [Op::Set(-5, 1), Op::Push(2)];
    let (_, e) = run_ops(&ops);
    assert_eq!(ints(&e), vec![(-5, 1), (-4, 2)]);
}

#[test]
fn updating_keeps_the_position() {
    let ops = [Op::Set(1, 1), Op::Set(2, 2), Op::Set(1, 9)];
    let (_, e) = run_ops(&ops);
    assert_eq!(ints(&e), vec![(1, 9), (2, 2)]);
}

#[test]
fn reinserting_a_deleted_key_appends_it() {
    let ops = [Op::Set(1, 1), Op::Set(2, 2), Op::Del(1), Op::Set(1, 3)];
    let (_, e) = run_ops(&ops);
    assert_eq!(ints(&e), vec![(2, 2), (1, 3)]);
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (-20i32..20, any::<i32>()).prop_map(|(k, v)| Op::Set(k, v)),
        any::<i32>().prop_map(Op::Push),
        (-20i32..20).prop_map(Op::Del),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every sequence of sets, pushes, and deletes leaves the map equal to
    /// the PHP reference model, order included (this exercises compaction:
    /// sequences delete freely).
    #[test]
    fn prop_maps_match_the_php_model(ops in proptest::collection::vec(op_strategy(), 0..200)) {
        let (_, e) = run_ops(&ops);
        prop_assert_eq!(ints(&e), model(&ops));
    }
}

#[test]
fn keys_of_different_kinds_are_different() {
    // $a[1] = "int"; $a[1.0] = "float"; $a["1"] = "str"; three entries.
    let p = common::load(common::module(&[], &[D], |m, f| {
        let mt = m.add_type(TypeDef::Map { key: D, value: D });
        let fk = m.constant(Const::f64(1.0));
        let sk = m.constant(Const::Bytes(b"1".to_vec()));
        let tr = f.type_ref(mt);
        let (map, k, v) = (f.reg(D), f.reg(D), f.reg(D));
        f.emit(Inst::NewMap { dst: map, ty: tr });
        f.emit(Inst::DLoadInt { dst: k, val: 1 });
        f.emit(Inst::DLoadInt { dst: v, val: 10 });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        f.emit(Inst::DLoadConst { dst: k, k: fk });
        f.emit(Inst::DLoadInt { dst: v, val: 20 });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        f.emit(Inst::DLoadConst { dst: k, k: sk });
        f.emit(Inst::DLoadInt { dst: v, val: 30 });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        f.ret(map);
    }));
    let mut vm = Vm::new(&p);
    let out = vm.run(FuncId(0), &[]).unwrap();
    let e = vm.entries(out).unwrap();
    assert_eq!(e.len(), 3);
    assert_eq!(e[0], (Value::Int(1), Value::Int(10)));
    assert_eq!(e[1], (Value::Float(1.0), Value::Int(20)));
    assert_eq!(vm.str_bytes(e[2].0), Some(&b"1"[..]));
}

#[test]
fn signed_zero_keys_are_one_key_and_strings_compare_by_bytes() {
    let p = common::load(common::module(&[], &[D], |m, f| {
        let mt = m.add_type(TypeDef::Map { key: D, value: D });
        let pz = m.constant(Const::f64(0.0));
        let nz = m.constant(Const::f64(-0.0));
        let nan1 = m.constant(Const::f64(f64::NAN));
        let nan2 = m.constant(Const::F64(0xFFF8_0000_0000_0001));
        let a = m.constant(Const::Bytes(b"ab".to_vec()));
        let a1 = m.constant(Const::Bytes(b"a".to_vec()));
        let b1 = m.constant(Const::Bytes(b"b".to_vec()));
        let tr = f.type_ref(mt);
        let (map, k, v, x, y) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D), f.reg(D));
        f.emit(Inst::NewMap { dst: map, ty: tr });
        f.emit(Inst::DLoadInt { dst: v, val: 1 });
        f.emit(Inst::DLoadConst { dst: k, k: pz });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        f.emit(Inst::DLoadConst { dst: k, k: nz });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        f.emit(Inst::DLoadConst { dst: k, k: nan1 });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        f.emit(Inst::DLoadConst { dst: k, k: nan2 });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        f.emit(Inst::DLoadConst { dst: k, k: a });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        // A different string object with the same bytes is the same key.
        f.emit(Inst::DLoadConst { dst: x, k: a1 });
        f.emit(Inst::DLoadConst { dst: y, k: b1 });
        f.emit(Inst::DConcat {
            dst: k,
            lhs: x,
            rhs: y,
        });
        f.emit(Inst::DLoadInt { dst: v, val: 2 });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: v,
        });
        f.ret(map);
    }));
    let mut vm = Vm::new(&p);
    let out = vm.run(FuncId(0), &[]).unwrap();
    let e = vm.entries(out).unwrap();
    assert_eq!(e.len(), 3, "{e:?}");
    assert_eq!(e[0].0, Value::Float(0.0));
    assert!(e[1].0.as_float().unwrap().is_nan());
    assert_eq!(e[2].1, Value::Int(2));
}

#[test]
fn iteration_skips_deleted_and_visits_appended_entries() {
    // foreach over $a while deleting the next key and appending: the
    // iterator follows the live map.
    let p = common::load(common::module(&[], &[D], |m, f| {
        let mt = m.add_type(TypeDef::Map { key: D, value: D });
        let tr = f.type_ref(mt);
        let (map, k, v, it, has, acc, ten, key) = (
            f.reg(D),
            f.reg(D),
            f.reg(D),
            f.reg(D),
            f.reg(BOOL),
            f.reg(D),
            f.reg(D),
            f.reg(D),
        );
        f.emit(Inst::NewMap { dst: map, ty: tr });
        for i in 0..3 {
            f.emit(Inst::DLoadInt { dst: v, val: i });
            f.emit(Inst::MapPush { map, src: v }); // {0:0, 1:1, 2:2}
        }
        f.emit(Inst::DIterNew { dst: it, src: map });
        f.emit(Inst::DLoadInt { dst: acc, val: 0 });
        f.emit(Inst::DLoadInt { dst: ten, val: 10 });
        let (top, done, skip) = (f.label(), f.label(), f.label());
        f.bind(top);
        f.emit(Inst::IterNext {
            has,
            iter: it,
            val: v,
        });
        f.jmp_if_not(has, done);
        f.emit(Inst::IterKey { dst: key, iter: it });
        // acc = acc * 10 + v
        f.emit(Inst::DMul {
            dst: acc,
            lhs: acc,
            rhs: ten,
            pol: bytecode_lang::Policy::new(),
        });
        f.emit(Inst::DAdd {
            dst: acc,
            lhs: acc,
            rhs: v,
            pol: bytecode_lang::Policy::new(),
        });
        // On key 0: delete key 1 and push 7.
        let is_zero = f.reg(BOOL);
        let zero = f.reg(D);
        f.emit(Inst::DLoadInt { dst: zero, val: 0 });
        f.emit(Inst::DEq {
            dst: is_zero,
            lhs: key,
            rhs: zero,
        });
        f.jmp_if_not(is_zero, skip);
        f.emit(Inst::DLoadInt { dst: k, val: 1 });
        f.emit(Inst::MapDel { map, key: k });
        f.emit(Inst::DLoadInt { dst: k, val: 7 });
        f.emit(Inst::MapPush { map, src: k });
        f.bind(skip);
        f.emit(Inst::Safepoint {});
        f.jmp(top);
        f.bind(done);
        f.ret(acc);
    }));
    // Visits 0, then 2 (1 deleted), then the pushed 7: digits 0, 2, 7.
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(27)));
}

#[test]
fn foreach_over_a_dup_is_value_semantics() {
    // PHP: foreach ($a as $v) { $a[] = $v; } iterates the original only.
    let p = common::load(common::module(&[], &[I64], |m, f| {
        let mt = m.add_type(TypeDef::Map { key: D, value: D });
        let tr = f.type_ref(mt);
        let (map, copy, v, it, has, n) = (
            f.reg(D),
            f.reg(D),
            f.reg(D),
            f.reg(D),
            f.reg(BOOL),
            f.reg(I64),
        );
        f.emit(Inst::NewMap { dst: map, ty: tr });
        for i in 0..3 {
            f.emit(Inst::DLoadInt { dst: v, val: i });
            f.emit(Inst::MapPush { map, src: v });
        }
        f.emit(Inst::Dup {
            dst: copy,
            src: map,
        });
        f.emit(Inst::DIterNew { dst: it, src: copy });
        let (top, done) = (f.label(), f.label());
        f.bind(top);
        f.emit(Inst::IterNext {
            has,
            iter: it,
            val: v,
        });
        f.jmp_if_not(has, done);
        f.emit(Inst::MapPush { map, src: v });
        f.emit(Inst::Safepoint {});
        f.jmp(top);
        f.bind(done);
        f.emit(Inst::MapLen { dst: n, map });
        f.ret(n);
    }));
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(6)));
}

fn push_past(max_key: Const, key: ValType) -> Result<Value, VmError> {
    let mut m = ModuleBuilder::new();
    let mt = m.add_type(TypeDef::Map { key, value: D });
    let k = m.constant(max_key);
    let mut f = m.function("main", &[], &[]);
    let tr = f.type_ref(mt);
    let (map, kr, v) = (f.reg(ValType::Ref(mt)), f.reg(key), f.reg(D));
    f.emit(Inst::NewMap { dst: map, ty: tr });
    if key == D {
        f.emit(Inst::DLoadConst { dst: kr, k });
    } else {
        f.emit(Inst::LoadConst { dst: kr, k });
    }
    f.emit(Inst::MapSet {
        map,
        key: kr,
        src: v,
    });
    f.emit(Inst::MapPush { map, src: v });
    f.ret_void();
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    Vm::new(&p).run(FuncId(0), &[])
}

#[test]
fn map_push_raises_when_the_next_key_overflows() {
    let overflow = Err(VmError::Raised {
        kind: ErrorKind::ArithOverflow,
        payload: bvm_lang::Value::Nil,
        func: FuncId(0),
        pc: 3,
    });
    assert_eq!(push_past(Const::Int(i64::MAX), D), overflow);
    assert_eq!(push_past(Const::Int(i64::MAX), I64), overflow);
    assert_eq!(
        push_past(Const::UInt(255), ValType::U8),
        overflow,
        "next key 256 is not a u8"
    );
    assert_eq!(push_past(Const::UInt(254), ValType::U8), Ok(Value::Nil));
}

#[test]
fn map_push_on_a_non_integer_keyed_map_is_a_type_error() {
    let mut m = ModuleBuilder::new();
    let mt = m.add_type(TypeDef::Map {
        key: ValType::Str,
        value: D,
    });
    let mut f = m.function("main", &[], &[]);
    let tr = f.type_ref(mt);
    let (map, v) = (f.reg(ValType::Ref(mt)), f.reg(D));
    f.emit(Inst::NewMap { dst: map, ty: tr });
    f.emit(Inst::MapPush { map, src: v });
    f.ret_void();
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[]),
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 1
        })
    );
}

#[test]
fn large_maps_stay_correct_through_growth_and_compaction() {
    // 100k inserts, delete every other key, then re-check all.
    let n = 100_000i32;
    let p = common::load(common::module(
        &[],
        &[I64],
        move |m, f: &mut FunctionBuilder| {
            let mt = m.add_type(TypeDef::Map {
                key: I64,
                value: I64,
            });
            let tr = f.type_ref(mt);
            let (map, i, two, lim, one, cond, rem, got, sum) = (
                f.reg(ValType::Ref(mt)),
                f.reg(I64),
                f.reg(I64),
                f.reg(I64),
                f.reg(I64),
                f.reg(BOOL),
                f.reg(I64),
                f.reg(I64),
                f.reg(I64),
            );
            let op = bytecode_lang::IntOp::new(IntTy::I64);
            f.emit(Inst::NewMap { dst: map, ty: tr });
            f.emit(Inst::LoadInt {
                dst: lim,
                val: n,
                ty: IntTy::I64,
            });
            f.emit(Inst::LoadInt {
                dst: one,
                val: 1,
                ty: IntTy::I64,
            });
            f.emit(Inst::LoadInt {
                dst: two,
                val: 2,
                ty: IntTy::I64,
            });
            // for i in 0..n: map[i] = i
            let (l1, e1) = (f.label(), f.label());
            f.bind(l1);
            f.emit(Inst::ILt {
                dst: cond,
                lhs: i,
                rhs: lim,
                ty: IntTy::I64,
            });
            f.jmp_if_not(cond, e1);
            f.emit(Inst::MapSet {
                map,
                key: i,
                src: i,
            });
            f.emit(Inst::IAdd {
                dst: i,
                lhs: i,
                rhs: one,
                op,
            });
            f.jmp(l1);
            f.bind(e1);
            // for i in 0..n step 2: del map[i]
            f.emit(Inst::LoadInt {
                dst: i,
                val: 0,
                ty: IntTy::I64,
            });
            let (l2, e2) = (f.label(), f.label());
            f.bind(l2);
            f.emit(Inst::ILt {
                dst: cond,
                lhs: i,
                rhs: lim,
                ty: IntTy::I64,
            });
            f.jmp_if_not(cond, e2);
            f.emit(Inst::MapDel { map, key: i });
            f.emit(Inst::IAdd {
                dst: i,
                lhs: i,
                rhs: two,
                op,
            });
            f.jmp(l2);
            f.bind(e2);
            // sum of map[i] for odd i (map_get), plus 1 per present even (0).
            f.emit(Inst::LoadInt {
                dst: i,
                val: 0,
                ty: IntTy::I64,
            });
            let (l3, e3, absent, next) = (f.label(), f.label(), f.label(), f.label());
            f.bind(l3);
            f.emit(Inst::ILt {
                dst: cond,
                lhs: i,
                rhs: lim,
                ty: IntTy::I64,
            });
            f.jmp_if_not(cond, e3);
            f.emit(Inst::MapHas {
                dst: cond,
                map,
                key: i,
            });
            f.jmp_if_not(cond, absent);
            f.emit(Inst::MapGet {
                dst: got,
                map,
                key: i,
            });
            f.emit(Inst::IAdd {
                dst: sum,
                lhs: sum,
                rhs: got,
                op,
            });
            f.jmp(next);
            f.bind(absent);
            f.emit(Inst::IRem {
                dst: rem,
                lhs: i,
                rhs: two,
                op,
            });
            f.emit(Inst::IAdd {
                dst: sum,
                lhs: sum,
                rhs: rem,
                op,
            }); // odd absent would add 1
            f.bind(next);
            f.emit(Inst::IAdd {
                dst: i,
                lhs: i,
                rhs: one,
                op,
            });
            f.jmp(l3);
            f.bind(e3);
            f.ret(sum);
            let _ = Reg(0);
        },
    ));
    let odd_sum: i64 = (0..i64::from(n)).filter(|i| i % 2 == 1).sum();
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(odd_sum)));
}
