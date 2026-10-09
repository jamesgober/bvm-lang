//! LSB conformance, heap instructions (LSB §5.10–§5.11): closures, cells,
//! structs, arrays, maps, iterators, `dup`, and strings.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

mod common;

use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{
    Const, ErrorKind, Field, FieldIdx, FuncId, Inst, IntOp, IntTy, ModuleBuilder, Reg, StructDef,
    TypeDef, TypeId, ValType,
};
use common::{BOOL, D, I64, STR};

fn op() -> IntOp {
    IntOp::new(IntTy::I64)
}

fn raised(kind: ErrorKind, pc: u32) -> Result<Value, VmError> {
    Err(VmError::Raised {
        kind,
        payload: Value::Nil,
        func: FuncId(0),
        pc,
    })
}

/// Loads a module whose function 0 is `main` with the given signature; the
/// body gets the module builder for types and constants.
fn program(
    results: &[ValType],
    body: impl FnOnce(&mut ModuleBuilder, &mut bytecode_lang::FunctionBuilder),
) -> Program {
    common::load(common::module(&[], results, body))
}

#[test]
fn op_make_closure_captures_by_value_and_get_upval() {
    let mut m = ModuleBuilder::new();
    let mut inner = m.function("inner", &[], &[I64]);
    let c = inner.capture(I64);
    let r = inner.reg(I64);
    inner.emit(Inst::GetUpval { dst: r, idx: c });
    inner.ret(r);
    let iid = inner.id();
    let fn_t = m.func_type(&[], &[I64]);
    let mut main = m.function("main", &[], &[I64]);
    let clo = main.regs(&[ValType::Ref(fn_t), I64]);
    let win = main.regs(&[I64]);
    main.emit(Inst::LoadInt {
        dst: Reg(clo.0 + 1),
        val: 7,
        ty: IntTy::I64,
    });
    main.emit(Inst::MakeClosure {
        dst: clo,
        func: iid,
    });
    // Changing the register after capture does not change the closure.
    main.emit(Inst::LoadInt {
        dst: Reg(clo.0 + 1),
        val: 8,
        ty: IntTy::I64,
    });
    main.emit(Inst::CallIndirect {
        dst: win,
        callee: clo,
        argc: 0,
    });
    main.ret(win);
    m.add_function(inner).unwrap();
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Int(7)));
}

#[test]
fn op_new_cell_cell_get_cell_set_share_state_between_closures() {
    // A counter closure over a cell: two calls return 1 then 2.
    let mut m = ModuleBuilder::new();
    let cell_t = m.add_type(TypeDef::Cell(I64));
    let mut inc = m.function("inc", &[], &[I64]);
    let c = inc.capture(ValType::Ref(cell_t));
    let (cr, v, one) = (inc.reg(ValType::Ref(cell_t)), inc.reg(I64), inc.reg(I64));
    inc.emit(Inst::GetUpval { dst: cr, idx: c });
    inc.emit(Inst::CellGet { dst: v, cell: cr });
    inc.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    inc.emit(Inst::IAdd {
        dst: v,
        lhs: v,
        rhs: one,
        op: op(),
    });
    inc.emit(Inst::CellSet { cell: cr, src: v });
    inc.ret(v);
    let iid = inc.id();
    let fn_t = m.func_type(&[], &[I64]);
    let mut main = m.function("main", &[], &[I64]);
    let ct = main.type_ref(cell_t);
    let zero = main.reg(I64);
    let clo = main.regs(&[ValType::Ref(fn_t), ValType::Ref(cell_t)]);
    let (w1, w2) = (main.regs(&[I64]), main.regs(&[I64]));
    let sum = main.reg(I64);
    main.emit(Inst::NewCell {
        dst: Reg(clo.0 + 1),
        src: zero,
        ty: ct,
    });
    main.emit(Inst::MakeClosure {
        dst: clo,
        func: iid,
    });
    main.emit(Inst::CallIndirect {
        dst: w1,
        callee: clo,
        argc: 0,
    });
    main.emit(Inst::CallIndirect {
        dst: w2,
        callee: clo,
        argc: 0,
    });
    // 1 * 10 + 2
    let ten = main.reg(I64);
    main.emit(Inst::LoadInt {
        dst: ten,
        val: 10,
        ty: IntTy::I64,
    });
    main.emit(Inst::IMul {
        dst: sum,
        lhs: w1,
        rhs: ten,
        op: op(),
    });
    main.emit(Inst::IAdd {
        dst: sum,
        lhs: sum,
        rhs: w2,
        op: op(),
    });
    main.ret(sum);
    m.add_function(inc).unwrap();
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).run(id, &[]), Ok(Value::Int(12)));
}

