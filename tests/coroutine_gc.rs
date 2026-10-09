//! Coroutines and the collector (LSB §5.13 "GC" and rule 13 as decided in
//! §10 question 9): suspended frames are traced; finished and never-started
//! coroutines are freed; a dropped suspended coroutine is closed (its
//! pending `finally` runs) from the finalization queue, in creation order,
//! at the next safepoint of a run or by `Vm::run_finalizers`; suspended
//! stacks count against the memory budget.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use bvm_lang::{Host, Limits, Program, Value, Vm, VmError};
use bytecode_lang::{
    CoroState, ErrorKind, FuncId, FunctionBuilder, GlobalId, Inst, IntOp, IntTy, ModuleBuilder,
    Reg, TypeDef, ValType,
};

const D: ValType = ValType::Dyn;
const I64: ValType = ValType::I64;

fn dint(f: &mut FunctionBuilder, v: i32) -> Reg {
    let r = f.reg(D);
    f.emit(Inst::DLoadInt { dst: r, val: v });
    r
}

fn load(m: ModuleBuilder) -> Program {
    Program::load(m.finish().unwrap(), &Host::new()).unwrap()
}

/// What a generator's `finally` does besides logging.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Fin {
    /// Log and rethrow (an ordinary `finally`).
    Plain,
    /// Log and throw something else.
    Throw,
    /// Log and yield (refusing the close).
    Yield,
    /// Log and trap.
    Trap,
}

/// `gen(id)`: `try { yield id; yield id } finally { log.push(id); <fin> }`.
fn make_gen(m: &mut ModuleBuilder, log: GlobalId, fin: Fin) -> FuncId {
    let mut g = m.function("gen", &[D], &[]);
    let (s, e, a) = (g.reg(D), g.reg(D), g.reg(D));
    let (start, end, h) = (g.label(), g.label(), g.label());
    g.bind(start);
    g.emit(Inst::Yield {
        dst: s,
        src: g.param(0),
    });
    g.emit(Inst::Yield {
        dst: s,
        src: g.param(0),
    });
    g.bind(end);
    g.ret_void();
    g.bind(h);
    g.emit(Inst::GetGlobal {
        dst: a,
        global: log,
    });
    let id = g.param(0);
    g.emit(Inst::ArrayPush { arr: a, src: id });
    match fin {
        Fin::Plain => {
            g.emit(Inst::Throw { src: e });
        }
        Fin::Throw => {
            let x = dint(&mut g, 1);
            g.emit(Inst::Throw { src: x });
        }
        Fin::Yield => {
            g.emit(Inst::Yield { dst: s, src: s });
            g.ret_void();
        }
        Fin::Trap => {
            g.emit(Inst::Unreachable {});
        }
    }
    g.try_region(start, end, h, e);
    m.add_function(g).unwrap()
}

/// A module with a `log` array global, the generator, and:
/// - `init()`: creates the log;
/// - `make(id)`: creates `gen(id)`, resumes it once (suspended inside the
///   try), and drops it;
/// - `keep(id)`: the same, but stores it in the global `kept`.
struct Fixture {
    p: Program,
    log: GlobalId,
    kept: GlobalId,
    init: FuncId,
    make: FuncId,
    keep: FuncId,
}

fn fixture(fin: Fin) -> Fixture {
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let log = m.global("log", D, true, None);
    let kept = m.global("kept", D, true, None);
    let g = make_gen(&mut m, log, fin);
    let mut init = m.function("init", &[], &[]);
    let ty = init.type_ref(at);
    let (len, a) = (init.reg(I64), init.reg(D));
    init.emit(Inst::NewArray { dst: a, len, ty });
    init.emit(Inst::SetGlobal {
        global: log,
        src: a,
    });
    init.ret_void();
    let init = m.add_function(init).unwrap();
    let mut ids = Vec::new();
    for store in [false, true] {
        let mut f = m.function(if store { "keep" } else { "make" }, &[D], &[]);
        let w = f.regs(&[D, D]);
        f.mov(Reg(w.0 + 1), f.param(0));
        f.emit(Inst::CoroNew {
            dst: w,
            func: g,
            argc: 1,
        });
        let r = f.reg(D);
        f.emit(Inst::Resume {
            dst: r,
            coro: w,
            src: r,
        });
        if store {
            f.emit(Inst::SetGlobal {
                global: kept,
                src: w,
            });
        }
        f.ret_void();
        ids.push(m.add_function(f).unwrap());
    }
    Fixture {
        p: load(m),
        log,
        kept,
        init,
        make: ids[0],
        keep: ids[1],
    }
}

