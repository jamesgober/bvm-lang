//! The memory profiles (DECISIONS D22) where they differ, each with its
//! expected values: PHP's destructor timing under `Memory::Rc` (scope exit,
//! reassignment, `unset`, array element overwrite, array and object
//! destruction order, cycles at the cycle-collection point, resurrection,
//! errors and traps in destructors), destructors under `Memory::Tracing`
//! (after the collection that finds the object), close on drop at the last
//! reference, PHP's reference unbinding on copy (LSB §5.17 "the one
//! divergence"; P34's runtime side), the cycle collector (shared storage,
//! finalizers, fuel), and hostile inputs (million-node cycles and chains).
//! Where the profiles agree, `differential.rs` runs the same random
//! programs under both.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use bvm_lang::{Host, Limits, Memory, Program, Value, Vm, VmError};
use bytecode_lang::{
    ErrorKind, Field, FieldIdx, FuncId, FunctionBuilder, GlobalId, Inst, IntOp, IntTy, Method,
    ModuleBuilder, Overflow, Policy, Reg, StructDef, TypeDef, TypeId, TypeRef, ValType,
};

const D: ValType = ValType::Dyn;
const I: ValType = ValType::I64;

/// Wrapping `i64` arithmetic: logs are digit strings that may run long.
fn op() -> IntOp {
    IntOp::new(IntTy::I64).with_policy(Policy::new().with_overflow(Overflow::Wrap))
}

/// What the destructor does after it logs.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Dtor {
    Log,
    /// Stores `$this` in the global `keep`.
    Resurrect,
    /// Throws.
    Throw,
    /// Traps (`unreachable`).
    Trap,
}

/// The ids every test program uses.
#[derive(Clone, Copy, Debug)]
struct Ids {
    /// `Obj { id: i64, other: dyn, more: dyn }` with `__destruct`.
    obj: TypeId,
    /// `log = log * 10 + id` per destructor call (and `mark`).
    log: GlobalId,
    /// Destructor calls.
    count: GlobalId,
    keep: GlobalId,
}

/// A module with `Obj` and its destructor (function 0).
fn kit(d: Dtor) -> (ModuleBuilder, Ids) {
    let mut m = ModuleBuilder::new();
    let obj = m.reserve_type();
    let log = m.global("log", I, true, None);
    let count = m.global("count", I, true, None);
    let keep = m.global("keep", D, true, None);
    let mut f = m.function("Obj::__destruct", &[D], &[]);
    let this = f.param(0);
    let id = f.reg(I);
    f.emit(Inst::GetField {
        dst: id,
        obj: this,
        field: FieldIdx(0),
    });
    log_digit(&mut f, log, id);
    let (c, one) = (f.reg(I), f.reg(I));
    f.emit(Inst::GetGlobal {
        dst: c,
        global: count,
    });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::IAdd {
        dst: c,
        lhs: c,
        rhs: one,
        op: op(),
    });
    f.emit(Inst::SetGlobal {
        global: count,
        src: c,
    });
    match d {
        Dtor::Log => {}
        Dtor::Resurrect => {
            f.emit(Inst::SetGlobal {
                global: keep,
                src: this,
            });
        }
        Dtor::Throw => {
            f.emit(Inst::Throw { src: this });
        }
        Dtor::Trap => {
            f.emit(Inst::Unreachable {});
        }
    }
    f.ret_void();
    let dtor = m.add_function(f).unwrap();
    let (name, idn, other, more, dn) = (
        m.string("Obj"),
        m.string("id"),
        m.string("other"),
        m.string("more"),
        m.string("__destruct"),
    );
    m.define_type(
        obj,
        TypeDef::Struct(StructDef {
            name,
            parent: None,
            fields: vec![
                Field { name: idn, ty: I },
                Field { name: other, ty: D },
                Field { name: more, ty: D },
            ],
            methods: vec![Method {
                name: dn,
                func: dtor,
            }],
        }),
    );
    (
        m,
        Ids {
            obj,
            log,
            count,
            keep,
        },
    )
}

/// `log = log * 10 + digit` (the digit in an `i64` register).
fn log_digit(f: &mut FunctionBuilder, log: GlobalId, digit: Reg) {
    let (l, ten) = (f.reg(I), f.reg(I));
    f.emit(Inst::GetGlobal {
        dst: l,
        global: log,
    });
    f.emit(Inst::LoadInt {
        dst: ten,
        val: 10,
        ty: IntTy::I64,
    });
    f.emit(Inst::IMul {
        dst: l,
        lhs: l,
        rhs: ten,
        op: op(),
    });
    f.emit(Inst::IAdd {
        dst: l,
        lhs: l,
        rhs: digit,
        op: op(),
    });
    f.emit(Inst::SetGlobal {
        global: log,
        src: l,
    });
}

/// A marker from the main code: `log = log * 10 + digit`.
fn mark(f: &mut FunctionBuilder, ids: Ids, digit: i32) {
    let d = f.reg(I);
    f.emit(Inst::LoadInt {
        dst: d,
        val: digit,
        ty: IntTy::I64,
    });
    log_digit(f, ids.log, d);
}

/// `dst = new Obj(id)` (the old value of `dst` is overwritten by the
/// `new_struct` itself).
fn new_into(f: &mut FunctionBuilder, ids: Ids, dst: Reg, id: i32) {
    let ty = f.type_ref(ids.obj);
    f.emit(Inst::NewStruct { dst, ty });
    let v = f.reg(I);
    f.emit(Inst::LoadInt {
        dst: v,
        val: id,
        ty: IntTy::I64,
    });
    f.emit(Inst::SetField {
        obj: dst,
        field: FieldIdx(0),
        src: v,
    });
}

fn new_obj(f: &mut FunctionBuilder, ids: Ids, id: i32) -> Reg {
    let r = f.reg(D);
    new_into(f, ids, r, id);
    r
}

