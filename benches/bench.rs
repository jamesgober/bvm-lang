//! Criterion benchmarks at realistic sizes.
//!
//! - `dispatch/*`: tight loops measuring the per-instruction cost of the
//!   dispatch loop, typed (`i*`) and dynamic (`d*`). `loop5` is the same
//!   five-instruction loop shape as bvm-lang 1.0's `loop_sum` benchmark
//!   (compare, branch, add, add, jump), so ns/instruction compares
//!   directly; `loop6` adds the `safepoint` a verified LSB loop carries.
//! - `call/fib`: call-heavy recursion (`fib(25)`, ~243k calls).
//! - `map/*`: PHP-array work: 100k integer-key inserts then 100k lookups,
//!   and 20k string-key inserts and lookups.
//! - `string/*`: 100k concatenations and comparisons.
//! - `gc/*`: 1M short-lived allocations under a 16 MiB budget, with a live
//!   set of 10k objects kept in a map.
//!
//! Run with `cargo bench`.

use std::hint::black_box;

use bvm_lang::{Host, Limits, Program, Value, Vm};
use bytecode_lang::{
    Const, FuncId, FunctionBuilder, Inst, IntOp, IntTy, ModuleBuilder, Policy, Reg, TypeDef,
    ValType,
};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

const I64: ValType = ValType::I64;
const D: ValType = ValType::Dyn;
const BOOL: ValType = ValType::Bool;

fn op() -> IntOp {
    IntOp::new(IntTy::I64)
}

fn load(m: ModuleBuilder) -> Program {
    Program::load(m.finish().expect("module builds"), &Host::new()).expect("module loads")
}

/// `sum = 0; i = 0; while i <= n { sum += i; i += 1 }` with typed
/// registers, optionally with a safepoint per iteration.
fn typed_loop(safepoint: bool) -> Program {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("sum", &[I64], &[I64]);
    let (sum, i, one, c) = (f.reg(I64), f.reg(I64), f.reg(I64), f.reg(BOOL));
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::ILe {
        dst: c,
        lhs: i,
        rhs: Reg(0),
        ty: IntTy::I64,
    });
    f.jmp_if_not(c, done);
    f.emit(Inst::IAdd {
        dst: sum,
        lhs: sum,
        rhs: i,
        op: op(),
    });
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: op(),
    });
    if safepoint {
        f.emit(Inst::Safepoint {});
    }
    f.jmp(top);
    f.bind(done);
    f.ret(sum);
    m.add_function(f).expect("builds");
    load(m)
}

/// The same loop over `dyn` registers (the Mox shape).
fn dyn_loop(safepoint: bool) -> Program {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("sum", &[D], &[D]);
    let (sum, i, one, c) = (f.reg(D), f.reg(D), f.reg(D), f.reg(BOOL));
    let p = Policy::new();
    f.emit(Inst::DLoadInt { dst: sum, val: 0 });
    f.emit(Inst::DLoadInt { dst: i, val: 0 });
    f.emit(Inst::DLoadInt { dst: one, val: 1 });
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::DLe {
        dst: c,
        lhs: i,
        rhs: Reg(0),
    });
    f.jmp_if_not(c, done);
    f.emit(Inst::DAdd {
        dst: sum,
        lhs: sum,
        rhs: i,
        pol: p,
    });
    f.emit(Inst::DAdd {
        dst: i,
        lhs: i,
        rhs: one,
        pol: p,
    });
    if safepoint {
        f.emit(Inst::Safepoint {});
    }
    f.jmp(top);
    f.bind(done);
    f.ret(sum);
    m.add_function(f).expect("builds");
    load(m)
}

fn bench_dispatch(c: &mut Criterion) {
    let n = 100_000i64;
    let mut g = c.benchmark_group("dispatch");
    for (name, prog, insts_per_iter, dynamic) in [
        ("typed_loop5", typed_loop(false), 5, false),
        ("typed_loop6", typed_loop(true), 6, false),
        ("dyn_loop5", dyn_loop(false), 5, true),
        ("dyn_loop6", dyn_loop(true), 6, true),
    ] {
        let mut vm = Vm::new(&prog);
        g.throughput(Throughput::Elements(
            u64::try_from(n).unwrap_or(0) * insts_per_iter,
        ));
        g.bench_function(name, |b| {
            b.iter(|| {
                let arg = if dynamic {
                    Value::Int(n)
                } else {
                    Value::Int(black_box(n))
                };
                black_box(vm.run(FuncId(0), &[arg]).expect("runs"))
            });
        });
    }
    g.finish();
}

