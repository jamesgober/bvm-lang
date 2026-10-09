//! Garbage collection: everything reachable from registers, globals,
//! closures, cells, containers, and iterators survives; unreachable objects,
//! cycles included, are reclaimed; handles to collected objects read as
//! `nil`; the heap stays bounded in allocation-heavy loops.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

mod common;

use bvm_lang::{Host, Limits, Program, Value, Vm};
use bytecode_lang::{
    Const, FuncId, Inst, IntOp, IntTy, Kind, ModuleBuilder, Reg, TypeDef, ValType,
};
use common::{BOOL, D, I64};

/// A loop of `n` iterations; each builds a 3-element dyn array referencing
/// the previous one (a long chain kept alive through a register) and a
/// throwaway array; returns the chain.
fn chain_program(n: i32) -> Program {
    common::load(common::module(&[], &[D], move |m, f| {
        let at = m.add_type(TypeDef::Array(D));
        let tr = f.type_ref(at);
        let (head, tmp, len, i, one, lim, c) = (
            f.reg(D),
            f.reg(D),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
            f.reg(BOOL),
        );
        let op = IntOp::new(IntTy::I64);
        f.emit(Inst::LoadInt {
            dst: len,
            val: 3,
            ty: IntTy::I64,
        });
        f.emit(Inst::LoadInt {
            dst: one,
            val: 1,
            ty: IntTy::I64,
        });
        f.emit(Inst::LoadInt {
            dst: lim,
            val: n,
            ty: IntTy::I64,
        });
        let (top, done) = (f.label(), f.label());
        f.bind(top);
        f.emit(Inst::ILt {
            dst: c,
            lhs: i,
            rhs: lim,
            ty: IntTy::I64,
        });
        f.jmp_if_not(c, done);
        f.emit(Inst::NewArray {
            dst: tmp,
            len,
            ty: tr,
        });
        f.emit(Inst::ArrayPush {
            arr: tmp,
            src: head,
        }); // tmp -> previous head
        f.emit(Inst::Mov {
            dst: head,
            src: tmp,
        });
        f.emit(Inst::NewArray {
            dst: tmp,
            len,
            ty: tr,
        }); // garbage
        f.emit(Inst::IAdd {
            dst: i,
            lhs: i,
            rhs: one,
            op,
        });
        f.emit(Inst::Safepoint {});
        f.jmp(top);
        f.bind(done);
        f.ret(head);
    }))
}

#[test]
fn reachable_chains_survive_and_garbage_is_freed() {
    let p = chain_program(50_000);
    let mut vm = Vm::with_limits(&p, Limits::new().with_memory(64 << 20));
    let head = vm.run(FuncId(0), &[]).unwrap();
    assert!(vm.collections() > 0, "the loop allocated enough to collect");
    // Walk the chain: 50,000 links, each array [nil, nil, nil, prev].
    let mut node = head;
    let mut links = 0;
    while let Some(items) = vm.elements(node) {
        assert_eq!(items.len(), 4);
        node = items[3];
        links += 1;
    }
    assert_eq!(links, 50_000);
    // After an explicit collection with no roots left, everything goes.
    vm.collect_garbage();
    assert_eq!(vm.heap_objects(), 0);
    assert_eq!(vm.kind(head), Kind::Nil, "a collected handle reads as nil");
}

#[test]
fn heap_stays_bounded_in_an_allocation_loop() {
    // 200k iterations each allocating garbage under a 2 MiB budget.
    let p = common::load(common::module(&[], &[I64], |m, f| {
        let at = m.add_type(TypeDef::Array(I64));
        let tr = f.type_ref(at);
        let (a, len, i, one, lim, c) = (
            f.reg(ValType::Ref(at)),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
            f.reg(BOOL),
        );
        let op = IntOp::new(IntTy::I64);
        f.emit(Inst::LoadInt {
            dst: len,
            val: 16,
            ty: IntTy::I64,
        });
        f.emit(Inst::LoadInt {
            dst: one,
            val: 1,
            ty: IntTy::I64,
        });
        f.emit(Inst::LoadInt {
            dst: lim,
            val: 200_000,
            ty: IntTy::I64,
        });
        let (top, done) = (f.label(), f.label());
        f.bind(top);
        f.emit(Inst::ILt {
            dst: c,
            lhs: i,
            rhs: lim,
            ty: IntTy::I64,
        });
        f.jmp_if_not(c, done);
        f.emit(Inst::NewArray {
            dst: a,
            len,
            ty: tr,
        });
        f.emit(Inst::IAdd {
            dst: i,
            lhs: i,
            rhs: one,
            op,
        });
        f.jmp(top);
        f.bind(done);
        f.ret(i);
    }));
    let mut vm = Vm::with_limits(&p, Limits::new().with_memory(2 << 20));
    assert_eq!(vm.run(FuncId(0), &[]), Ok(Value::Int(200_000)));
    assert!(vm.heap_bytes() <= 2 << 20);
}