#[test]
fn cell_on_nil_is_null_reference() {
    let mut m = ModuleBuilder::new();
    let cell_t = m.add_type(TypeDef::Cell(I64));
    let mut main = m.function("main", &[], &[I64]);
    let (c, v) = (main.reg(ValType::Ref(cell_t)), main.reg(I64));
    main.emit(Inst::CellGet { dst: v, cell: c });
    main.ret(v);
    m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[]),
        raised(ErrorKind::NullReference, 0)
    );
}

fn point(m: &mut ModuleBuilder) -> TypeId {
    let (x, y, name) = (m.string("x"), m.string("y"), m.string("Point"));
    m.add_type(TypeDef::Struct(StructDef {
        name,
        parent: None,
        fields: vec![Field { name: x, ty: I64 }, Field { name: y, ty: STR }],
        methods: Vec::new(),
    }))
}

#[test]
fn op_new_struct_get_field_set_field() {
    let mut m = ModuleBuilder::new();
    let pt = point(&mut m);
    let mut main = m.function("main", &[], &[ValType::Ref(pt)]);
    let t = main.type_ref(pt);
    let (o, v, d) = (main.reg(ValType::Ref(pt)), main.reg(I64), main.reg(I64));
    main.emit(Inst::NewStruct { dst: o, ty: t });
    main.emit(Inst::GetField {
        dst: d,
        obj: o,
        field: FieldIdx(0),
    }); // default 0
    main.emit(Inst::LoadInt {
        dst: v,
        val: 3,
        ty: IntTy::I64,
    });
    main.emit(Inst::IAdd {
        dst: v,
        lhs: v,
        rhs: d,
        op: op(),
    });
    main.emit(Inst::SetField {
        obj: o,
        field: FieldIdx(0),
        src: v,
    });
    main.ret(o);
    let id = m.add_function(main).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    let obj = vm.run(id, &[]).unwrap();
    assert_eq!(vm.field(obj, 0), Some(Value::Int(3)));
    assert_eq!(
        vm.field(obj, 1),
        Some(Value::Nil),
        "str field defaults to nil"
    );
}

#[test]
fn get_field_on_nil_and_out_of_range_slot() {
    let mut m = ModuleBuilder::new();
    let pt = point(&mut m);
    let mut main = m.function("main", &[], &[I64]);
    let t = main.type_ref(pt);
    let (o, v) = (main.reg(ValType::Ref(pt)), main.reg(I64));
    main.emit(Inst::GetField {
        dst: v,
        obj: o,
        field: FieldIdx(0),
    });
    main.ret(v);
    m.add_function(main).unwrap();
    let mut main2 = m.function("main2", &[], &[I64]);
    let (o2, v2) = (main2.reg(ValType::Ref(pt)), main2.reg(I64));
    let t2 = main2.type_ref(pt);
    main2.emit(Inst::NewStruct { dst: o2, ty: t2 });
    main2.emit(Inst::GetField {
        dst: v2,
        obj: o2,
        field: FieldIdx(9),
    });
    main2.ret(v2);
    let id2 = m.add_function(main2).unwrap();
    let _ = t;
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    assert_eq!(vm.run(FuncId(0), &[]), raised(ErrorKind::NullReference, 0));
    assert_eq!(
        vm.run(id2, &[]),
        Err(VmError::Raised {
            kind: ErrorKind::TypeError,
            payload: bvm_lang::Value::Nil,
            func: id2,
            pc: 1
        })
    );
}