fn fib_program() -> Program {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("fib", &[I64], &[I64]);
    let me = f.id();
    let (two, c, a, b) = (f.reg(I64), f.reg(BOOL), f.reg(I64), f.reg(I64));
    let w = f.regs(&[I64, I64]);
    let small = f.label();
    f.emit(Inst::LoadInt {
        dst: two,
        val: 2,
        ty: IntTy::I64,
    });
    f.emit(Inst::ILt {
        dst: c,
        lhs: Reg(0),
        rhs: two,
        ty: IntTy::I64,
    });
    f.jmp_if(c, small);
    f.emit(Inst::LoadInt {
        dst: a,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::ISub {
        dst: Reg(w.0 + 1),
        lhs: Reg(0),
        rhs: a,
        op: op(),
    });
    f.emit(Inst::Call {
        dst: w,
        func: me,
        argc: 1,
    });
    f.emit(Inst::Mov { dst: a, src: w });
    f.emit(Inst::ISub {
        dst: Reg(w.0 + 1),
        lhs: Reg(0),
        rhs: two,
        op: op(),
    });
    f.emit(Inst::Call {
        dst: w,
        func: me,
        argc: 1,
    });
    f.emit(Inst::IAdd {
        dst: b,
        lhs: a,
        rhs: w,
        op: op(),
    });
    f.ret(b);
    f.bind(small);
    f.ret(Reg(0));
    m.add_function(f).expect("builds");
    load(m)
}

fn bench_calls(c: &mut Criterion) {
    let p = fib_program();
    let mut vm = Vm::new(&p);
    let mut g = c.benchmark_group("call");
    g.sample_size(20);
    // fib(25) makes 242,785 calls.
    g.throughput(Throughput::Elements(242_785));
    g.bench_function("fib25", |b| {
        b.iter(|| {
            black_box(
                vm.run(FuncId(0), &[Value::Int(black_box(25))])
                    .expect("runs"),
            )
        });
    });
    g.finish();
}

/// Emits `for i in 0..n { body(i) }` with a safepoint per iteration.
fn counted(f: &mut FunctionBuilder, n: Reg, body: impl FnOnce(&mut FunctionBuilder, Reg)) {
    let (i, one, c) = (f.reg(I64), f.reg(I64), f.reg(BOOL));
    f.emit(Inst::LoadInt {
        dst: i,
        val: 0,
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
        dst: c,
        lhs: i,
        rhs: n,
        ty: IntTy::I64,
    });
    f.jmp_if_not(c, done);
    body(f, i);
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: op(),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
}

/// `$a = []; for i in 0..n { $a[i] = i } ; for i in 0..n { $s += $a[i] }`
/// over a dyn map with dyn int keys.
fn map_int_program() -> Program {
    let mut m = ModuleBuilder::new();
    let mt = m.add_type(TypeDef::Map { key: D, value: D });
    let mut f = m.function("maps", &[I64], &[D]);
    let tr = f.type_ref(mt);
    let (map, k, v, sum) = (f.reg(D), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::NewMap { dst: map, ty: tr });
    f.emit(Inst::DLoadInt { dst: sum, val: 0 });
    counted(&mut f, Reg(0), |f, i| {
        f.emit(Inst::ToDyn {
            dst: k,
            src: i,
            from: bytecode_lang::Prim::I64,
        });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: k,
        });
    });
    counted(&mut f, Reg(0), |f, i| {
        f.emit(Inst::ToDyn {
            dst: k,
            src: i,
            from: bytecode_lang::Prim::I64,
        });
        f.emit(Inst::MapGet {
            dst: v,
            map,
            key: k,
        });
        f.emit(Inst::DAdd {
            dst: sum,
            lhs: sum,
            rhs: v,
            pol: Policy::new(),
        });
    });
    f.ret(sum);
    m.add_function(f).expect("builds");
    load(m)
}

