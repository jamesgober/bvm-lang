//! Dynamic-call binding at scale (ISSUES P33): large calls are charged fuel
//! per item, named arguments are found through a hashed index (a hostile
//! named spread binds in time linear in its size), and an empty rest
//! parameter shares one empty storage until it is written.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use bvm_lang::{Host, HostCtx, HostError, Limits, Memory, Program, Value, Vm, VmError};
use bytecode_lang::{
    ArgKind, Const, ErrorKind, FuncId, Inst, ModuleBuilder, Param, ParamKind, ParamList, Reg,
    ValType,
};

const D: ValType = ValType::Dyn;

/// `main()`: calls `f(...$k)` where `$k` is an `n`-element constant array
/// and `f` collects everything in a rest parameter; returns the rest.
fn spread_program(n: usize) -> (Program, FuncId) {
    let mut m = ModuleBuilder::new();
    let items: Vec<_> = (0..n)
        .map(|i| m.constant(Const::Int(i64::try_from(i).unwrap())))
        .collect();
    let k = m.constant(Const::Array(items));
    let rest = m.string("rest");
    let mut f = m.function("f", &[D], &[D]);
    f.set_params(ParamList::new(vec![Param::new(
        ParamKind::Rest,
        Some(rest),
    )]));
    f.ret(Reg(0));
    let f = m.add_function(f).unwrap();
    let mut main = m.function("main", &[], &[D]);
    let fv = main.reg(D);
    let w = main.regs(&[D, D]);
    main.emit(Inst::MakeClosure { dst: fv, func: f });
    main.emit(Inst::DLoadConst {
        dst: Reg(w.0 + 1),
        k,
    });
    main.dcall_shape(w, fv, &[ArgKind::Spread]);
    main.ret(w);
    let main = m.add_function(main).unwrap();
    (
        Program::load(m.finish().unwrap(), &Host::new()).unwrap(),
        main,
    )
}

/// A call of `n` items costs one unit plus one per 64 items; below 64
/// items exactly LSB §5.14's one unit. One unit short traps at the call,
/// which has no effect.
#[test]
fn large_calls_are_charged_per_item() {
    for (n, extra) in [(0, 0), (63, 0), (64, 1), (640, 10), (6_400, 100)] {
        let (p, main) = spread_program(n);
        let mut vm = Vm::new(&p);
        let out = vm.run(main, &[]).unwrap();
        assert_eq!(vm.elements(out).map(|e| e.len()), Some(n));
        assert_eq!(vm.fuel_used(), 1 + extra, "{n} items");
        if extra > 0 {
            for fuel in [extra, extra - 1] {
                let err = vm
                    .run_with(main, &[], Limits::new().with_fuel(fuel))
                    .unwrap_err();
                assert_eq!(
                    err,
                    VmError::Trap {
                        kind: ErrorKind::OutOfFuel,
                        func: main,
                        pc: 2,
                    },
                    "{n} items, fuel {fuel}"
                );
            }
        }
    }
}

/// The host function that makes the hostile argument: a map of `n` string
/// keys `k0`, `k1`, ... plus every parameter name `p0`..`p199`.
fn host_with_spread(n: usize) -> Host {
    let mut host = Host::new();
    host.register(
        "env",
        "named",
        move |ctx: &mut HostCtx<'_>, _args: &[Value]| {
            let mut entries = Vec::with_capacity(n + 200);
            for i in 0..200 {
                let k = ctx.new_str(format!("p{i}").as_bytes())?;
                entries.push((k, Value::Int(i)));
            }
            for i in 0..n {
                let k = ctx.new_str(format!("k{i}").as_bytes())?;
                entries.push((k, Value::Int(-1)));
            }
            ctx.new_map(&entries)
                .map_err(|_: HostError| HostError::Raise(ErrorKind::OutOfMemory))
        },
    );
    host
}