/// `main() -> ref array i64` over a fresh array type.
fn array_main(
    body: impl FnOnce(&mut bytecode_lang::FunctionBuilder, TypeId, bytecode_lang::TypeRef),
) -> Program {
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(I64));
    let mut main = m.function("main", &[], &[ValType::Ref(at)]);
    let tr = main.type_ref(at);
    body(&mut main, at, tr);
    m.add_function(main).unwrap();
    Program::load(m.finish().unwrap(), &Host::new()).unwrap()
}

#[test]
fn op_array_instructions() {
    let p = array_main(|f, at, tr| {
        let (a, n, i, v, len, popped) = (
            f.reg(ValType::Ref(at)),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
        );
        f.emit(Inst::LoadInt {
            dst: n,
            val: 3,
            ty: IntTy::I64,
        });
        f.emit(Inst::NewArray {
            dst: a,
            len: n,
            ty: tr,
        }); // [0, 0, 0]
        f.emit(Inst::LoadInt {
            dst: i,
            val: 1,
            ty: IntTy::I64,
        });
        f.emit(Inst::LoadInt {
            dst: v,
            val: 5,
            ty: IntTy::I64,
        });
        f.emit(Inst::ArraySet {
            arr: a,
            idx: i,
            src: v,
        }); // [0, 5, 0]
        f.emit(Inst::ArrayPush { arr: a, src: v }); // [0, 5, 0, 5]
        f.emit(Inst::ArrayPop {
            dst: popped,
            arr: a,
        }); // [0, 5, 0]
        f.emit(Inst::ArrayLen { dst: len, arr: a }); // 3
        f.emit(Inst::ArrayGet {
            dst: v,
            arr: a,
            idx: i,
        }); // 5
        f.emit(Inst::IAdd {
            dst: v,
            lhs: v,
            rhs: len,
            op: op(),
        }); // 8
        f.emit(Inst::IAdd {
            dst: v,
            lhs: v,
            rhs: popped,
            op: op(),
        }); // 13
        f.emit(Inst::ArrayPush { arr: a, src: v }); // [0, 5, 0, 13]
        f.ret(a);
    });
    let mut vm = Vm::new(&p);
    let a = vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(
        vm.elements(a),
        Some(vec![
            Value::Int(0),
            Value::Int(5),
            Value::Int(0),
            Value::Int(13)
        ])
    );
}

#[test]
fn array_bounds_and_nil() {
    let run = |idx: i32,
               setup: fn(
        &mut bytecode_lang::FunctionBuilder,
        Reg,
        Reg,
        Reg,
        bytecode_lang::TypeRef,
    )| {
        let p = array_main(move |f, at, tr| {
            let (a, n, i) = (f.reg(ValType::Ref(at)), f.reg(I64), f.reg(I64));
            f.emit(Inst::LoadInt {
                dst: n,
                val: 2,
                ty: IntTy::I64,
            });
            f.emit(Inst::LoadInt {
                dst: i,
                val: idx,
                ty: IntTy::I64,
            });
            setup(f, a, n, i, tr);
            f.ret(a);
        });
        Vm::new(&p).run(FuncId(0), &[])
    };
    let get: fn(&mut bytecode_lang::FunctionBuilder, Reg, Reg, Reg, bytecode_lang::TypeRef) =
        |f, a, n, i, tr| {
            f.emit(Inst::NewArray {
                dst: a,
                len: n,
                ty: tr,
            });
            f.emit(Inst::ArrayGet {
                dst: n,
                arr: a,
                idx: i,
            });
        };
    let set: fn(&mut bytecode_lang::FunctionBuilder, Reg, Reg, Reg, bytecode_lang::TypeRef) =
        |f, a, n, i, tr| {
            f.emit(Inst::NewArray {
                dst: a,
                len: n,
                ty: tr,
            });
            f.emit(Inst::ArraySet {
                arr: a,
                idx: i,
                src: n,
            });
        };
    let neg_len: fn(&mut bytecode_lang::FunctionBuilder, Reg, Reg, Reg, bytecode_lang::TypeRef) =
        |f, a, _n, i, tr| {
            f.emit(Inst::NewArray {
                dst: a,
                len: i,
                ty: tr,
            });
            f.emit(Inst::Nop {});
        };
    let pop_empty: fn(&mut bytecode_lang::FunctionBuilder, Reg, Reg, Reg, bytecode_lang::TypeRef) =
        |f, a, n, _i, tr| {
            f.emit(Inst::NewArray {
                dst: a,
                len: n,
                ty: tr,
            });
            f.emit(Inst::ArrayPop { dst: n, arr: a });
            f.emit(Inst::ArrayPop { dst: n, arr: a });
            f.emit(Inst::ArrayPop { dst: n, arr: a });
        };
    let nil: fn(&mut bytecode_lang::FunctionBuilder, Reg, Reg, Reg, bytecode_lang::TypeRef) =
        |f, a, n, _i, _tr| {
            f.emit(Inst::ArrayLen { dst: n, arr: a });
        };
    assert!(run(1, get).is_ok());
    assert_eq!(run(2, get), raised(ErrorKind::IndexOutOfBounds, 3));
    assert_eq!(run(-1, get), raised(ErrorKind::IndexOutOfBounds, 3));
    assert_eq!(run(2, set), raised(ErrorKind::IndexOutOfBounds, 3));
    assert_eq!(run(-5, neg_len), raised(ErrorKind::IndexOutOfBounds, 2));
    assert_eq!(run(0, pop_empty), raised(ErrorKind::IndexOutOfBounds, 5));
    assert_eq!(run(0, nil), raised(ErrorKind::NullReference, 2));
}