/// String keys: `$k .= "x"; $a[$k] = $i` (distinct, growing keys), then
/// lookups of every key, over a dyn map.
fn map_str_program() -> Program {
    let mut m = ModuleBuilder::new();
    let mt = m.add_type(TypeDef::Map { key: D, value: D });
    let at = m.add_type(TypeDef::Array(D));
    let prefix = m.constant(Const::Bytes(b"key_".to_vec()));
    let x = m.constant(Const::Bytes(b"x".to_vec()));
    let mut f = m.function("smaps", &[I64], &[D]);
    let (mtr, atr) = (f.type_ref(mt), f.type_ref(at));
    let (map, keys, k, v, sum, xs, idx) = (
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
    );
    let zero = f.reg(I64);
    f.emit(Inst::NewMap { dst: map, ty: mtr });
    f.emit(Inst::NewArray {
        dst: keys,
        len: zero,
        ty: atr,
    });
    f.emit(Inst::DLoadConst { dst: k, k: prefix });
    f.emit(Inst::DLoadConst { dst: xs, k: x });
    f.emit(Inst::DLoadInt { dst: sum, val: 0 });
    counted(&mut f, Reg(0), |f, i| {
        f.emit(Inst::ToDyn {
            dst: idx,
            src: i,
            from: bytecode_lang::Prim::I64,
        });
        f.emit(Inst::DConcat {
            dst: k,
            lhs: k,
            rhs: xs,
        });
        f.emit(Inst::ArrayPush { arr: keys, src: k });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: idx,
        });
    });
    counted(&mut f, Reg(0), |f, i| {
        f.emit(Inst::ArrayGet {
            dst: k,
            arr: keys,
            idx: i,
        });
        f.emit(Inst::MapGet {
            dst: v,
            map,
            key: k,
        });
        f.emit(Inst::DAdd {
            dst: sum,
            lhs: sum,
            rhs: v,
            pol: Policy::new(),
        });
    });
    f.ret(sum);
    m.add_function(f).expect("builds");
    load(m)
}

fn bench_maps(c: &mut Criterion) {
    let mut g = c.benchmark_group("map");
    g.sample_size(20);
    let p = map_int_program();
    let mut vm = Vm::new(&p);
    let n = 100_000i64;
    g.throughput(Throughput::Elements(2 * u64::try_from(n).unwrap_or(0)));
    g.bench_function("int_keys_100k_set_get", |b| {
        b.iter(|| black_box(vm.run(FuncId(0), &[Value::Int(n)]).expect("runs")));
    });
    let p = map_str_program();
    let mut vm = Vm::new(&p);
    let n = 2_000i64;
    g.throughput(Throughput::Elements(2 * u64::try_from(n).unwrap_or(0)));
    g.bench_function("str_keys_2k_growing_set_get", |b| {
        b.iter(|| black_box(vm.run(FuncId(0), &[Value::Int(n)]).expect("runs")));
    });
    g.finish();
}

/// 100k iterations of: concat two short strings, compare with another,
/// take a slice.
fn string_program() -> Program {
    let mut m = ModuleBuilder::new();
    let a = m.constant(Const::Bytes(b"hello, ".to_vec()));
    let b = m.constant(Const::Bytes(b"world".to_vec()));
    let mut f = m.function("strings", &[I64], &[I64]);
    let s = ValType::Str;
    let (x, y, z, w, eq, count, one) = (
        f.reg(s),
        f.reg(s),
        f.reg(s),
        f.reg(s),
        f.reg(BOOL),
        f.reg(I64),
        f.reg(I64),
    );
    let range = f.regs(&[I64, I64]);
    f.emit(Inst::LoadConst { dst: x, k: a });
    f.emit(Inst::LoadConst { dst: y, k: b });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: range,
        val: 2,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: Reg(range.0 + 1),
        val: 9,
        ty: IntTy::I64,
    });
    f.emit(Inst::StrConcat {
        dst: w,
        lhs: x,
        rhs: y,
    });
    counted(&mut f, Reg(0), |f, _| {
        f.emit(Inst::StrConcat {
            dst: z,
            lhs: x,
            rhs: y,
        });
        f.emit(Inst::StrEq {
            dst: eq,
            lhs: z,
            rhs: w,
        });
        f.emit(Inst::StrSlice {
            dst: z,
            s: z,
            range,
            utf8: true,
        });
        let skip = f.label();
        f.jmp_if_not(eq, skip);
        f.emit(Inst::IAdd {
            dst: count,
            lhs: count,
            rhs: one,
            op: op(),
        });
        f.bind(skip);
    });
    f.ret(count);
    m.add_function(f).expect("builds");
    load(m)
}

