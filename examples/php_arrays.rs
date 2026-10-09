//! Mox/PHP-style dynamic code: an ordered hash-map array, `$a[] = v`,
//! `foreach`, and a hook (PHP's loose `==` between an int and a numeric
//! string) supplied by the host.
//!
//! ```php
//! $a = [];
//! $a[] = 10; $a[] = 20; $a["name"] = "mox"; $a[] = 30;   // keys 0, 1, "name", 2
//! $hits = 0;
//! foreach ($a as $k => $v) { if ($v == "20") $hits++; }
//! return $hits;                                          // 1 (loose ==)
//! ```
//!
//! `cargo run --example php_arrays`

use bvm_lang::{Host, HostError, Program, Value, Vm};
use bytecode_lang::{
    Callee, Const, ErrorKind, Hook, Inst, ModuleBuilder, Policy, TypeDef, ValType,
};

const D: ValType = ValType::Dyn;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The host's loose equality for (int, numeric string); everything else
    // compares as the VM's built-in rule would (identity here).
    let mut host = Host::new();
    host.register("mox.rt", "loose_eq", |ctx, args| {
        let [a, b] = args else {
            return Err(HostError::Raise(ErrorKind::TypeError));
        };
        let num = |v: &Value| match *v {
            Value::Int(i) => Some(i),
            other => ctx
                .str_bytes(other)
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.trim().parse().ok()),
        };
        Ok(Value::Bool(
            matches!((num(a), num(b)), (Some(x), Some(y)) if x == y),
        ))
    });

    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[D, D], &[D]);
    let loose_eq = m.import("mox.rt", "loose_eq", sig);
    m.hook(Hook::Eq, Callee::Import(loose_eq));
    let map_t = m.add_type(TypeDef::Map { key: D, value: D });
    let name_k = m.constant(Const::Bytes(b"name".to_vec()));
    let mox = m.constant(Const::Bytes(b"mox".to_vec()));
    let twenty = m.constant(Const::Bytes(b"20".to_vec()));

    let mut f = m.function("main", &[], &[D]);
    let mt = f.type_ref(map_t);
    let (a, v, k, it, hits, one, needle) = (
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
    );
    let (has, eq) = (f.reg(ValType::Bool), f.reg(ValType::Bool));
    let p = Policy::new();
    f.emit(Inst::NewMap { dst: a, ty: mt });
    for val in [10, 20] {
        f.emit(Inst::DLoadInt { dst: v, val });
        f.emit(Inst::MapPush { map: a, src: v });
    }
    f.emit(Inst::DLoadConst { dst: k, k: name_k });
    f.emit(Inst::DLoadConst { dst: v, k: mox });
    f.emit(Inst::MapSet {
        map: a,
        key: k,
        src: v,
    });
    f.emit(Inst::DLoadInt { dst: v, val: 30 });
    f.emit(Inst::MapPush { map: a, src: v });

    f.emit(Inst::DLoadInt { dst: hits, val: 0 });
    f.emit(Inst::DLoadInt { dst: one, val: 1 });
    f.emit(Inst::DLoadConst {
        dst: needle,
        k: twenty,
    });
    f.emit(Inst::DIterNew { dst: it, src: a });
    let (top, done, skip) = (f.label(), f.label(), f.label());
    f.bind(top);
    f.emit(Inst::IterNext {
        has,
        iter: it,
        val: v,
    });
    f.jmp_if_not(has, done);
    f.emit(Inst::DEq {
        dst: eq,
        lhs: v,
        rhs: needle,
    }); // int vs str: the hook decides
    f.jmp_if_not(eq, skip);
    f.emit(Inst::DAdd {
        dst: hits,
        lhs: hits,
        rhs: one,
        pol: p,
    });
    f.bind(skip);
    f.emit(Inst::Safepoint {});
    f.jmp(top);
    f.bind(done);
    f.ret(hits);
    let main = m.add_function(f)?;

    let program = Program::load(m.finish()?, &host)?;
    let mut vm = Vm::new(&program);
    let hits = vm.run(main, &[])?;
    println!("hits = {hits}"); // 1
    Ok(())
}