/// `main() -> ref map K V` helper.
fn map_program(
    key: ValType,
    value: ValType,
    body: impl FnOnce(
        &mut ModuleBuilder,
        &mut bytecode_lang::FunctionBuilder,
        TypeId,
        bytecode_lang::TypeRef,
    ),
) -> Program {
    let mut m = ModuleBuilder::new();
    let mt = m.add_type(TypeDef::Map { key, value });
    let mut main = m.function("main", &[], &[ValType::Ref(mt)]);
    let tr = main.type_ref(mt);
    body(&mut m, &mut main, mt, tr);
    m.add_function(main).unwrap();
    Program::load(m.finish().unwrap(), &Host::new()).unwrap()
}

#[test]
fn op_map_instructions_typed() {
    let p = map_program(I64, I64, |_, f, mt, tr| {
        let (mp, k, v, n, found, has) = (
            f.reg(ValType::Ref(mt)),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
            f.reg(I64),
            f.reg(BOOL),
        );
        f.emit(Inst::NewMap { dst: mp, ty: tr });
        f.emit(Inst::LoadInt {
            dst: k,
            val: 5,
            ty: IntTy::I64,
        });
        f.emit(Inst::LoadInt {
            dst: v,
            val: 50,
            ty: IntTy::I64,
        });
        f.emit(Inst::MapSet {
            map: mp,
            key: k,
            src: v,
        }); // {5: 50}
        f.emit(Inst::MapPush { map: mp, src: v }); // {5: 50, 6: 50}
        f.emit(Inst::MapGet {
            dst: found,
            map: mp,
            key: k,
        }); // 50
        f.emit(Inst::MapHas {
            dst: has,
            map: mp,
            key: found,
        }); // has 50? false
        f.emit(Inst::MapLen { dst: n, map: mp }); // 2
        f.emit(Inst::MapSet {
            map: mp,
            key: n,
            src: n,
        }); // {5: 50, 6: 50, 2: 2}
        f.emit(Inst::MapDel { map: mp, key: k }); // {6: 50, 2: 2}
        f.ret(mp);
    });
    let mut vm = Vm::new(&p);
    let mp = vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(
        vm.entries(mp),
        Some(vec![
            (Value::Int(6), Value::Int(50)),
            (Value::Int(2), Value::Int(2))
        ])
    );
}

#[test]
fn map_get_missing_and_map_find() {
    let p = map_program(I64, D, |_, f, mt, tr| {
        let (mp, k, v) = (f.reg(ValType::Ref(mt)), f.reg(I64), f.reg(D));
        f.emit(Inst::NewMap { dst: mp, ty: tr });
        f.emit(Inst::MapFind {
            dst: v,
            map: mp,
            key: k,
        }); // nil
        f.emit(Inst::MapGet {
            dst: v,
            map: mp,
            key: k,
        }); // KeyNotFound
        f.ret(mp);
    });
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[]),
        raised(ErrorKind::KeyNotFound, 2)
    );
}

