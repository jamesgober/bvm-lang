//! The Tier-1 path: build an LSB module with bytecode-lang, load it, run it.
//!
//! `cargo run --example quickstart`

use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{ExportItem, Inst, IntOp, IntTy, ModuleBuilder, Reg, ValType};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // fn sum_to(n: i64) -> i64 { let mut s = 0; for i in 0..=n { s += i } s }
    let mut m = ModuleBuilder::new();
    let mut f = m.function("sum_to", &[ValType::I64], &[ValType::I64]);
    let (s, i, one, more) = (
        f.reg(ValType::I64),
        f.reg(ValType::I64),
        f.reg(ValType::I64),
        f.reg(ValType::Bool),
    );
    let op = IntOp::new(IntTy::I64);
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    let (top, done) = (f.label(), f.label());
    f.bind(top);
    f.emit(Inst::ILe {
        dst: more,
        lhs: i,
        rhs: Reg(0),
        ty: IntTy::I64,
    });
    f.jmp_if_not(more, done);
    f.emit(Inst::IAdd {
        dst: s,
        lhs: s,
        rhs: i,
        op,
    });
    f.emit(Inst::IAdd {
        dst: i,
        lhs: i,
        rhs: one,
        op,
    });
    f.emit(Inst::Safepoint {}); // where fuel is charged and GC may run
    f.jmp(top);
    f.bind(done);
    f.ret(s);
    let id = m.add_function(f)?;
    m.export("sum_to", ExportItem::Func(id));

    // The bytes are what a compiler would write to disk.
    let bytes = bytecode_lang::encode(&m.finish()?);
    let program = Program::decode(&bytes, &Host::new())?;
    let mut vm = Vm::new(&program);
    let out = vm.run_export("sum_to", &[Value::Int(1_000)])?;
    println!("sum_to(1000) = {out}"); // 500500
    println!("fuel used: {}", vm.fuel_used());
    Ok(())
}
