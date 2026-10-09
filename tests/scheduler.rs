//! The built-in deterministic scheduler (`Host::register_scheduler`,
//! `Vm::run_async`) and `spawn`/`await` through it (LSB §5.13 rules 6 and 8,
//! hook 27): FIFO order, awaiting tasks and other values, failures,
//! deadlock, fuel.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use bvm_lang::{Host, Limits, Program, Value, Vm, VmError};
use bytecode_lang::{
    Callee, CoroState, ErrorKind, FuncId, FunctionBuilder, GlobalId, Hook, Inst, IntTy, Kind,
    ModuleBuilder, Policy, Prim, Reg, TypeDef, ValType,
};

const D: ValType = ValType::Dyn;

fn dint(f: &mut FunctionBuilder, v: i32) -> Reg {
    let r = f.reg(D);
    f.emit(Inst::DLoadInt { dst: r, val: v });
    r
}

/// A module with the scheduler bound as the `spawn` hook and a global
/// `log` array.
fn base() -> (ModuleBuilder, GlobalId) {
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D], &[D]);
    let spawn = m.import("ls.async", "spawn", sig);
    m.hook(Hook::Spawn, Callee::Import(spawn));
    let log = m.global("log", D, true, None);
    (m, log)
}

fn host() -> Host {
    let mut h = Host::new();
    h.register_scheduler("ls.async", "spawn");
    h
}

/// Appends `v` to the global log (creating it on first use is the main
/// function's job).
fn log(f: &mut FunctionBuilder, log: GlobalId, v: Reg) {
    let a = f.reg(D);
    f.emit(Inst::GetGlobal {
        dst: a,
        global: log,
    });
    f.emit(Inst::ArrayPush { arr: a, src: v });
}

fn init_log(m: &mut ModuleBuilder, f: &mut FunctionBuilder, log: GlobalId) {
    let at = m.add_type(TypeDef::Array(D));
    let ty = f.type_ref(at);
    let (len, a) = (f.reg(ValType::I64), f.reg(D));
    f.emit(Inst::LoadInt {
        dst: len,
        val: 0,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewArray { dst: a, len, ty });
    f.emit(Inst::SetGlobal {
        global: log,
        src: a,
    });
}

/// `spawn f(args...)` into a fresh window; returns the handle register.
fn spawn(f: &mut FunctionBuilder, func: FuncId, args: &[i32]) -> Reg {
    let fv = f.reg(D);
    f.emit(Inst::MakeClosure { dst: fv, func });
    let w = f.regs(&vec![D; args.len() + 1]);
    for (i, &a) in args.iter().enumerate() {
        f.emit(Inst::DLoadInt {
            dst: Reg(w.0 + 1 + i as u16),
            val: a,
        });
    }
    f.emit(Inst::Spawn {
        dst: w,
        callee: fv,
        argc: u8::try_from(args.len()).unwrap(),
    });
    w
}

/// `worker(id, n)`: n times { log(id); await nil }; return id * 100.
fn worker(m: &mut ModuleBuilder, logg: GlobalId) -> FuncId {
    let mut f = m.function("worker", &[D, D], &[D]);
    let (i, more, s, nil, one) = (f.reg(D), f.reg(ValType::Bool), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::DLoadInt { dst: one, val: 1 });
    f.emit(Inst::DLoadInt { dst: i, val: 0 });
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::DLt {
        dst: more,
        lhs: i,
        rhs: f.param(1),
    });
    f.jmp_if_not(more, done);
    let id = f.param(0);
    log(&mut f, logg, id);
    f.emit(Inst::Await { dst: s, src: nil });
    f.emit(Inst::DAdd {
        dst: i,
        lhs: i,
        rhs: one,
        pol: Policy::new(),
    });
    f.jmp(top);
    f.bind(done);
    let h = dint(&mut f, 100);
    let r = f.reg(D);
    f.emit(Inst::DMul {
        dst: r,
        lhs: f.param(0),
        rhs: h,
        pol: Policy::new(),
    });
    f.ret(r);
    m.add_function(f).unwrap()
}

fn logged(vm: &Vm<'_>, logg: GlobalId) -> Vec<Value> {
    vm.elements(vm.global(logg).unwrap()).unwrap()
}