#[test]
fn op_iter_new_iter_next_iter_key_typed() {
    // Sum keys * 100 + values over a typed i64 -> i64 map.
    let mut m = ModuleBuilder::new();
    let mt = m.add_type(TypeDef::Map {
        key: I64,
        value: I64,
    });
    let it_t = m.add_type(TypeDef::Iter {
        key: I64,
        value: I64,
    });
    let ks: Vec<_> = [(3, 30), (1, 10)]
        .iter()
        .map(|&(k, v)| (m.constant(Const::Int(k)), m.constant(Const::Int(v))))
        .collect();
    let mc = m.constant(Const::Map(ks));
    let mut f = m.function("main", &[], &[I64]);
    let (mp, it, v, k, has, acc, hundred) = (
        f.reg(ValType::Ref(mt)),
        f.reg(ValType::Ref(it_t)),
        f.reg(I64),
        f.reg(I64),
        f.reg(BOOL),
        f.reg(I64),
        f.reg(I64),
    );
    f.emit(Inst::LoadConst { dst: mp, k: mc });
    f.emit(Inst::IterNew { dst: it, src: mp });
    f.emit(Inst::LoadInt {
        dst: hundred,
        val: 100,
        ty: IntTy::I64,
    });
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::IterNext {
        has,
        iter: it,
        val: v,
    });
    f.jmp_if_not(has, done);
    f.emit(Inst::IterKey { dst: k, iter: it });
    f.emit(Inst::IMul {
        dst: k,
        lhs: k,
        rhs: hundred,
        op: op(),
    });
    f.emit(Inst::IAdd {
        dst: acc,
        lhs: acc,
        rhs: k,
        op: op(),
    });
    f.emit(Inst::IAdd {
        dst: acc,
        lhs: acc,
        rhs: v,
        op: op(),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    // After exhaustion iter_next keeps reporting false.
    f.emit(Inst::IterNext {
        has,
        iter: it,
        val: v,
    });
    f.ret(acc);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(id, &[]),
        Ok(Value::Int(300 + 30 + 100 + 10))
    );
}

#[test]
fn iter_key_before_iter_next_is_index_out_of_bounds() {
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(I64));
    let it_t = m.add_type(TypeDef::Iter {
        key: I64,
        value: I64,
    });
    let mut f = m.function("main", &[], &[I64]);
    let tr = f.type_ref(at);
    let (a, n, it, k) = (
        f.reg(ValType::Ref(at)),
        f.reg(I64),
        f.reg(ValType::Ref(it_t)),
        f.reg(I64),
    );
    f.emit(Inst::NewArray {
        dst: a,
        len: n,
        ty: tr,
    });
    f.emit(Inst::IterNew { dst: it, src: a });
    f.emit(Inst::IterKey { dst: k, iter: it });
    f.ret(k);
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[]),
        raised(ErrorKind::IndexOutOfBounds, 2)
    );
}