fn bench_strings(c: &mut Criterion) {
    let p = string_program();
    let mut vm = Vm::new(&p);
    let n = 100_000i64;
    let mut g = c.benchmark_group("string");
    g.sample_size(20);
    g.throughput(Throughput::Elements(u64::try_from(n).unwrap_or(0)));
    g.bench_function("concat_eq_slice_100k", |b| {
        b.iter(|| black_box(vm.run(FuncId(0), &[Value::Int(n)]).expect("runs")));
    });
    g.finish();
}

/// 1M iterations: allocate a 4-element dyn array (garbage), and every
/// 100th iteration store one into a map that keeps 10k live.
fn gc_program() -> Program {
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let mt = m.add_type(TypeDef::Map { key: D, value: D });
    let mut f = m.function("gc", &[I64], &[I64]);
    let (atr, mtr) = (f.type_ref(at), f.type_ref(mt));
    let (arr, map, len, k, hundred, rem, c, live) = (
        f.reg(D),
        f.reg(D),
        f.reg(I64),
        f.reg(D),
        f.reg(I64),
        f.reg(I64),
        f.reg(BOOL),
        f.reg(I64),
    );
    let zero = f.reg(I64);
    f.emit(Inst::LoadInt {
        dst: len,
        val: 4,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: hundred,
        val: 100,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewMap { dst: map, ty: mtr });
    counted(&mut f, Reg(0), |f, i| {
        f.emit(Inst::NewArray {
            dst: arr,
            len,
            ty: atr,
        });
        f.emit(Inst::IRem {
            dst: rem,
            lhs: i,
            rhs: hundred,
            op: op(),
        });
        f.emit(Inst::IEq {
            dst: c,
            lhs: rem,
            rhs: zero,
            ty: IntTy::I64,
        });
        let skip = f.label();
        f.jmp_if_not(c, skip);
        f.emit(Inst::ToDyn {
            dst: k,
            src: i,
            from: bytecode_lang::Prim::I64,
        });
        f.emit(Inst::MapSet {
            map,
            key: k,
            src: arr,
        });
        f.bind(skip);
    });
    f.emit(Inst::MapLen { dst: live, map });
    f.ret(live);
    m.add_function(f).expect("builds");
    load(m)
}

fn bench_gc(c: &mut Criterion) {
    let p = gc_program();
    let mut vm = Vm::with_limits(&p, Limits::new().with_memory(16 << 20));
    let n = 1_000_000i64;
    let mut g = c.benchmark_group("gc");
    g.sample_size(10);
    g.throughput(Throughput::Elements(u64::try_from(n).unwrap_or(0)));
    g.bench_function("alloc_1m_live_10k", |b| {
        b.iter(|| black_box(vm.run(FuncId(0), &[Value::Int(n)]).expect("runs")));
    });
    g.finish();
}

fn bench_load(c: &mut Criterion) {
    // Loading cost: a module with 1,000 functions of 100 instructions.
    let mut m = ModuleBuilder::new();
    for i in 0..1_000 {
        let mut f = m.function(&format!("f{i}"), &[I64], &[I64]);
        let r = f.reg(I64);
        for _ in 0..99 {
            f.emit(Inst::IAdd {
                dst: r,
                lhs: r,
                rhs: Reg(0),
                op: op(),
            });
        }
        f.ret(r);
        m.add_function(f).expect("builds");
    }
    let module = m.finish().expect("builds");
    let mut g = c.benchmark_group("load");
    g.throughput(Throughput::Elements(100_000));
    g.bench_function("100k_instructions", |b| {
        b.iter(|| black_box(Program::load(module.clone(), &Host::new()).expect("loads")));
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_dispatch,
    bench_calls,
    bench_maps,
    bench_strings,
    bench_gc,
    bench_load
);
criterion_main!(benches);
