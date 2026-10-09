//! Coroutines on the Tier-1 path: a PHP-style generator with keys consumed by
//! `foreach`, and two async tasks taking turns under the built-in scheduler.
//!
//! `cargo run --example coroutines`

use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{Callee, Const, Hook, Inst, ModuleBuilder, Policy, Reg, TypeDef, ValType};

const D: ValType = ValType::Dyn;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    generator()?;
    tasks()?;
    Ok(())
}

/// ```php
/// function squares($n) { for ($i = 0; $i < $n; $i++) yield $i * 10 => $i * $i; return "done"; }
/// foreach (squares(4) as $k => $v) $out[$k] = $v;
/// ```
fn generator() -> Result<(), Box<dyn std::error::Error>> {
    let mut m = ModuleBuilder::new();
    let map_t = m.add_type(TypeDef::Map { key: D, value: D });
    let done_k = m.constant(Const::Bytes(b"done".to_vec()));

    let mut g = m.function("squares", &[D], &[D]);
    let (i, sq, key, ten, one, sent, more) = (
        g.reg(D),
        g.reg(D),
        g.reg(D),
        g.reg(D),
        g.reg(D),
        g.reg(D),
        g.reg(ValType::Bool),
    );
    g.emit(Inst::DLoadInt { dst: one, val: 1 });
    g.emit(Inst::DLoadInt { dst: ten, val: 10 });
    g.emit(Inst::DLoadInt { dst: i, val: 0 });
    let (top, end) = (g.label(), g.label());
    g.bind(top);
    g.emit(Inst::DLt {
        dst: more,
        lhs: i,
        rhs: Reg(0),
    });
    g.jmp_if_not(more, end);
    g.emit(Inst::DMul {
        dst: sq,
        lhs: i,
        rhs: i,
        pol: Policy::new(),
    });
    g.emit(Inst::DMul {
        dst: key,
        lhs: i,
        rhs: ten,
        pol: Policy::new(),
    });
    // `yield $key => $sq`: suspends here; `sent` receives what the consumer
    // sends back (nil from `foreach`).
    g.emit(Inst::YieldKv {
        dst: sent,
        key,
        src: sq,
    });
    g.emit(Inst::DAdd {
        dst: i,
        lhs: i,
        rhs: one,
        pol: Policy::new(),
    });
    g.emit(Inst::Safepoint {});
    g.jmp(top);
    g.bind(end);
    let r = g.reg(D);
    g.emit(Inst::DLoadConst { dst: r, k: done_k });
    g.ret(r);
    let squares = m.add_function(g)?;

    let mut f = m.function("main", &[], &[D]);
    let mt = f.type_ref(map_t);
    let (out, w, it, k, v, has) = (
        f.reg(D),
        f.regs(&[D, D]),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(ValType::Bool),
    );
    f.emit(Inst::NewMap { dst: out, ty: mt });
    f.emit(Inst::DLoadInt {
        dst: Reg(w.0 + 1),
        val: 4,
    });
    f.emit(Inst::CoroNew {
        dst: w,
        func: squares,
        argc: 1,
    });
    // foreach: one `iter_next` (a resume) and one branch per element.
    f.emit(Inst::DIterNew { dst: it, src: w });
    let (top, end) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::IterNext {
        has,
        iter: it,
        val: v,
    });
    f.jmp_if_not(has, end);
    f.emit(Inst::IterKey { dst: k, iter: it });
    f.emit(Inst::DSetIndex {
        obj: out,
        key: k,
        src: v,
    });
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(end);
    f.ret(out);
    let main = m.add_function(f)?;

    let program = Program::load(m.finish()?, &Host::new())?;
    let mut vm = Vm::new(&program);
    let out = vm.run(main, &[])?;
    println!("foreach (squares(4) as $k => $v):");
    for (k, v) in vm.entries(out).unwrap_or_default() {
        println!("  {k} => {v}");
    }
    Ok(())
}

/// ```text
/// async fn player(name, n) { for _ in 0..n { log(name); await nil } }
/// async fn main() { a = spawn player(1, 3); b = spawn player(2, 3); await a; await b }
/// ```
fn tasks() -> Result<(), Box<dyn std::error::Error>> {
    let mut host = Host::new();
    host.register_scheduler("ls.async", "spawn");
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D], &[D]);
    let spawn = m.import("ls.async", "spawn", sig);
    m.hook(Hook::Spawn, Callee::Import(spawn));
    let at = m.add_type(TypeDef::Array(D));
    let log = m.global("log", D, true, None);

    let mut p = m.function("player", &[D, D], &[]);
    let (i, one, more, a, nil, got) = (
        p.reg(D),
        p.reg(D),
        p.reg(ValType::Bool),
        p.reg(D),
        p.reg(D),
        p.reg(D),
    );
    p.emit(Inst::DLoadInt { dst: one, val: 1 });
    p.emit(Inst::DLoadInt { dst: i, val: 0 });
    let (top, end) = (p.label(), p.label());
    p.bind(top);
    p.emit(Inst::DLt {
        dst: more,
        lhs: i,
        rhs: Reg(1),
    });
    p.jmp_if_not(more, end);
    p.emit(Inst::GetGlobal {
        dst: a,
        global: log,
    });
    p.emit(Inst::ArrayPush {
        arr: a,
        src: Reg(0),
    });
    p.emit(Inst::Await { dst: got, src: nil }); // let the other task run
    p.emit(Inst::DAdd {
        dst: i,
        lhs: i,
        rhs: one,
        pol: Policy::new(),
    });
    p.jmp(top);
    p.bind(end);
    p.ret_void();
    let player = m.add_function(p)?;

    let mut f = m.function("main", &[], &[]);
    let ty = f.type_ref(at);
    let (len, arr, fv, r) = (f.reg(ValType::I64), f.reg(D), f.reg(D), f.reg(D));
    f.emit(Inst::NewArray { dst: arr, len, ty });
    f.emit(Inst::SetGlobal {
        global: log,
        src: arr,
    });
    f.emit(Inst::MakeClosure {
        dst: fv,
        func: player,
    });
    let mut handles = Vec::new();
    for name in [1, 2] {
        let w = f.regs(&[D, D, D]);
        f.emit(Inst::DLoadInt {
            dst: Reg(w.0 + 1),
            val: name,
        });
        f.emit(Inst::DLoadInt {
            dst: Reg(w.0 + 2),
            val: 3,
        });
        f.emit(Inst::Spawn {
            dst: w,
            callee: fv,
            argc: 2,
        });
        handles.push(w);
    }
    for h in handles {
        f.emit(Inst::Await { dst: r, src: h });
    }
    f.ret_void();
    let main = m.add_function(f)?;

    let program = Program::load(m.finish()?, &host)?;
    let mut vm = Vm::new(&program);
    vm.run_async(main, &[])?;
    let order: Vec<String> = vm
        .global(log)
        .and_then(|l| vm.elements(l))
        .unwrap_or_default()
        .iter()
        .map(Value::to_string)
        .collect();
    println!("tasks ran in turn: {}", order.join(" "));
    Ok(())
}