#[test]
fn array_iteration_sees_the_current_length() {
    // Push during iteration: the iterator visits the pushed element too.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(I64));
    let it_t = m.add_type(TypeDef::Iter {
        key: I64,
        value: I64,
    });
    let mut f = m.function("main", &[], &[I64]);
    let tr = f.type_ref(at);
    let (a, n, it, v, has, count, one, limit, more) = (
        f.reg(ValType::Ref(at)),
        f.reg(I64),
        f.reg(ValType::Ref(it_t)),
        f.reg(I64),
        f.reg(BOOL),
        f.reg(I64),
        f.reg(I64),
        f.reg(I64),
        f.reg(BOOL),
    );
    f.emit(Inst::LoadInt {
        dst: n,
        val: 2,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    f.emit(Inst::LoadInt {
        dst: limit,
        val: 5,
        ty: IntTy::I64,
    });
    f.emit(Inst::NewArray {
        dst: a,
        len: n,
        ty: tr,
    });
    f.emit(Inst::IterNew { dst: it, src: a });
    let (top, done, skip) = (f.label(), f.label(), f.label());
    f.bind(top);
    f.emit(Inst::IterNext {
        has,
        iter: it,
        val: v,
    });
    f.jmp_if_not(has, done);
    f.emit(Inst::IAdd {
        dst: count,
        lhs: count,
        rhs: one,
        op: op(),
    });
    f.emit(Inst::ILt {
        dst: more,
        lhs: count,
        rhs: limit,
        ty: IntTy::I64,
    });
    f.jmp_if_not(more, skip);
    f.emit(Inst::ArrayPush { arr: a, src: count });
    f.bind(skip);
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    f.ret(count);
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    // 2 initial + pushes at counts 1..=4 → 6 elements visited.
    assert_eq!(Vm::new(&p).run(FuncId(0), &[]), Ok(Value::Int(6)));
}

#[test]
fn op_dup_copies_arrays_maps_structs_cells_and_rejects_iterators() {
    // dup an array, mutate the copy: the original is unchanged (COW).
    let p = array_main(|f, at, tr| {
        let (a, b, n, v) = (
            f.reg(ValType::Ref(at)),
            f.reg(ValType::Ref(at)),
            f.reg(I64),
            f.reg(I64),
        );
        f.emit(Inst::LoadInt {
            dst: n,
            val: 1,
            ty: IntTy::I64,
        });
        f.emit(Inst::NewArray {
            dst: a,
            len: n,
            ty: tr,
        });
        f.emit(Inst::Dup { dst: b, src: a });
        f.emit(Inst::LoadInt {
            dst: v,
            val: 9,
            ty: IntTy::I64,
        });
        f.emit(Inst::ArrayPush { arr: b, src: v });
        f.ret(a);
    });
    let mut vm = Vm::new(&p);
    let a = vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(vm.elements(a), Some(vec![Value::Int(0)]));

    // Iterators cannot be duplicated; strings come back as is.
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(I64));
    let it_t = m.add_type(TypeDef::Iter {
        key: I64,
        value: I64,
    });
    let s = m.constant(Const::Bytes(b"s".to_vec()));
    let mut f = m.function("main", &[], &[BOOL]);
    let tr = f.type_ref(at);
    let (a, n, it, it2, s1, s2, same) = (
        f.reg(ValType::Ref(at)),
        f.reg(I64),
        f.reg(ValType::Ref(it_t)),
        f.reg(ValType::Ref(it_t)),
        f.reg(STR),
        f.reg(STR),
        f.reg(BOOL),
    );
    f.emit(Inst::LoadConst { dst: s1, k: s });
    f.emit(Inst::Dup { dst: s2, src: s1 });
    f.emit(Inst::RefEq {
        dst: same,
        lhs: s1,
        rhs: s2,
    });
    f.emit(Inst::NewArray {
        dst: a,
        len: n,
        ty: tr,
    });
    f.emit(Inst::IterNew { dst: it, src: a });
    f.emit(Inst::Dup { dst: it2, src: it });
    f.ret(same);
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(
        Vm::new(&p).run(FuncId(0), &[]),
        raised(ErrorKind::TypeError, 5)
    );
}

#[test]
fn dup_of_a_struct_has_a_new_identity() {
    let mut m = ModuleBuilder::new();
    let pt = point(&mut m);
    let mut f = m.function("main", &[], &[ValType::Ref(pt)]);
    let t = f.type_ref(pt);
    let (a, b, v, same) = (
        f.reg(ValType::Ref(pt)),
        f.reg(ValType::Ref(pt)),
        f.reg(I64),
        f.reg(BOOL),
    );
    f.emit(Inst::NewStruct { dst: a, ty: t });
    f.emit(Inst::Dup { dst: b, src: a });
    f.emit(Inst::LoadInt {
        dst: v,
        val: 4,
        ty: IntTy::I64,
    });
    f.emit(Inst::SetField {
        obj: b,
        field: FieldIdx(0),
        src: v,
    });
    f.emit(Inst::RefEq {
        dst: same,
        lhs: a,
        rhs: b,
    });
    f.ret(a);
    m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&p);
    let a = vm.run(FuncId(0), &[]).unwrap();
    assert_eq!(vm.field(a, 0), Some(Value::Int(0)));
}