/// `f($p0, ..., $p199, ...$extra)` called with a named spread of `n + 200`
/// keys: every parameter is found through the hashed name index and the
/// unknown names go to the named rest (duplicates checked with an ordered
/// set), so binding is O(n log n), not O(n x 200); the call is charged per
/// item.
fn named_spread(n: usize) -> (Result<Value, VmError>, u64, Option<usize>) {
    let mut m = ModuleBuilder::new();
    let names: Vec<_> = (0..200).map(|i| m.string(&format!("p{i}"))).collect();
    let extra = m.string("extra");
    let mut params: Vec<Param> = names.iter().map(|&s| Param::normal(s)).collect();
    params.push(Param::new(ParamKind::RestNamed, Some(extra)));
    let sig = vec![D; 201];
    let mut f = m.function("f", &sig, &[D]);
    f.set_params(ParamList::new(params));
    f.ret(Reg(200));
    let f = m.add_function(f).unwrap();
    let isig = m.func_type(&[], &[D]);
    let named = m.import("env", "named", isig);
    let mut main = m.function("main", &[], &[D]);
    let fv = main.reg(D);
    let w = main.regs(&[D, D]);
    main.emit(Inst::MakeClosure { dst: fv, func: f });
    main.emit(Inst::CallImport {
        dst: Reg(w.0 + 1),
        import: named,
        argc: 0,
    });
    main.dcall_shape(w, fv, &[ArgKind::SpreadNamed]);
    main.ret(w);
    let main = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &host_with_spread(n)).unwrap();
    let mut vm = Vm::new(&p);
    let out = vm.run(main, &[]);
    let len = out
        .as_ref()
        .ok()
        .and_then(|v| vm.entries(*v))
        .map(|e| e.len());
    (out, vm.fuel_used(), len)
}

#[test]
fn a_hostile_named_spread_binds_in_linear_time_and_pays_for_it() {
    let n = 200_000;
    let (out, fuel, len) = named_spread(n);
    assert!(out.is_ok(), "{out:?}");
    assert_eq!(len, Some(n), "the unknown names are the named rest");
    // One unit for the call import, one for the dcall, and one per 64 of
    // the n + 200 items.
    assert_eq!(fuel, 2 + ((n + 200) / 64) as u64);
}

/// `f(...$rest) { $rest[] = 1; return $rest; }` called with nothing,
/// twice: each call's empty rest is its own array once written (the shared
/// empty storage is copied on that first write), under both profiles; the
/// same for a PHP-style `rest_map`.
#[test]
fn an_empty_rest_shares_storage_until_written() {
    for kind in [ParamKind::Rest, ParamKind::RestMap] {
        for memory in [Memory::Tracing, Memory::Rc] {
            let mut m = ModuleBuilder::new();
            let rest = m.string("rest");
            let mut f = m.function("f", &[D], &[D]);
            f.set_params(ParamList::new(vec![Param::new(kind, Some(rest))]));
            let one = f.reg(D);
            f.emit(Inst::DLoadInt { dst: one, val: 1 });
            if kind == ParamKind::Rest {
                f.emit(Inst::ArrayPush {
                    arr: Reg(0),
                    src: one,
                });
            } else {
                f.emit(Inst::MapPush {
                    map: Reg(0),
                    src: one,
                });
            }
            f.ret(Reg(0));
            let f = m.add_function(f).unwrap();
            let mut main = m.function("main", &[], &[D]);
            let (fv, a, b) = (main.reg(D), main.reg(D), main.reg(D));
            let w = main.regs(&[D]);
            main.emit(Inst::MakeClosure { dst: fv, func: f });
            main.emit(Inst::DCall {
                dst: w,
                callee: fv,
                argc: 0,
            });
            main.mov(a, w);
            main.emit(Inst::DCall {
                dst: w,
                callee: fv,
                argc: 0,
            });
            main.mov(b, w);
            // [a, b] as a two-element array: both must be [1] / {0: 1}.
            let at = m.add_type(bytecode_lang::TypeDef::Array(D));
            let ty = main.type_ref(at);
            let (len, out) = (main.reg(ValType::I64), main.reg(D));
            main.emit(Inst::LoadInt {
                dst: len,
                val: 0,
                ty: bytecode_lang::IntTy::I64,
            });
            main.emit(Inst::NewArray { dst: out, len, ty });
            main.emit(Inst::ArrayPush { arr: out, src: a });
            main.emit(Inst::ArrayPush { arr: out, src: b });
            main.ret(out);
            let main = m.add_function(main).unwrap();
            let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
            let mut vm = Vm::with_memory(&p, memory);
            let out = vm.run(main, &[]).unwrap();
            let parts = vm.elements(out).unwrap();
            assert_eq!(parts.len(), 2);
            assert_ne!(parts[0], parts[1], "two calls, two arrays");
            for part in parts {
                let n = match kind {
                    ParamKind::Rest => vm.elements(part).unwrap().len(),
                    _ => vm.entries(part).unwrap().len(),
                };
                assert_eq!(n, 1, "{kind:?} {memory:?}");
            }
        }
    }
}