#[test]
fn cycles_are_collected() {
    // a = [b], b = [a], both dropped.
    let p = common::load(common::module(&[], &[], |m, f| {
        let at = m.add_type(TypeDef::Array(D));
        let tr = f.type_ref(at);
        let (a, b, n) = (f.reg(D), f.reg(D), f.reg(I64));
        f.emit(Inst::NewArray {
            dst: a,
            len: n,
            ty: tr,
        });
        f.emit(Inst::NewArray {
            dst: b,
            len: n,
            ty: tr,
        });
        f.emit(Inst::ArrayPush { arr: a, src: b });
        f.emit(Inst::ArrayPush { arr: b, src: a });
        f.ret_void();
    }));
    let mut vm = Vm::new(&p);
    vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(vm.heap_objects(), 2);
    vm.collect_garbage();
    assert_eq!(vm.heap_objects(), 0);
}

#[test]
fn globals_closures_cells_maps_and_iterators_are_roots() {
    // Build: global g = map { "k": cell(closure capturing array [str]) },
    // plus an iterator stored in the map; collect; everything stays.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(D));
    let cell_t = m.add_type(TypeDef::Cell(D));
    let map_t = m.add_type(TypeDef::Map { key: D, value: D });
    let g = m.global("g", D, true, None);
    let text = m.constant(Const::Bytes(b"payload".to_vec()));
    let key = m.constant(Const::Bytes(b"k".to_vec()));
    let mut inner = m.function("inner", &[], &[D]);
    let cap = inner.capture(D);
    let r = inner.reg(D);
    inner.emit(Inst::GetUpval { dst: r, idx: cap });
    inner.ret(r);
    let iid = inner.id();
    m.add_function(inner).unwrap();
    let mut f = m.function("build", &[], &[]);
    let (atr, ctr, mtr) = (f.type_ref(at), f.type_ref(cell_t), f.type_ref(map_t));
    let (arr, s, n, cell, map, k, it) = (
        f.reg(D),
        f.reg(D),
        f.reg(I64),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
    );
    let clo = f.regs(&[D, D]);
    f.emit(Inst::NewArray {
        dst: arr,
        len: n,
        ty: atr,
    });
    f.emit(Inst::DLoadConst { dst: s, k: text });
    f.emit(Inst::ArrayPush { arr, src: s });
    f.emit(Inst::Mov {
        dst: Reg(clo.0 + 1),
        src: arr,
    });
    f.emit(Inst::MakeClosure {
        dst: clo,
        func: iid,
    });
    f.emit(Inst::NewCell {
        dst: cell,
        src: clo,
        ty: ctr,
    });
    f.emit(Inst::NewMap { dst: map, ty: mtr });
    f.emit(Inst::DLoadConst { dst: k, k: key });
    f.emit(Inst::MapSet {
        map,
        key: k,
        src: cell,
    });
    f.emit(Inst::DIterNew { dst: it, src: arr });
    f.emit(Inst::MapPush { map, src: it });
    f.emit(Inst::SetGlobal {
        global: g,
        src: map,
    });
    f.ret_void();
    let build = m.add_function(f).unwrap();
    // read: g["k"] -> cell -> closure() -> array[0] -> "payload"
    let mut rd = m.function("read", &[], &[D]);
    let (map, k, cell, zero, out) = (rd.reg(D), rd.reg(D), rd.reg(D), rd.reg(I64), rd.reg(D));
    let win = rd.regs(&[D]);
    rd.emit(Inst::GetGlobal {
        dst: map,
        global: g,
    });
    rd.emit(Inst::DLoadConst { dst: k, k: key });
    rd.emit(Inst::MapGet {
        dst: cell,
        map,
        key: k,
    });
    rd.emit(Inst::CellGet { dst: cell, cell });
    rd.emit(Inst::CallIndirect {
        dst: win,
        callee: cell,
        argc: 0,
    });
    rd.emit(Inst::ArrayGet {
        dst: out,
        arr: win,
        idx: zero,
    });
    rd.ret(out);
    let read = m.add_function(rd).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    vm.run(build, &[]).unwrap();
    let before = vm.heap_objects();
    vm.collect_garbage();
    vm.collect_garbage();
    assert_eq!(vm.heap_objects(), before, "nothing reachable was freed");
    let s = vm.run(read, &[]).unwrap();
    assert_eq!(vm.str_bytes(s), Some(&b"payload"[..]));
    let _ = Host::new();
}

#[test]
fn values_crossing_runs_are_valid_until_collected() {
    let p = chain_program(3);
    let mut vm = Vm::new(&p);
    let head = vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(vm.kind(head), Kind::Array);
    // Pass it back in as an argument to a run: still valid during the run.
    let mut m = ModuleBuilder::new();
    let mut f = m.function("id", &[D], &[D]);
    f.ret(Reg(0));
    m.add_function(f).unwrap();
    let p2 = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm2 = Vm::new(&p2);
    let s = vm2.new_str(b"x").unwrap();
    assert_eq!(vm2.run(FuncId(0), &[s]), Ok(s));
}