fn nil(f: &mut FunctionBuilder, r: Reg) {
    f.emit(Inst::LoadNil { dst: r });
}

/// Returns the log as it is at the return.
fn ret_log(f: &mut FunctionBuilder, ids: Ids) {
    let r = f.reg(I);
    f.emit(Inst::GetGlobal {
        dst: r,
        global: ids.log,
    });
    f.ret(r);
}

fn load(m: ModuleBuilder) -> Program {
    Program::load(m.finish().unwrap(), &Host::new()).unwrap()
}

fn global_int(vm: &Vm<'_>, g: GlobalId) -> i64 {
    vm.global(g).and_then(|v| v.as_int()).unwrap_or(-1)
}

/// Builds `main` (returning the log at its return) and runs it with the
/// destructor set: (the run's result, the log afterwards).
fn destructors(
    d: Dtor,
    memory: Memory,
    body: impl FnOnce(&mut ModuleBuilder, &mut FunctionBuilder, Ids),
) -> (Result<Value, VmError>, i64) {
    let (mut m, ids) = kit(d);
    let mut f = m.function("main", &[], &[I]);
    body(&mut m, &mut f, ids);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, memory);
    assert_eq!(vm.set_destructor("__destruct"), Ok(1));
    let out = vm.run(main, &[]);
    let log = global_int(&vm, ids.log);
    (out, log)
}

// ---------------------------------------------------------------------------
// PHP's destructor timing (Memory::Rc)
// ---------------------------------------------------------------------------

/// `$a = new Obj(1); $a = new Obj(2); echo 9;` prints `1` (the first
/// object's destructor), `9`, then `2` when the script ends: `192`.
#[test]
fn reassignment_destructs_the_old_object_before_the_next_statement() {
    let (out, log) = destructors(Dtor::Log, Memory::Rc, |_, f, ids| {
        let a = new_obj(f, ids, 1);
        new_into(f, ids, a, 2);
        mark(f, ids, 9);
        ret_log(f, ids);
    });
    // At the return only the first destructor and the marker have run;
    // the second object dies with the frame, before the run returns.
    assert_eq!(out, Ok(Value::Int(19)));
    assert_eq!(log, 192);
}

/// `$a = new Obj(1); $b = new Obj(2); unset($a); echo 9;`: `192`.
#[test]
fn unset_destructs_at_once() {
    let (out, log) = destructors(Dtor::Log, Memory::Rc, |_, f, ids| {
        let a = new_obj(f, ids, 1);
        let _b = new_obj(f, ids, 2);
        nil(f, a);
        mark(f, ids, 9);
        ret_log(f, ids);
    });
    assert_eq!(out, Ok(Value::Int(19)));
    assert_eq!(log, 192);
}

/// `function h() { $x = new Obj(1); $y = new Obj(2); } h(); echo 9;`: the
/// locals die at `h`'s return, in declaration order, before `9`: `129`.
#[test]
fn scope_exit_destructs_locals_in_order_before_the_caller_continues() {
    let (out, log) = destructors(Dtor::Log, Memory::Rc, |m, f, ids| {
        let mut h = m.function("h", &[], &[]);
        let _x = new_obj(&mut h, ids, 1);
        let _y = new_obj(&mut h, ids, 2);
        h.ret_void();
        let h = m.add_function(h).unwrap();
        let w = f.reg(D);
        f.emit(Inst::Call {
            dst: w,
            func: h,
            argc: 0,
        });
        mark(f, ids, 9);
        ret_log(f, ids);
    });
    assert_eq!(out, Ok(Value::Int(129)));
    assert_eq!(log, 129);
}

/// An array type for the tests.
fn arr_type(m: &mut ModuleBuilder, f: &mut FunctionBuilder) -> TypeRef {
    let t = m.add_type(TypeDef::Array(D));
    f.type_ref(t)
}