fn strs(
    texts: &[&[u8]],
    body: impl FnOnce(&mut bytecode_lang::FunctionBuilder, &[Reg]),
    result: ValType,
) -> Result<(Program, Vec<u8>), VmError> {
    let texts: Vec<Vec<u8>> = texts.iter().map(|t| t.to_vec()).collect();
    let p = program(&[result], |m, f| {
        let regs: Vec<Reg> = texts
            .iter()
            .map(|t| {
                let k = m.constant(Const::Bytes(t.clone()));
                let r = f.reg(STR);
                f.emit(Inst::LoadConst { dst: r, k });
                r
            })
            .collect();
        body(f, &regs);
    });
    let mut vm = Vm::new(&p);
    let out = vm.run(FuncId(0), &[])?;
    let bytes = vm.str_bytes(out).map(<[u8]>::to_vec).unwrap_or_default();
    drop(vm);
    Ok((p, bytes))
}

#[test]
fn op_string_instructions() {
    let (_, s) = strs(
        &[b"ab", b"cd"],
        |f, r| {
            let d = f.reg(STR);
            f.emit(Inst::StrConcat {
                dst: d,
                lhs: r[0],
                rhs: r[1],
            });
            f.ret(d);
        },
        STR,
    )
    .unwrap();
    assert_eq!(s, b"abcd");
    let (_, s) = strs(
        &[b"x", b"y", b"z"],
        |f, r| {
            let d = f.reg(STR);
            f.emit(Inst::StrConcatN {
                dst: d,
                first: r[0],
                count: 3,
            });
            f.ret(d);
        },
        STR,
    )
    .unwrap();
    assert_eq!(s, b"xyz");
    let (_, s) = strs(
        &[],
        |f, _| {
            let d = f.reg(STR);
            f.emit(Inst::StrConcatN {
                dst: d,
                first: Reg(0),
                count: 0,
            });
            f.ret(d);
        },
        STR,
    )
    .unwrap();
    assert_eq!(s, b"", "count 0 gives the empty string");
    let len = program(&[I64], |m, f| {
        let k = m.constant(Const::Bytes("héllo".as_bytes().to_vec()));
        let (s, n) = (f.reg(STR), f.reg(I64));
        f.emit(Inst::LoadConst { dst: s, k });
        f.emit(Inst::StrLen { dst: n, s });
        f.ret(n);
    });
    assert_eq!(
        Vm::new(&len).run(FuncId(0), &[]),
        Ok(Value::Int(6)),
        "bytes, not chars"
    );
}