#[test]
fn tasks_interleave_in_fifo_order_and_await_returns_results() {
    // spawn, await (task), rule 8: round-robin through `await nil`.
    let (mut m, logg) = base();
    let w = worker(&mut m, logg);
    let mut f = m.function("main", &[], &[D]);
    init_log(&mut m, &mut f, logg);
    let a = spawn(&mut f, w, &[1, 3]);
    let b = spawn(&mut f, w, &[2, 2]);
    let (ra, rb, sum) = (f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::Await { dst: ra, src: a });
    let marker = dint(&mut f, 0);
    log(&mut f, logg, marker);
    f.emit(Inst::Await { dst: rb, src: b }); // b already finished: at once
    f.emit(Inst::DAdd {
        dst: sum,
        lhs: ra,
        rhs: rb,
        pol: Policy::new(),
    });
    f.ret(sum);
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    let mut vm = Vm::new(&p);
    assert_eq!(vm.run_async(main, &[]), Ok(Value::Int(300)));
    use Value::Int;
    // main waits for a; a and b alternate; b finishes after its 2 rounds; a
    // after its 3rd; then main resumes.
    assert_eq!(
        logged(&vm, logg),
        vec![Int(1), Int(2), Int(1), Int(2), Int(1), Int(0)]
    );
}

#[test]
fn awaiting_a_failed_task_raises_its_error_at_the_await() {
    // rule 8: errors reach waiters through resume_throw.
    let (mut m, _) = base();
    let mut bad = m.function("bad", &[], &[]);
    let t = dint(&mut bad, 13);
    bad.emit(Inst::Throw { src: t });
    let bad = m.add_function(bad).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let h = spawn(&mut f, bad, &[]);
    let (r, e) = (f.reg(D), f.reg(D));
    let (s, end, hd) = (f.label(), f.label(), f.label());
    f.bind(s);
    f.emit(Inst::Await { dst: r, src: h });
    f.bind(end);
    f.ret(r);
    f.bind(hd);
    f.ret(e);
    f.try_region(s, end, hd, e);
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    assert_eq!(Vm::new(&p).run_async(main, &[]), Ok(Value::Int(13)));
}

#[test]
fn the_main_task_failing_ends_the_run_with_its_error() {
    let (mut m, _) = base();
    let mut f = m.function("main", &[], &[D]);
    let (one, zero, q, s) = (dint(&mut f, 1), dint(&mut f, 0), f.reg(D), f.reg(D));
    let nil = f.reg(D);
    f.emit(Inst::Await { dst: s, src: nil });
    f.emit(Inst::DDiv {
        dst: q,
        lhs: one,
        rhs: zero,
        pol: Policy::new(),
    });
    f.ret(q);
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    assert_eq!(
        Vm::new(&p).run_async(main, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::DivByZero,
            payload: bvm_lang::Value::Nil,
            func: main,
            pc: 3
        })
    );
    // A thrown non-error value reports where it was thrown.
    let (mut m, _) = base();
    let mut f = m.function("main", &[], &[]);
    let t = dint(&mut f, 4);
    f.emit(Inst::Throw { src: t });
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    assert_eq!(
        Vm::new(&p).run_async(main, &[]),
        Err(VmError::Thrown {
            value: Value::Int(4),
            func: main,
            pc: 1
        })
    );
}

#[test]
fn awaiting_values_and_plain_coroutines_and_yielding_tasks() {
    // await of a non-coroutine resumes with that value; await of a plain
    // coroutine adopts it as a task; a task that yields is resumed with nil.
    let (mut m, _) = base();
    let mut gn = m.function("gen", &[], &[D]);
    let (s, t) = (gn.reg(D), gn.reg(D));
    let five = dint(&mut gn, 5);
    gn.emit(Inst::Yield { dst: s, src: five }); // resumed with nil
    let k = gn.reg(ValType::U8);
    gn.emit(Inst::TypeOf { dst: k, src: s });
    gn.emit(Inst::ToDyn {
        dst: t,
        src: k,
        from: Prim::U8,
    });
    gn.ret(t); // 0: nil's kind
    let gn = m.add_function(gn).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let (x, c, r, sum) = (dint(&mut f, 40), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::Await { dst: x, src: x }); // 40 back
    f.emit(Inst::CoroNew {
        dst: c,
        func: gn,
        argc: 0,
    });
    f.emit(Inst::Await { dst: r, src: c }); // 0
    let two = dint(&mut f, 2);
    f.emit(Inst::DAdd {
        dst: sum,
        lhs: x,
        rhs: r,
        pol: Policy::new(),
    });
    f.emit(Inst::DAdd {
        dst: sum,
        lhs: sum,
        rhs: two,
        pol: Policy::new(),
    });
    f.ret(sum);
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    assert_eq!(Vm::new(&p).run_async(main, &[]), Ok(Value::Int(42)));
}

