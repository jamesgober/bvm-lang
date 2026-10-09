//! PHP's dynamic calls, references, and nested writes on LSB format 2, as a
//! PHP code generator lowers them:
//!
//! ```php
//! function tally(array &$counts, string $key, int $by = 1, ...$tags) {
//!     $counts[$key] = ($counts[$key] ?? 0) + $by;
//!     return count($tags);
//! }
//! $counts = [];
//! $f = 'tally';                              // a function value
//! $f($counts, 'a');                          // by reference: decided at run time
//! $f($counts, 'a', by: 5);                   // a named argument
//! $f($counts, 'b', 1, 'x', 'y');             // extra arguments go to ...$tags
//! $groups = [[], []];
//! for ($i = 0; $i < 6; $i++) $groups[$i % 2][] = $i;   // nested writes
//! return [$counts, $groups];
//! ```
//!
//! `cargo run --example php_calls`

use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{
    ArgKind, Const, Inst, IntTy, ModuleBuilder, Param, ParamKind, ParamList, Policy, Prim, Reg,
    TypeDef, ValType,
};

const D: ValType = ValType::Dyn;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut m = ModuleBuilder::new();
    let map_t = m.add_type(TypeDef::Map { key: D, value: D });
    let (counts_n, key_n, by_n, tags_n) = (
        m.string("counts"),
        m.string("key"),
        m.string("by"),
        m.string("tags"),
    );

    // tally(&$counts, $key, $by = <default>, ...$tags): the presence mask is
    // the last parameter; the callee computes `$by`'s default itself.
    let mut t = m.function("tally", &[D, D, D, D, ValType::I64], &[D]);
    t.set_params(ParamList::new(vec![
        Param::normal(counts_n).by_ref(),
        Param::normal(key_n),
        Param::normal(by_n).with_default(),
        Param::new(ParamKind::RestMap, Some(tags_n)),
    ]));
    let (counts, cur, by, bit, zero, has, n, out) = (
        t.reg(D),
        t.reg(D),
        t.reg(D),
        t.reg(ValType::I64),
        t.reg(ValType::I64),
        t.reg(ValType::Bool),
        t.reg(ValType::I64),
        t.reg(D),
    );
    // $by defaults to 1 when bit 2 of the mask is clear.
    t.mov(by, Reg(2));
    t.emit(Inst::LoadInt {
        dst: bit,
        val: 4,
        ty: IntTy::I64,
    });
    t.emit(Inst::IAnd {
        dst: bit,
        lhs: Reg(4),
        rhs: bit,
        op: bytecode_lang::IntOp::new(IntTy::I64),
    });
    t.emit(Inst::LoadInt {
        dst: zero,
        val: 0,
        ty: IntTy::I64,
    });
    t.emit(Inst::INe {
        dst: has,
        lhs: bit,
        rhs: zero,
        ty: IntTy::I64,
    });
    let given = t.label();
    t.jmp_if(has, given);
    t.emit(Inst::DLoadInt { dst: by, val: 1 });
    t.bind(given);
    // $counts is the caller's array through the reference.
    t.emit(Inst::CellGet {
        dst: counts,
        cell: Reg(0),
    });
    // `($counts[$key] ?? 0) + $by`: a missing key reads as nil.
    t.emit(Inst::MapFind {
        dst: cur,
        map: counts,
        key: Reg(1),
    });
    let (missing, store) = (t.label(), t.label());
    t.emit(Inst::IsKind {
        dst: has,
        src: cur,
        kind: bytecode_lang::Kind::Int,
    });
    t.jmp_if_not(has, missing);
    t.emit(Inst::DAdd {
        dst: cur,
        lhs: cur,
        rhs: by,
        pol: Policy::new(),
    });
    t.jmp(store);
    t.bind(missing);
    t.mov(cur, by);
    t.bind(store);
    t.emit(Inst::DSetIndex {
        obj: counts,
        key: Reg(1),
        src: cur,
    });
    t.emit(Inst::DLen {
        dst: n,
        src: Reg(3),
    });
    t.emit(Inst::ToDyn {
        dst: out,
        src: n,
        from: Prim::I64,
    });
    t.ret(out);
    let tally = m.add_function(t)?;

    let a = m.constant(Const::Bytes(b"a".to_vec()));
    let b = m.constant(Const::Bytes(b"b".to_vec()));
    let x = m.constant(Const::Bytes(b"x".to_vec()));
    let y = m.constant(Const::Bytes(b"y".to_vec()));

    let mut f = m.function("main", &[], &[D]);
    let mt = f.type_ref(map_t);
    let (fv, counts, rcounts, pos, flag) = (
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(ValType::I64),
        f.reg(ValType::Bool),
    );
    f.emit(Inst::MakeClosure {
        dst: fv,
        func: tally,
    });
    f.emit(Inst::NewMap {
        dst: counts,
        ty: mt,
    });
    // Before evaluating argument 0 the code generator asks whether `$f`
    // takes it by reference; it does, so it passes a reference to $counts.
    f.emit(Inst::LoadInt {
        dst: pos,
        val: 0,
        ty: IntTy::I64,
    });
    f.emit(Inst::DParamRef {
        dst: flag,
        callee: fv,
        pos,
    });
    f.emit(Inst::NewRef {
        dst: rcounts,
        src: counts,
    });
    // $f($counts, 'a')
    let w = f.regs(&[D, D, D]);
    f.mov(Reg(w.0 + 1), rcounts);
    f.emit(Inst::DLoadConst {
        dst: Reg(w.0 + 2),
        k: a,
    });
    f.emit(Inst::DCall {
        dst: w,
        callee: fv,
        argc: 2,
    });
    // $f($counts, 'a', by: 5)
    let w = f.regs(&[D, D, D, D]);
    f.mov(Reg(w.0 + 1), rcounts);
    f.emit(Inst::DLoadConst {
        dst: Reg(w.0 + 2),
        k: a,
    });
    f.emit(Inst::DLoadInt {
        dst: Reg(w.0 + 3),
        val: 5,
    });
    f.dcall_shape(
        w,
        fv,
        &[
            ArgKind::Positional,
            ArgKind::Positional,
            ArgKind::Named(by_n),
        ],
    );
    // $f($counts, 'b', 1, 'x', 'y')
    let w = f.regs(&[D, D, D, D, D, D]);
    let tags = w;
    f.mov(Reg(w.0 + 1), rcounts);
    f.emit(Inst::DLoadConst {
        dst: Reg(w.0 + 2),
        k: b,
    });
    f.emit(Inst::DLoadInt {
        dst: Reg(w.0 + 3),
        val: 1,
    });
    f.emit(Inst::DLoadConst {
        dst: Reg(w.0 + 4),
        k: x,
    });
    f.emit(Inst::DLoadConst {
        dst: Reg(w.0 + 5),
        k: y,
    });
    f.emit(Inst::DCall {
        dst: w,
        callee: fv,
        argc: 5,
    });

    // $groups = [[], []]; for ($i = 0; $i < 6; $i++) $groups[$i % 2][] = $i;
    let (groups, inner, i, two, six, one, more, k) = (
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(ValType::Bool),
        f.reg(D),
    );
    f.emit(Inst::NewMap {
        dst: groups,
        ty: mt,
    });
    for _ in 0..2 {
        f.emit(Inst::NewMap { dst: inner, ty: mt });
        f.emit(Inst::MapPush {
            map: groups,
            src: inner,
        });
    }
    f.emit(Inst::DLoadInt { dst: i, val: 0 });
    f.emit(Inst::DLoadInt { dst: two, val: 2 });
    f.emit(Inst::DLoadInt { dst: six, val: 6 });
    f.emit(Inst::DLoadInt { dst: one, val: 1 });
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::DLt {
        dst: more,
        lhs: i,
        rhs: six,
    });
    f.jmp_if_not(more, done);
    f.emit(Inst::DFloorMod {
        dst: k,
        lhs: i,
        rhs: two,
        pol: Policy::new(),
    });
    // Separate $groups[$k] (a copy only if another container may share it),
    // then append to it in place.
    f.emit(Inst::DSepIndex {
        dst: inner,
        obj: groups,
        key: k,
    });
    f.emit(Inst::MapPush { map: inner, src: i });
    f.emit(Inst::DAdd {
        dst: i,
        lhs: i,
        rhs: one,
        pol: Policy::new(),
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);

    let out = f.reg(D);
    f.emit(Inst::NewMap { dst: out, ty: mt });
    f.emit(Inst::CellGet {
        dst: counts,
        cell: rcounts,
    });
    f.emit(Inst::MapPush {
        map: out,
        src: counts,
    });
    f.emit(Inst::MapPush {
        map: out,
        src: groups,
    });
    f.emit(Inst::MapPush {
        map: out,
        src: tags,
    });
    f.ret(out);
    let main = m.add_function(f)?;

    let program = Program::load(m.finish()?, &Host::new())?;
    let mut vm = Vm::new(&program);
    let result = vm.run(main, &[])?;
    println!("{}", show(&vm, result));
    // [[a => 6, b => 1], [[0, 2, 4], [1, 3, 5]], 2]
    Ok(())
}

/// PHP-like rendering of a result.
fn show(vm: &Vm<'_>, v: Value) -> String {
    if let Some(b) = vm.str_bytes(v) {
        return String::from_utf8_lossy(b).into_owned();
    }
    if let Some(entries) = vm.entries(v) {
        let list = entries
            .iter()
            .enumerate()
            .all(|(i, (k, _))| *k == Value::Int(i as i64));
        let parts: Vec<String> = entries
            .into_iter()
            .map(|(k, x)| {
                if list {
                    show(vm, x)
                } else {
                    format!("{} => {}", show(vm, k), show(vm, x))
                }
            })
            .collect();
        return format!("[{}]", parts.join(", "));
    }
    match v {
        Value::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}