#[test]
fn op_str_eq_str_cmp_str_byte() {
    let cmp = |a: &[u8], b: &[u8]| {
        let p = program(&[ValType::I8], |m, f| {
            let (ka, kb) = (
                m.constant(Const::Bytes(a.to_vec())),
                m.constant(Const::Bytes(b.to_vec())),
            );
            let (x, y, r) = (f.reg(STR), f.reg(STR), f.reg(ValType::I8));
            f.emit(Inst::LoadConst { dst: x, k: ka });
            f.emit(Inst::LoadConst { dst: y, k: kb });
            f.emit(Inst::StrCmp {
                dst: r,
                lhs: x,
                rhs: y,
            });
            f.ret(r);
        });
        Vm::new(&p).run(FuncId(0), &[]).unwrap()
    };
    assert_eq!(cmp(b"a", b"b"), Value::Int(-1));
    assert_eq!(cmp(b"b", b"a"), Value::Int(1));
    assert_eq!(cmp(b"ab", b"ab"), Value::Int(0));
    assert_eq!(cmp(b"a", b"ab"), Value::Int(-1));
    let eq = program(&[BOOL], |m, f| {
        let (ka, kb) = (
            m.constant(Const::Bytes(b"q".to_vec())),
            m.constant(Const::Bytes(b"qq".to_vec())),
        );
        let (x, y, z, r) = (f.reg(STR), f.reg(STR), f.reg(STR), f.reg(BOOL));
        f.emit(Inst::LoadConst { dst: x, k: ka });
        f.emit(Inst::LoadConst { dst: y, k: kb });
        f.emit(Inst::StrConcat {
            dst: z,
            lhs: x,
            rhs: x,
        });
        f.emit(Inst::StrEq {
            dst: r,
            lhs: y,
            rhs: z,
        });
        f.ret(r);
    });
    assert_eq!(Vm::new(&eq).run(FuncId(0), &[]), Ok(Value::Bool(true)));
    let byte = |i: i32| {
        let p = program(&[ValType::U8], |m, f| {
            let k = m.constant(Const::Bytes(vec![7, 200]));
            let (s, idx, r) = (f.reg(STR), f.reg(I64), f.reg(ValType::U8));
            f.emit(Inst::LoadConst { dst: s, k });
            f.emit(Inst::LoadInt {
                dst: idx,
                val: i,
                ty: IntTy::I64,
            });
            f.emit(Inst::StrByte { dst: r, s, idx });
            f.ret(r);
        });
        Vm::new(&p).run(FuncId(0), &[])
    };
    assert_eq!(byte(1), Ok(Value::UInt(200)));
    assert_eq!(byte(2), raised(ErrorKind::IndexOutOfBounds, 2));
    assert_eq!(byte(-1), raised(ErrorKind::IndexOutOfBounds, 2));
}

#[test]
fn op_str_slice_bounds_and_utf8() {
    let slice = |start: i32, end: i32, utf8: bool| -> Result<Vec<u8>, VmError> {
        let p = program(&[STR], |m, f| {
            let k = m.constant(Const::Bytes("aé!".as_bytes().to_vec())); // a, c3 a9, !
            let s = f.reg(STR);
            let range = f.regs(&[I64, I64]);
            let d = f.reg(STR);
            f.emit(Inst::LoadConst { dst: s, k });
            f.emit(Inst::LoadInt {
                dst: range,
                val: start,
                ty: IntTy::I64,
            });
            f.emit(Inst::LoadInt {
                dst: Reg(range.0 + 1),
                val: end,
                ty: IntTy::I64,
            });
            f.emit(Inst::StrSlice {
                dst: d,
                s,
                range,
                utf8,
            });
            f.ret(d);
        });
        let mut vm = Vm::new(&p);
        let out = vm.run(FuncId(0), &[])?;
        Ok(vm.str_bytes(out).unwrap_or_default().to_vec())
    };
    assert_eq!(slice(1, 3, true), Ok("é".as_bytes().to_vec()));
    assert_eq!(slice(0, 0, false), Ok(Vec::new()));
    assert_eq!(slice(2, 4, false), Ok(vec![0xA9, b'!']));
    assert_eq!(
        slice(2, 4, true),
        Err(VmError::Raised {
            kind: ErrorKind::InvalidStrIndex,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 3
        })
    );
    assert_eq!(
        slice(0, 2, true),
        Err(VmError::Raised {
            kind: ErrorKind::InvalidStrIndex,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 3
        })
    );
    assert_eq!(
        slice(3, 2, false),
        Err(VmError::Raised {
            kind: ErrorKind::IndexOutOfBounds,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 3
        })
    );
    assert_eq!(
        slice(0, 5, false),
        Err(VmError::Raised {
            kind: ErrorKind::IndexOutOfBounds,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 3
        })
    );
    assert_eq!(
        slice(-1, 1, false),
        Err(VmError::Raised {
            kind: ErrorKind::IndexOutOfBounds,
            payload: bvm_lang::Value::Nil,
            func: FuncId(0),
            pc: 3
        })
    );
}

#[test]
fn string_instructions_on_nil_are_null_reference() {
    let out = common::eval(&[], &[I64], &[], |_, f| {
        let (s, n) = (f.reg(STR), f.reg(I64));
        f.emit(Inst::StrLen { dst: n, s });
        f.ret(n);
    });
    assert_eq!(out, raised(ErrorKind::NullReference, 0));
}