#[test]
fn a_cycle_of_waiting_tasks_is_a_deadlock() {
    // main awaits a task that awaits main.
    let (mut m, _) = base();
    let mut t = m.function("t", &[D], &[D]);
    let r = t.reg(D);
    t.emit(Inst::Await {
        dst: r,
        src: t.param(0),
    });
    t.ret(r);
    let t = m.add_function(t).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let fv = f.reg(D);
    f.emit(Inst::MakeClosure { dst: fv, func: t });
    let w = f.regs(&[D, D]);
    f.emit(Inst::CoroCurrent { dst: Reg(w.0 + 1) });
    f.emit(Inst::Spawn {
        dst: w,
        callee: fv,
        argc: 1,
    });
    let r = f.reg(D);
    f.emit(Inst::Await { dst: r, src: w });
    f.ret(r);
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    assert_eq!(
        Vm::new(&p).run_async(main, &[]),
        Err(VmError::Deadlock { waiting: 2 })
    );
}

#[test]
fn spawn_returns_the_task_and_plain_runs_queue_tasks_for_run_async() {
    // The handle is the coroutine; a task spawned during `run` waits for the
    // next `run_async`; unfinished tasks are dropped when main finishes.
    let (mut m, logg) = base();
    let w = worker(&mut m, logg);
    let mut setup = m.function("setup", &[], &[D]);
    init_log(&mut m, &mut setup, logg);
    let h = spawn(&mut setup, w, &[7, 1]);
    setup.ret(h);
    let setup = m.add_function(setup).unwrap();
    let mut f = m.function("main", &[], &[D]);
    let (s, nil) = (f.reg(D), f.reg(D));
    f.emit(Inst::Await { dst: s, src: nil });
    let one = dint(&mut f, 1);
    f.ret(one);
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    let mut vm = Vm::new(&p);
    let h = vm.run(setup, &[]).unwrap();
    assert_eq!(vm.kind(h), Kind::Coroutine);
    assert_eq!(vm.coro_state(h), Some(CoroState::Created));
    assert_eq!(logged(&vm, logg), vec![]);
    assert_eq!(vm.run_async(main, &[]), Ok(Value::Int(1)));
    // The queued worker ran before main resumed.
    assert_eq!(logged(&vm, logg), vec![Value::Int(7)]);
}

#[test]
fn run_async_is_bounded_by_one_fuel_budget() {
    // Two tasks bouncing forever: OutOfFuel, whatever task is running.
    let (mut m, logg) = base();
    let w = worker(&mut m, logg);
    let mut f = m.function("main", &[], &[D]);
    init_log(&mut m, &mut f, logg);
    let a = spawn(&mut f, w, &[1, i32::MAX]);
    let r = f.reg(D);
    f.emit(Inst::Await { dst: r, src: a });
    f.ret(r);
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    let mut vm = Vm::with_limits(&p, Limits::new().with_fuel(10_000));
    let err = vm.run_async(main, &[]).unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::OutOfFuel));
    assert_eq!(vm.fuel_used(), 10_000);
}

#[test]
fn run_async_checks_its_entry_like_run() {
    let (mut m, _) = base();
    let mut f = m.function("main", &[D], &[D]);
    f.ret(f.param(0));
    let main = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &host()).unwrap();
    let mut vm = Vm::new(&p);
    assert_eq!(
        vm.run_async(main, &[]),
        Err(VmError::ArgumentCount {
            expected: 1,
            found: 0
        })
    );
    assert_eq!(
        vm.run_async(FuncId(9), &[]),
        Err(VmError::NoSuchFunction(FuncId(9)))
    );
    assert_eq!(vm.run_async(main, &[Value::Int(3)]), Ok(Value::Int(3)));
}