fn logged(vm: &Vm<'_>, log: GlobalId) -> Vec<Value> {
    vm.elements(vm.global(log).unwrap()).unwrap()
}

#[test]
fn a_dropped_suspended_coroutine_is_closed_by_run_finalizers() {
    let fx = fixture(Fin::Plain);
    let mut vm = Vm::new(&fx.p);
    vm.run(fx.init, &[]).unwrap();
    vm.run(fx.make, &[Value::Int(1)]).unwrap();
    // Nothing runs until a collection finds it unreachable...
    assert_eq!(logged(&vm, fx.log), vec![]);
    vm.collect_garbage();
    assert_eq!(vm.pending_finalizers(), 1);
    assert_eq!(logged(&vm, fx.log), vec![]);
    // ...and then its finally runs, once.
    assert_eq!(vm.run_finalizers(), Ok(1));
    assert_eq!(logged(&vm, fx.log), vec![Value::Int(1)]);
    assert_eq!(vm.pending_finalizers(), 0);
    let before = vm.heap_objects();
    vm.collect_garbage();
    assert_eq!(vm.pending_finalizers(), 0);
    assert!(vm.heap_objects() < before, "the closed coroutine is freed");
    assert_eq!(vm.run_finalizers(), Ok(0));
}

#[test]
fn dropped_coroutines_close_in_creation_order() {
    let fx = fixture(Fin::Plain);
    let mut vm = Vm::new(&fx.p);
    vm.run(fx.init, &[]).unwrap();
    for id in [3, 1, 2] {
        vm.run(fx.make, &[Value::Int(id)]).unwrap();
    }
    vm.collect_garbage();
    assert_eq!(vm.pending_finalizers(), 3);
    assert_eq!(vm.run_finalizers(), Ok(3));
    use Value::Int;
    assert_eq!(logged(&vm, fx.log), vec![Int(3), Int(1), Int(2)]);
}

#[test]
fn reachable_coroutines_are_not_closed_and_their_frames_survive() {
    // A kept generator is traced (its frames' references included) and
    // resumes after collections with its state intact.
    let fx = fixture(Fin::Plain);
    let mut vm = Vm::new(&fx.p);
    vm.run(fx.init, &[]).unwrap();
    vm.run(fx.keep, &[Value::Int(9)]).unwrap();
    for _ in 0..3 {
        vm.collect_garbage();
    }
    assert_eq!(vm.pending_finalizers(), 0);
    let c = vm.global(fx.kept).unwrap();
    assert_eq!(vm.coro_state(c), Some(CoroState::Yielded));
    assert_eq!(logged(&vm, fx.log), vec![]);
}