/// A new empty `dyn` array.
fn new_arr(m: &mut ModuleBuilder, f: &mut FunctionBuilder) -> Reg {
    let ty = arr_type(m, f);
    let (n, r) = (f.reg(I), f.reg(D));
    f.emit(Inst::LoadInt {
        dst: n,
        val: 0,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewArray { dst: r, len: n, ty });
    r
}

fn dint(f: &mut FunctionBuilder, v: i32) -> Reg {
    let r = f.reg(D);
    f.emit(Inst::DLoadInt { dst: r, val: v });
    r
}

/// `$arr = [new Obj(1)]; $arr[0] = new Obj(2); echo 9; $arr = null;`:
/// overwriting the element destructs the old one: `192`.
#[test]
fn overwriting_an_array_element_destructs_the_old_value() {
    let (out, log) = destructors(Dtor::Log, Memory::Rc, |m, f, ids| {
        let arr = new_arr(m, f);
        let o = new_obj(f, ids, 1);
        f.emit(Inst::ArrayPush { arr, src: o });
        nil(f, o);
        let zero = dint(f, 0);
        let o2 = new_obj(f, ids, 2);
        f.emit(Inst::DSetIndex {
            obj: arr,
            key: zero,
            src: o2,
        });
        nil(f, o2);
        mark(f, ids, 9);
        nil(f, arr);
        ret_log(f, ids);
    });
    assert_eq!(out, Ok(Value::Int(192)));
    assert_eq!(log, 192);
}

/// `$arr = [new Obj(1), new Obj(2), new Obj(3)]; $arr = null; echo 9;`:
/// an array's elements are destructed in order: `1239`.
#[test]
fn destroying_an_array_destructs_its_elements_in_order() {
    let (out, log) = destructors(Dtor::Log, Memory::Rc, |m, f, ids| {
        let arr = new_arr(m, f);
        let o = f.reg(D);
        for id in 1..=3 {
            new_into(f, ids, o, id);
            f.emit(Inst::ArrayPush { arr, src: o });
        }
        nil(f, o);
        nil(f, arr);
        mark(f, ids, 9);
        ret_log(f, ids);
    });
    assert_eq!(out, Ok(Value::Int(1239)));
    assert_eq!(log, 1239);
}

/// `$o = new Obj(1); $o->other = new Obj(2); $o = null; echo 9;`: the
/// outer destructor runs first, the inner one when the outer is freed:
/// `129`.
#[test]
fn an_object_is_destructed_before_what_it_holds() {
    let (out, log) = destructors(Dtor::Log, Memory::Rc, |_, f, ids| {
        let o = new_obj(f, ids, 1);
        let inner = new_obj(f, ids, 2);
        f.emit(Inst::SetField {
            obj: o,
            field: FieldIdx(1),
            src: inner,
        });
        nil(f, inner);
        nil(f, o);
        mark(f, ids, 9);
        ret_log(f, ids);
    });
    assert_eq!(out, Ok(Value::Int(129)));
    assert_eq!(log, 129);
}

/// A destructor that stores `$this` somewhere resurrects the object, which
/// is never destructed again (PHP).
#[test]
fn a_resurrected_object_lives_on_and_is_destructed_once() {
    let (mut m, ids) = kit(Dtor::Resurrect);
    let mut f = m.function("main", &[], &[I]);
    let o = new_obj(&mut f, ids, 1);
    nil(&mut f, o);
    mark(&mut f, ids, 9);
    ret_log(&mut f, ids);
    let main = m.add_function(f).unwrap();
    let mut f2 = m.function("release", &[], &[]);
    let n = f2.reg(D);
    f2.emit(Inst::SetGlobal {
        global: ids.keep,
        src: n,
    });
    f2.ret_void();
    let release = m.add_function(f2).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    vm.set_destructor("__destruct").unwrap();
    assert_eq!(vm.run(main, &[]), Ok(Value::Int(19)));
    let kept = vm.global(ids.keep).unwrap();
    assert_eq!(vm.field(kept, 0), Some(Value::Int(1)));
    let live = vm.heap_objects();
    vm.run(release, &[]).unwrap();
    assert_eq!(global_int(&vm, ids.count), 1, "never destructed twice");
    assert_eq!(vm.heap_objects(), live - 1, "freed at the last reference");
}

/// An error escaping a destructor is discarded (as for a drop-close; PHP
/// would raise it where the object died: see the release notes).
#[test]
fn an_error_escaping_a_destructor_is_discarded() {
    let (out, log) = destructors(Dtor::Throw, Memory::Rc, |_, f, ids| {
        let o = new_obj(f, ids, 1);
        nil(f, o);
        mark(f, ids, 9);
        ret_log(f, ids);
    });
    assert_eq!(out, Ok(Value::Int(19)));
    assert_eq!(log, 19);
}

/// A trap in a destructor ends the run, at the destructor's own pc.
#[test]
fn a_trap_in_a_destructor_ends_the_run() {
    let (out, log) = destructors(Dtor::Trap, Memory::Rc, |_, f, ids| {
        let o = new_obj(f, ids, 1);
        nil(f, o);
        mark(f, ids, 9);
        ret_log(f, ids);
    });
    match out {
        Err(VmError::Trap {
            kind: ErrorKind::Unreachable,
            func,
            ..
        }) => assert_eq!(func, FuncId(0)),
        other => panic!("{other:?}"),
    }
    assert_eq!(log, 1);
}

/// Destructor code is charged fuel like any code, and a destructor the
/// depth limit cannot hold is skipped (the object is freed all the same).
#[test]
fn destructors_cost_fuel_and_respect_the_depth_limit() {
    let (mut m, ids) = kit(Dtor::Log);
    let mut f = m.function("main", &[], &[I]);
    let o = new_obj(&mut f, ids, 1);
    nil(&mut f, o);
    f.emit(Inst::Safepoint {});
    ret_log(&mut f, ids);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut plain = Vm::with_memory(&p, Memory::Rc);
    plain.run(main, &[]).unwrap();
    assert_eq!(plain.fuel_used(), 1, "no destructor set: one safepoint");
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    vm.set_destructor("__destruct").unwrap();
    // The destructor itself has no charge point: the same fuel, and it ran.
    assert_eq!(vm.run(main, &[]), Ok(Value::Int(1)));
    assert_eq!(vm.fuel_used(), 1);
    // Depth 1: only the entry frame fits, so the destructor is skipped.
    let mut tight = Vm::with_memory(&p, Memory::Rc);
    tight.set_destructor("__destruct").unwrap();
    tight.set_limits(Limits::new().with_depth(1));
    assert_eq!(tight.run(main, &[]), Ok(Value::Int(0)));
    assert_eq!(tight.memory_stats().finalized, 0);
    let before = tight.heap_objects();
    tight.collect_garbage();
    assert_eq!(
        tight.heap_objects(),
        before,
        "the skipped object was freed already"
    );
}

/// `$o = new Obj(1); $o->other = $o; $o = null; echo 9;` keeps the cycle
/// until the cycle collector runs; then the destructor runs and the object
/// goes (PHP's `gc_collect_cycles()`).
#[test]
fn a_garbage_cycle_is_destructed_at_the_cycle_collection_point() {
    let (mut m, ids) = kit(Dtor::Log);
    let mut f = m.function("main", &[], &[I]);
    let o = new_obj(&mut f, ids, 1);
    f.emit(Inst::SetField {
        obj: o,
        field: FieldIdx(1),
        src: o,
    });
    nil(&mut f, o);
    mark(&mut f, ids, 9);
    ret_log(&mut f, ids);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    vm.set_destructor("__destruct").unwrap();
    assert_eq!(vm.run(main, &[]), Ok(Value::Int(9)));
    assert_eq!(
        global_int(&vm, ids.log),
        9,
        "a cycle outlives its last outside reference"
    );
    let live = vm.heap_objects();
    vm.collect_garbage();
    assert_eq!(
        vm.pending_finalizers(),
        1,
        "found, and kept for its destructor"
    );
    assert_eq!(vm.run_finalizers(), Ok(1));
    assert_eq!(global_int(&vm, ids.log), 91);
    vm.collect_garbage();
    assert_eq!(vm.heap_objects(), live - 1, "freed by the next collection");
    assert_eq!(vm.memory_stats().cycle_freed, 1);
}

/// A loop making 20,000 self-referencing objects fills the possible-roots
/// buffer: the cycle collector runs inside the run, at an allocation, and
/// the destructors of what it found run before the next instruction. The
/// rest goes at an explicit collection.
#[test]
fn the_cycle_collector_runs_by_itself_and_destructs_what_it_finds() {
    let (mut m, ids) = kit(Dtor::Log);
    let mut f = m.function("main", &[], &[I]);
    let (i, n, one, more) = (f.reg(I), f.reg(I), f.reg(I), f.reg(ValType::Bool));
    f.emit(Inst::LoadInt {
        dst: n,
        val: 20_000,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    let o = f.reg(D);
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::ILt {
        dst: more,
        lhs: i,
        rhs: n,
        ty: IntTy::I64,
    });
    f.jmp_if_not(more, done);
    new_into(&mut f, ids, o, 0);
    f.emit(Inst::SetField {
        obj: o,
        field: FieldIdx(1),
        src: o,
    });
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: op(),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    nil(&mut f, o);
    let c = f.reg(I);
    f.emit(Inst::GetGlobal {
        dst: c,
        global: ids.count,
    });
    f.ret(c);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    vm.set_destructor("__destruct").unwrap();
    let before = vm.heap_objects();
    let during = vm.run(main, &[]).unwrap().as_int().unwrap();
    assert!(vm.collections() >= 1, "the buffer filled");
    assert!(during >= 9_000, "destructed inside the run: {during}");
    vm.collect_garbage();
    vm.run_finalizers().unwrap();
    assert_eq!(
        global_int(&vm, ids.count),
        20_000,
        "each destructed exactly once"
    );
    vm.collect_garbage();
    assert_eq!(vm.heap_objects(), before, "and all freed");
}

// ---------------------------------------------------------------------------
// Destructors under the tracing profile
// ---------------------------------------------------------------------------

/// Tracing cannot see the last reference go: the object is destructed after
/// the collection that finds it unreachable, from the finalization queue.
#[test]
fn tracing_destructs_after_the_collection_that_finds_the_object() {
    let (out, log) = destructors(Dtor::Log, Memory::Tracing, |_, f, ids| {
        let o = new_obj(f, ids, 1);
        nil(f, o);
        mark(f, ids, 9);
        ret_log(f, ids);
    });
    assert_eq!(out, Ok(Value::Int(9)));
    assert_eq!(log, 9, "nothing collected yet");

    let (mut m, ids) = kit(Dtor::Log);
    let mut f = m.function("main", &[], &[I]);
    let o = new_obj(&mut f, ids, 1);
    nil(&mut f, o);
    ret_log(&mut f, ids);
    let main = m.add_function(f).unwrap();
    let mut g = m.function("tick", &[], &[I]);
    g.emit(Inst::Safepoint {});
    ret_log(&mut g, ids);
    let tick = m.add_function(g).unwrap();
    let p = load(m);
    let mut vm = Vm::new(&p);
    vm.set_destructor("__destruct").unwrap();
    assert_eq!(vm.run(main, &[]), Ok(Value::Int(0)));
    vm.collect_garbage();
    assert_eq!(vm.pending_finalizers(), 1);
    // The next run's first fuel charge runs it, before the safepoint.
    assert_eq!(vm.run(tick, &[]), Ok(Value::Int(1)));
    assert_eq!(vm.pending_finalizers(), 0);
    // Destructed once; the next collection frees it.
    let live = vm.heap_objects();
    vm.collect_garbage();
    assert!(vm.heap_objects() < live);
    assert_eq!(global_int(&vm, ids.count), 1);
    assert_eq!(vm.run_finalizers(), Ok(0));
}

/// `set_destructor` checks the methods it finds, finds inherited ones,
/// and an unknown name clears it.
#[test]
fn set_destructor_finds_inherited_methods_and_checks_them() {
    let (mut m, ids) = kit(Dtor::Log);
    let child = m.reserve_type();
    let (cn, idn, other, more) = (
        m.string("Child"),
        m.string("id"),
        m.string("other"),
        m.string("more"),
    );
    m.define_type(
        child,
        TypeDef::Struct(StructDef {
            name: cn,
            parent: Some(ids.obj),
            fields: vec![
                Field { name: idn, ty: I },
                Field { name: other, ty: D },
                Field { name: more, ty: D },
            ],
            methods: vec![],
        }),
    );
    let mut f = m.function("main", &[], &[I]);
    let ty = f.type_ref(child);
    let c = f.reg(D);
    f.emit(Inst::NewStruct { dst: c, ty });
    let v = f.reg(I);
    f.emit(Inst::LoadInt {
        dst: v,
        val: 7,
        ty: IntTy::I64,
    });
    f.emit(Inst::SetField {
        obj: c,
        field: FieldIdx(0),
        src: v,
    });
    nil(&mut f, c);
    ret_log(&mut f, ids);
    let main = m.add_function(f).unwrap();
    // A bad destructor on another type: two parameters.
    let bad_t = m.reserve_type();
    let mut b = m.function("Bad::__destruct", &[D, D], &[]);
    b.ret_void();
    let bad = m.add_function(b).unwrap();
    let (bn, dn) = (m.string("Bad"), m.string("__destruct"));
    m.define_type(
        bad_t,
        TypeDef::Struct(StructDef {
            name: bn,
            parent: None,
            fields: vec![],
            methods: vec![Method {
                name: dn,
                func: bad,
            }],
        }),
    );
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    assert_eq!(
        vm.set_destructor("__destruct"),
        Err(VmError::BadDestructor(bad))
    );
    assert_eq!(vm.run(main, &[]), Ok(Value::Int(0)), "nothing was set");
    assert_eq!(vm.set_destructor("no_such_method"), Ok(0));
    assert!(
        VmError::BadDestructor(bad)
            .to_string()
            .contains("destructor")
    );
}

/// The inherited destructor runs for the child (PHP calls the parent's
/// `__destruct` when the child declares none).
#[test]
fn an_inherited_destructor_runs_for_the_child() {
    let (mut m, ids) = kit(Dtor::Log);
    let child = m.reserve_type();
    let (cn, idn) = (m.string("Child"), m.string("id"));
    let (other, more) = (m.string("other"), m.string("more"));
    m.define_type(
        child,
        TypeDef::Struct(StructDef {
            name: cn,
            parent: Some(ids.obj),
            fields: vec![
                Field { name: idn, ty: I },
                Field { name: other, ty: D },
                Field { name: more, ty: D },
            ],
            methods: vec![],
        }),
    );
    let mut f = m.function("main", &[], &[I]);
    let ty = f.type_ref(child);
    let c = f.reg(D);
    f.emit(Inst::NewStruct { dst: c, ty });
    let v = f.reg(I);
    f.emit(Inst::LoadInt {
        dst: v,
        val: 7,
        ty: IntTy::I64,
    });
    f.emit(Inst::SetField {
        obj: c,
        field: FieldIdx(0),
        src: v,
    });
    nil(&mut f, c);
    ret_log(&mut f, ids);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    assert_eq!(vm.set_destructor("__destruct"), Ok(2));
    assert_eq!(vm.run(main, &[]), Ok(Value::Int(7)));
}

// ---------------------------------------------------------------------------
// Close on drop at the last reference (LSB §5.13 rule 13, rc runtimes)
// ---------------------------------------------------------------------------

/// `gen() { try { yield 1; yield 1 } finally { log 5 } }`
fn gen_with_finally(m: &mut ModuleBuilder, ids: Ids) -> FuncId {
    let mut g = m.function("gen", &[], &[]);
    let (s, e) = (g.reg(D), g.reg(D));
    let one = dint(&mut g, 1);
    let (start, end, h) = (g.label(), g.label(), g.label());
    g.bind(start);
    g.emit(Inst::Yield { dst: s, src: one });
    g.emit(Inst::Yield { dst: s, src: one });
    g.bind(end);
    g.ret_void();
    g.bind(h);
    mark(&mut g, ids, 5);
    g.emit(Inst::Throw { src: e });
    g.try_region(start, end, h, e);
    m.add_function(g).unwrap()
}

/// `$g = gen(); $g->current(); $g = null; echo 9;`: PHP destroys the
/// generator at the assignment, running its `finally` first: `59`. The
/// tracing profile closes it only after a collection finds it: `9`, then
/// `95`.
#[test]
fn a_dropped_generator_closes_at_its_last_reference() {
    for memory in [Memory::Rc, Memory::Tracing] {
        let (mut m, ids) = kit(Dtor::Log);
        let body = gen_with_finally(&mut m, ids);
        let mut f = m.function("main", &[], &[I]);
        let (c, x, sent) = (f.reg(D), f.reg(D), f.reg(D));
        f.emit(Inst::CoroNew {
            dst: c,
            func: body,
            argc: 0,
        });
        f.emit(Inst::Resume {
            dst: x,
            coro: c,
            src: sent,
        });
        nil(&mut f, c);
        mark(&mut f, ids, 9);
        ret_log(&mut f, ids);
        let main = m.add_function(f).unwrap();
        let p = load(m);
        let mut vm = Vm::with_memory(&p, memory);
        let out = vm.run(main, &[]);
        match memory {
            Memory::Rc => {
                assert_eq!(out, Ok(Value::Int(59)));
                assert_eq!(vm.memory_stats().finalized, 1);
            }
            _ => {
                assert_eq!(out, Ok(Value::Int(9)));
                vm.collect_garbage();
                assert_eq!(vm.run_finalizers(), Ok(1));
                assert_eq!(global_int(&vm, ids.log), 95);
            }
        }
    }
}

/// A generator held only by a function's local is closed when the function
/// returns, before the caller continues.
#[test]
fn a_generator_dies_with_the_frame_holding_it() {
    let (mut m, ids) = kit(Dtor::Log);
    let body = gen_with_finally(&mut m, ids);
    let mut h = m.function("h", &[], &[]);
    let (c, x, sent) = (h.reg(D), h.reg(D), h.reg(D));
    h.emit(Inst::CoroNew {
        dst: c,
        func: body,
        argc: 0,
    });
    h.emit(Inst::Resume {
        dst: x,
        coro: c,
        src: sent,
    });
    h.ret_void();
    let h = m.add_function(h).unwrap();
    let mut f = m.function("main", &[], &[I]);
    let w = f.reg(D);
    f.emit(Inst::Call {
        dst: w,
        func: h,
        argc: 0,
    });
    mark(&mut f, ids, 9);
    ret_log(&mut f, ids);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    assert_eq!(vm.run(main, &[]), Ok(Value::Int(59)));
}

// ---------------------------------------------------------------------------
// Reference unbinding on copy (LSB §5.17 "the one divergence"; P34)
// ---------------------------------------------------------------------------

/// Runs `body` (returning an array) under a profile; the elements.
fn elements(
    memory: Memory,
    body: impl FnOnce(&mut ModuleBuilder, &mut FunctionBuilder),
) -> (Vec<Value>, u64) {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("main", &[], &[D]);
    body(&mut m, &mut f);
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, memory);
    let v = vm.run(main, &[]).unwrap();
    let unbound = vm.memory_stats().unbound;
    (vm.elements(v).unwrap(), unbound)
}

fn ints(v: &[Value]) -> Vec<i64> {
    v.iter().map(|x| x.as_int().unwrap_or(-1)).collect()
}

/// `$a = [1, 2]; $r = &$a[0]; unset($r); $b = $a; $b[0] = 9;` PHP drops
/// the reference when copying (nothing else holds it): `$a[0]` stays 1.
/// LSB keeps it under tracing (the documented divergence): both are 9.
#[test]
fn copying_an_array_unbinds_a_reference_nothing_else_holds() {
    let body = |keep_r: bool| {
        move |m: &mut ModuleBuilder, f: &mut FunctionBuilder| {
            let a = new_arr(m, f);
            let (one, two, zero, nine) = (dint(f, 1), dint(f, 2), dint(f, 0), dint(f, 9));
            f.emit(Inst::ArrayPush { arr: a, src: one });
            f.emit(Inst::ArrayPush { arr: a, src: two });
            let r = f.reg(D);
            f.emit(Inst::DRefIndex {
                dst: r,
                obj: a,
                key: zero,
            });
            if !keep_r {
                nil(f, r);
            }
            let b = f.reg(D);
            f.emit(Inst::Dup { dst: b, src: a });
            f.emit(Inst::DSetIndex {
                obj: b,
                key: zero,
                src: nine,
            });
            let out = new_arr(m, f);
            for c in [a, b] {
                let x = f.reg(D);
                f.emit(Inst::DGetIndex {
                    dst: x,
                    obj: c,
                    key: zero,
                });
                f.emit(Inst::ArrayPush { arr: out, src: x });
            }
            f.ret(out);
        }
    };
    let (rc, unbound) = elements(Memory::Rc, body(false));
    assert_eq!(ints(&rc), [1, 9]);
    assert_eq!(unbound, 1);
    let (gc, unbound) = elements(Memory::Tracing, body(false));
    assert_eq!(ints(&gc), [9, 9]);
    assert_eq!(unbound, 0);
    // With `$r` still bound the reference is shared by both copies, in
    // both profiles (and in PHP).
    for memory in [Memory::Rc, Memory::Tracing] {
        let (v, unbound) = elements(memory, body(true));
        assert_eq!(ints(&v), [9, 9]);
        assert_eq!(unbound, 0);
    }
}

/// The classic: `$a = [1, 2, 3]; foreach ($a as &$v) {} $b = $a; $b[0] =
/// 7; $b[2] = 9;` leaves `$a` as `[1, 2, 9]` in PHP: the last element is
/// still bound to `$v`, the others were unbound by the copy. Tracing (no
/// `dunref`) gives `[7, 2, 9]`.
#[test]
fn foreach_by_reference_then_copy_matches_php() {
    let body = |m: &mut ModuleBuilder, f: &mut FunctionBuilder| {
        let a = new_arr(m, f);
        for i in 1..=3 {
            let x = dint(f, i);
            f.emit(Inst::ArrayPush { arr: a, src: x });
        }
        let v = f.reg(D);
        for i in 0..3 {
            let k = dint(f, i);
            f.emit(Inst::DRefIndex {
                dst: v,
                obj: a,
                key: k,
            });
        }
        let b = f.reg(D);
        f.emit(Inst::Dup { dst: b, src: a });
        let (zero, two, seven, nine) = (dint(f, 0), dint(f, 2), dint(f, 7), dint(f, 9));
        f.emit(Inst::DSetIndex {
            obj: b,
            key: zero,
            src: seven,
        });
        f.emit(Inst::DSetIndex {
            obj: b,
            key: two,
            src: nine,
        });
        f.ret(a);
    };
    let (rc, unbound) = elements(Memory::Rc, body);
    assert_eq!(ints(&rc), [1, 2, 9]);
    assert_eq!(unbound, 2);
    let (gc, _) = elements(Memory::Tracing, body);
    assert_eq!(ints(&gc), [7, 2, 9]);
}

/// The same rule for a PHP array that is a map, and for `clone` of an
/// object (`zval_add_ref` unbinds a refcount-1 reference there too).
#[test]
fn maps_and_object_clones_unbind_too() {
    let map_body = |m: &mut ModuleBuilder, f: &mut FunctionBuilder| {
        let mt = m.add_type(TypeDef::Map { key: D, value: D });
        let ty = f.type_ref(mt);
        let a = f.reg(D);
        f.emit(Inst::NewMap { dst: a, ty });
        let (k, one, nine) = (dint(f, 5), dint(f, 1), dint(f, 9));
        f.emit(Inst::DSetIndex {
            obj: a,
            key: k,
            src: one,
        });
        let r = f.reg(D);
        f.emit(Inst::DRefIndex {
            dst: r,
            obj: a,
            key: k,
        });
        nil(f, r);
        let b = f.reg(D);
        f.emit(Inst::Dup { dst: b, src: a });
        f.emit(Inst::DSetIndex {
            obj: b,
            key: k,
            src: nine,
        });
        let out = new_arr(m, f);
        for c in [a, b] {
            let x = f.reg(D);
            f.emit(Inst::DGetIndex {
                dst: x,
                obj: c,
                key: k,
            });
            f.emit(Inst::ArrayPush { arr: out, src: x });
        }
        f.ret(out);
    };
    assert_eq!(ints(&elements(Memory::Rc, map_body).0), [1, 9]);
    assert_eq!(ints(&elements(Memory::Tracing, map_body).0), [9, 9]);

    let clone_body = |m: &mut ModuleBuilder, f: &mut FunctionBuilder| {
        let (bn, vn) = (m.string("Box"), m.string("v"));
        let t = m.add_type(TypeDef::Struct(StructDef {
            name: bn,
            parent: None,
            fields: vec![Field { name: vn, ty: D }],
            methods: vec![],
        }));
        let ty = f.type_ref(t);
        let o = f.reg(D);
        f.emit(Inst::NewStruct { dst: o, ty });
        let one = dint(f, 1);
        f.emit(Inst::SetField {
            obj: o,
            field: FieldIdx(0),
            src: one,
        });
        let vname = m.string("v");
        let name = f.name_ref(vname);
        let r = f.reg(D);
        f.emit(Inst::DRefProp {
            dst: r,
            obj: o,
            name,
        });
        nil(f, r);
        let c = f.reg(D);
        f.emit(Inst::Dup { dst: c, src: o });
        let nine = dint(f, 9);
        f.emit(Inst::SetField {
            obj: c,
            field: FieldIdx(0),
            src: nine,
        });
        let out = new_arr(m, f);
        for x in [o, c] {
            let v = f.reg(D);
            f.emit(Inst::GetField {
                dst: v,
                obj: x,
                field: FieldIdx(0),
            });
            f.emit(Inst::ArrayPush { arr: out, src: v });
        }
        f.ret(out);
    };
    let (rc, unbound) = elements(Memory::Rc, clone_body);
    assert_eq!(ints(&rc), [1, 9]);
    assert_eq!(unbound, 1);
    assert_eq!(ints(&elements(Memory::Tracing, clone_body).0), [9, 9]);
}

// ---------------------------------------------------------------------------
// The cycle collector: shared storage, fuel, hostile sizes
// ---------------------------------------------------------------------------

/// A cycle that runs through array storage shared by two containers (a
/// `dup` not yet written): `$o->other = $a; $o->more = $b = $a;` with `$a =
/// [$o]`. The storage is a node of its own, so the cycle is found.
#[test]
fn a_cycle_through_shared_storage_is_collected() {
    let mut m = ModuleBuilder::new();
    let (_, ids) = (0, kit(Dtor::Log).1);
    let obj = m.reserve_type();
    let (name, idn, other, more) = (
        m.string("Obj"),
        m.string("id"),
        m.string("other"),
        m.string("more"),
    );
    m.define_type(
        obj,
        TypeDef::Struct(StructDef {
            name,
            parent: None,
            fields: vec![
                Field { name: idn, ty: I },
                Field { name: other, ty: D },
                Field { name: more, ty: D },
            ],
            methods: vec![],
        }),
    );
    let ids = Ids { obj, ..ids };
    let mut f = m.function("main", &[], &[]);
    let o = new_obj(&mut f, ids, 1);
    let a = new_arr(&mut m, &mut f);
    f.emit(Inst::ArrayPush { arr: a, src: o });
    let b = f.reg(D);
    f.emit(Inst::Dup { dst: b, src: a });
    f.emit(Inst::SetField {
        obj: o,
        field: FieldIdx(1),
        src: a,
    });
    f.emit(Inst::SetField {
        obj: o,
        field: FieldIdx(2),
        src: b,
    });
    f.ret_void();
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    let before = vm.heap_objects();
    vm.run(main, &[]).unwrap();
    assert_eq!(vm.heap_objects(), before + 3, "o, a, b: a garbage cycle");
    vm.collect_garbage();
    assert_eq!(vm.heap_objects(), before);
    assert_eq!(vm.memory_stats().cycle_freed, 3);
}

/// Builds `main(n)`: a ring (or with `ring = false` a chain) of `n` objects
/// linked through `other`, then dropped.
fn ring_program(n: i32, ring: bool) -> (Program, FuncId) {
    let mut m = ModuleBuilder::new();
    let (nn, next) = (m.string("Node"), m.string("next"));
    let obj = m.add_type(TypeDef::Struct(StructDef {
        name: nn,
        parent: None,
        fields: vec![Field { name: next, ty: D }],
        methods: vec![],
    }));
    let mut f = m.function("main", &[], &[]);
    let ty = f.type_ref(obj);
    let (first, prev, node) = (f.reg(D), f.reg(D), f.reg(D));
    let (i, count, one, more) = (f.reg(I), f.reg(I), f.reg(I), f.reg(ValType::Bool));
    f.emit(Inst::LoadInt {
        dst: count,
        val: n,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewStruct { dst: first, ty });
    f.emit(Inst::Mov {
        dst: prev,
        src: first,
    });
    f.emit(Inst::LoadInt {
        dst: i,
        val: 1,
        ty: IntTy::I64,
    });
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::ILt {
        dst: more,
        lhs: i,
        rhs: count,
        ty: IntTy::I64,
    });
    f.jmp_if_not(more, done);
    f.emit(Inst::NewStruct { dst: node, ty });
    f.emit(Inst::SetField {
        obj: prev,
        field: FieldIdx(0),
        src: node,
    });
    f.emit(Inst::Mov {
        dst: prev,
        src: node,
    });
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: op(),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    if ring {
        f.emit(Inst::SetField {
            obj: prev,
            field: FieldIdx(0),
            src: first,
        });
    }
    f.emit(Inst::LoadNil { dst: first });
    f.emit(Inst::LoadNil { dst: prev });
    f.emit(Inst::LoadNil { dst: node });
    f.ret_void();
    let main = m.add_function(f).unwrap();
    (load(m), main)
}

/// A million-node garbage ring: trial deletion walks it with work lists
/// (no recursion), and frees all of it.
#[test]
fn a_million_node_cycle_is_collected_iteratively() {
    let (p, main) = ring_program(1_000_000, true);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    vm.set_limits(Limits::new().with_memory(1 << 30));
    let before = vm.heap_objects();
    vm.run(main, &[]).unwrap();
    assert_eq!(
        vm.heap_objects(),
        before + 1_000_000,
        "a ring survives counting"
    );
    vm.collect_garbage();
    assert_eq!(vm.heap_objects(), before);
    assert_eq!(vm.memory_stats().cycle_freed, 1_000_000);
}

/// A million-node chain dies at once when its head goes: freeing is
/// iterative too (the zero list is the work list).
#[test]
fn a_million_node_chain_is_freed_iteratively_at_its_last_reference() {
    let (p, main) = ring_program(1_000_000, false);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    let before = vm.heap_objects();
    vm.run(main, &[]).unwrap();
    assert_eq!(vm.heap_objects(), before, "freed by counting");
    let stats = vm.memory_stats();
    assert_eq!(stats.freed, 1_000_000);
    // Building the chain buffered possible roots (each node lost its
    // register holder), so the collector ran a few times; it found nothing.
    assert_eq!(stats.cycle_freed, 0);
}

/// The cycle collector's work is charged against the run's fuel: the same
/// program costs more under RC, and a budget that covers the tracing run
/// exactly ends the RC run with `OutOfFuel`.
#[test]
fn cycle_collection_is_charged_fuel() {
    let (mut m, ids) = kit(Dtor::Log);
    let mut f = m.function("main", &[], &[]);
    let (i, n, one, more) = (f.reg(I), f.reg(I), f.reg(I), f.reg(ValType::Bool));
    f.emit(Inst::LoadInt {
        dst: n,
        val: 15_000,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    let o = f.reg(D);
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::ILt {
        dst: more,
        lhs: i,
        rhs: n,
        ty: IntTy::I64,
    });
    f.jmp_if_not(more, done);
    new_into(&mut f, ids, o, 0);
    f.emit(Inst::SetField {
        obj: o,
        field: FieldIdx(1),
        src: o,
    });
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: op(),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    f.ret_void();
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut gc = Vm::new(&p);
    gc.run(main, &[]).unwrap();
    let mut rc = Vm::with_memory(&p, Memory::Rc);
    rc.run(main, &[]).unwrap();
    assert!(rc.collections() >= 1);
    assert!(
        rc.fuel_used() > gc.fuel_used(),
        "{} vs {}",
        rc.fuel_used(),
        gc.fuel_used()
    );
    let mut tight = Vm::with_memory(&p, Memory::Rc);
    let out = tight.run_with(main, &[], Limits::new().with_fuel(gc.fuel_used()));
    assert!(
        matches!(
            out,
            Err(VmError::Trap {
                kind: ErrorKind::OutOfFuel,
                ..
            })
        ),
        "{out:?}"
    );
}

/// Values a run hands the host stay valid until the next run (as under
/// tracing), and are let go then.
#[test]
fn results_are_held_for_the_host_until_the_next_run() {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("make", &[], &[D]);
    let a = new_arr(&mut m, &mut f);
    let x = dint(&mut f, 42);
    f.emit(Inst::ArrayPush { arr: a, src: x });
    f.ret(a);
    let make = m.add_function(f).unwrap();
    let mut g = m.function("noop", &[], &[]);
    g.ret_void();
    let noop = m.add_function(g).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    let base = vm.heap_objects();
    let v = vm.run(make, &[]).unwrap();
    assert_eq!(vm.elements(v), Some(vec![Value::Int(42)]));
    assert_eq!(vm.heap_objects(), base + 1);
    vm.run(noop, &[]).unwrap();
    assert_eq!(vm.heap_objects(), base, "let go at the next run");
    assert_eq!(vm.elements(v), None, "a stale handle reads as nothing");
    let s = vm.new_str(b"kept").unwrap();
    vm.collect_garbage();
    assert_eq!(vm.str_bytes(s), None, "collect_garbage lets go too");
}

/// Under a 2 MiB budget: a loop that `dup`s a 100 KiB array 1,000 times
/// (each copy charged its share of storage it does not own) and a loop that
/// makes and drops 100,000 strings both run to the end under RC: freed
/// memory is refunded at once, and the estimate's drift is recounted away
/// before it can trap.
#[test]
fn the_memory_budget_follows_what_counting_frees() {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("main", &[], &[]);
    let at = m.add_type(TypeDef::Array(D));
    let ty = f.type_ref(at);
    let (len, a, b) = (f.reg(I), f.reg(D), f.reg(D));
    f.emit(Inst::LoadInt {
        dst: len,
        val: 12_800,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewArray { dst: a, len, ty });
    let (i, n, one, more) = (f.reg(I), f.reg(I), f.reg(I), f.reg(ValType::Bool));
    f.emit(Inst::LoadInt {
        dst: n,
        val: 1_000,
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
        dst: more,
        lhs: i,
        rhs: n,
        ty: IntTy::I64,
    });
    f.jmp_if_not(more, done);
    f.emit(Inst::Dup { dst: b, src: a });
    let s = f.reg(D);
    let k = m.constant(bytecode_lang::Const::Bytes(vec![b'x'; 100]));
    f.emit(Inst::DLoadConst { dst: s, k });
    for _ in 0..100 {
        f.emit(Inst::DConcat {
            dst: b,
            lhs: s,
            rhs: s,
        });
    }
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op: op(),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    f.ret_void();
    let main = m.add_function(f).unwrap();
    let p = load(m);
    let mut vm = Vm::with_memory(&p, Memory::Rc);
    vm.set_limits(Limits::new().with_memory(2 << 20));
    assert_eq!(vm.run(main, &[]), Ok(Value::Nil));
    // Two units per pass, and 400 for the 12,800-element array (one per
    // 256 bytes built).
    assert_eq!(vm.fuel_used(), 2_400, "every pass ran");
    assert!(vm.heap_bytes() < 2 << 20);
    assert!(vm.memory_stats().freed >= 100_000);
}