#[test]
fn queued_closes_run_at_the_next_safepoint_of_a_run() {
    // Collections inside a run queue dropped coroutines; each is closed at
    // a following frame transition, before the run ends.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let log = m.global("log", D, true, None);
    let g = make_gen(&mut m, log, Fin::Plain);
    // main(n): log = []; for i in 0..n { c = gen(i); resume c; drop;
    // allocate 4 KiB of garbage } ; return log
    let mut f = m.function("main", &[I64], &[D]);
    let ty = f.type_ref(at);
    let (len, a, i, one, more, r, big) = (
        f.reg(I64),
        f.reg(D),
        f.reg(I64),
        f.reg(I64),
        f.reg(ValType::Bool),
        f.reg(D),
        f.reg(I64),
    );
    f.emit(Inst::NewArray { dst: a, len, ty });
    f.emit(Inst::SetGlobal {
        global: log,
        src: a,
    });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: big,
        val: 512,
        ty: IntTy::I64,
    });
    let w = f.regs(&[D, D]);
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::ILt {
        dst: more,
        lhs: i,
        rhs: f.param(0),
        ty: IntTy::I64,
    });
    f.jmp_if_not(more, done);
    f.emit(Inst::ToDyn {
        dst: Reg(w.0 + 1),
        src: i,
        from: bytecode_lang::Prim::I64,
    });
    f.emit(Inst::CoroNew {
        dst: w,
        func: g,
        argc: 1,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: w,
        src: r,
    });
    f.emit(Inst::NewArray {
        dst: r,
        len: big,
        ty,
    });
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: IntOp::new(IntTy::I64),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    // The last coroutine is still referenced by `w`: drop it so the final
    // count is exact after the host collects.
    f.emit(Inst::LoadNil { dst: w });
    f.emit(Inst::GetGlobal {
        dst: a,
        global: log,
    });
    f.ret(a);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_limits(&p, Limits::new().with_memory(4 << 20));
    let out = vm.run(main, &[Value::Int(2_000)]).unwrap();
    let during = vm.elements(out).unwrap().len();
    assert!(vm.collections() > 0);
    assert!(during > 0, "some closes ran inside the run");
    assert!(during < 2_000);
    // Each logged id is distinct and in increasing (creation) order.
    let ids: Vec<i64> = vm
        .elements(out)
        .unwrap()
        .iter()
        .map(|v| v.as_int().unwrap())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
    vm.collect_garbage();
    let _ = vm.run_finalizers().unwrap();
    let all = vm.elements(vm.global(log).unwrap()).unwrap().len();
    assert_eq!(
        all, 2_000,
        "every dropped coroutine was closed exactly once"
    );
}

#[test]
fn a_close_that_yields_or_throws_is_discarded_and_runs_once() {
    for fin in [Fin::Yield, Fin::Throw] {
        let fx = fixture(fin);
        let mut vm = Vm::new(&fx.p);
        vm.run(fx.init, &[]).unwrap();
        vm.run(fx.make, &[Value::Int(5)]).unwrap();
        vm.collect_garbage();
        assert_eq!(vm.run_finalizers(), Ok(1), "{fin:?}");
        assert_eq!(logged(&vm, fx.log), vec![Value::Int(5)]);
        // A refusing coroutine stays suspended but is never closed again;
        // the next collection frees it.
        vm.collect_garbage();
        assert_eq!(vm.pending_finalizers(), 0);
        assert_eq!(vm.run_finalizers(), Ok(0));
        assert_eq!(logged(&vm, fx.log), vec![Value::Int(5)]);
    }
}

#[test]
fn a_trap_in_a_close_stops_run_finalizers() {
    let fx = fixture(Fin::Trap);
    let mut vm = Vm::new(&fx.p);
    vm.run(fx.init, &[]).unwrap();
    vm.run(fx.make, &[Value::Int(1)]).unwrap();
    vm.run(fx.make, &[Value::Int(2)]).unwrap();
    vm.collect_garbage();
    assert!(matches!(
        vm.run_finalizers(),
        Err(VmError::Trap {
            kind: ErrorKind::Unreachable,
            ..
        })
    ));
    // The second is still queued.
    assert_eq!(vm.pending_finalizers(), 1);
    assert_eq!(logged(&vm, fx.log), vec![Value::Int(1)]);
}

#[test]
fn created_and_finished_coroutines_are_freed_without_running_code() {
    // Only suspended coroutines have pending finally blocks to run.
    let mut m = ModuleBuilder::new();
    let log = m.global("log", D, true, None);
    let g = make_gen(&mut m, log, Fin::Plain);
    let mut f = m.function("main", &[I64], &[]);
    let (i, one, more) = (f.reg(I64), f.reg(I64), f.reg(ValType::Bool));
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    let w = f.regs(&[D, D]);
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::ILt {
        dst: more,
        lhs: i,
        rhs: f.param(0),
        ty: IntTy::I64,
    });
    f.jmp_if_not(more, done);
    f.emit(Inst::CoroNew {
        dst: w,
        func: g,
        argc: 1,
    });
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: IntOp::new(IntTy::I64),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    f.ret_void();
    let main = m.add_function(f).unwrap();
    let p = load(m);
    // 100,000 coroutines created and dropped inside a 2 MiB budget.
    let mut vm = Vm::with_limits(&p, Limits::new().with_memory(2 << 20));
    assert_eq!(vm.run(main, &[Value::Int(100_000)]), Ok(Value::Nil));
    assert!(vm.collections() > 0);
    vm.collect_garbage();
    assert_eq!(vm.pending_finalizers(), 0);
    assert_eq!(vm.heap_objects(), 0);
}

#[test]
fn a_coroutine_holding_itself_is_closed_then_collected() {
    // A cycle through a suspended frame (the coroutine's own handle in its
    // registers) is unreachable from outside: closed, then freed.
    let mut m = ModuleBuilder::new();
    let count = m.global("count", I64, true, None);
    let mut g = m.function("selfish", &[], &[]);
    let (me, s, e, c, one) = (g.reg(D), g.reg(D), g.reg(D), g.reg(I64), g.reg(I64));
    let (start, end, h) = (g.label(), g.label(), g.label());
    g.emit(Inst::CoroCurrent { dst: me });
    g.bind(start);
    g.emit(Inst::Yield { dst: s, src: me });
    g.bind(end);
    g.ret_void();
    g.bind(h);
    g.emit(Inst::GetGlobal {
        dst: c,
        global: count,
    });
    g.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    g.emit(Inst::IAdd {
        dst: c,
        lhs: c,
        rhs: one,
        op: IntOp::new(IntTy::I64),
    });
    g.emit(Inst::SetGlobal {
        global: count,
        src: c,
    });
    g.emit(Inst::Throw { src: e });
    g.try_region(start, end, h, e);
    let g = m.add_function(g).unwrap();
    let mut f = m.function("main", &[], &[]);
    let (c, r) = (f.reg(D), f.reg(D));
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
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
    vm.run(main, &[]).unwrap();
    vm.collect_garbage();
    assert_eq!(vm.run_finalizers(), Ok(1));
    assert_eq!(vm.global(count), Some(Value::Int(1)));
    vm.collect_garbage();
    assert_eq!(vm.heap_objects(), 0);
}

#[test]
fn suspended_stacks_count_against_the_memory_budget() {
    // Keeping many suspended coroutines with wide frames alive exhausts the
    // budget: the OutOfMemory trap, not host memory.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let mut g = m.function("wide", &[], &[]);
    for _ in 0..1_000 {
        let _ = g.reg(D);
    }
    let s = g.reg(D);
    g.emit(Inst::Yield { dst: s, src: s });
    g.ret_void();
    let g = m.add_function(g).unwrap();
    let mut f = m.function("main", &[], &[]);
    let ty = f.type_ref(at);
    let (len, keep, c, r) = (f.reg(I64), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::NewArray { dst: keep, len, ty });
    let top = f.label();
    f.bind(top);
    f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    f.emit(Inst::Resume {
        dst: r,
        coro: c,
        src: r,
    });
    f.emit(Inst::ArrayPush { arr: keep, src: c });
    f.jmp(top);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_limits(&p, Limits::new().with_memory(4 << 20));
    let err = vm.run(main, &[]).unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::OutOfMemory));
    assert!(vm.heap_bytes() <= 4 << 20);
}
