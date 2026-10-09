//! Running code you do not trust: every budget is explicit, every failure
//! is a value with an error code and a location.
//!
//! `cargo run --example untrusted`

use bvm_lang::{Host, Limits, Program, Vm, VmError};
use bytecode_lang::{Const, Inst, ModuleBuilder, Target, TypeDef, ValType};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut m = ModuleBuilder::new();
    let file = m.string("hostile.mox");
    // spin: `while (true) {}` with no safepoint at all.
    let mut spin = m.function("spin", &[], &[]);
    spin.set_location(file, 1, 1);
    spin.emit(Inst::Jmp { target: Target(0) });
    let spin = m.add_function(spin)?;
    // hog: `$a = []; while (true) { $a[] = $a; }`.
    let at = m.add_type(TypeDef::Array(ValType::Dyn));
    let mut hog = m.function("hog", &[], &[]);
    hog.set_location(file, 2, 1);
    let tr = hog.type_ref(at);
    let (a, n) = (hog.reg(ValType::Dyn), hog.reg(ValType::I64));
    hog.emit(Inst::NewArray {
        dst: a,
        len: n,
        ty: tr,
    });
    hog.emit(Inst::ArrayPush { arr: a, src: a });
    hog.emit(Inst::Jmp { target: Target(1) });
    let hog = m.add_function(hog)?;
    // boom: `"abc" . null` raises NullReference.
    let s = m.constant(Const::Bytes(b"abc".to_vec()));
    let mut boom = m.function("boom", &[], &[]);
    boom.set_location(file, 3, 7);
    let (x, y) = (boom.reg(ValType::Str), boom.reg(ValType::Str));
    boom.emit(Inst::LoadConst { dst: x, k: s });
    boom.emit(Inst::StrConcat {
        dst: x,
        lhs: x,
        rhs: y,
    });
    boom.ret_void();
    let boom = m.add_function(boom)?;

    let program = Program::load(m.finish()?, &Host::new())?;
    let limits = Limits::new()
        .with_fuel(1_000_000)
        .with_memory(8 << 20)
        .with_depth(256);
    let mut vm = Vm::with_limits(&program, limits);
    for f in [spin, hog, boom] {
        let err = vm.run(f, &[]).unwrap_err();
        let at = err
            .location()
            .and_then(|(func, pc)| program.location(func, pc))
            .map(|l| l.to_string())
            .unwrap_or_default();
        println!("{f}: {err} ({at}); code {:?}", err.code());
        assert!(matches!(err, VmError::Trap { .. } | VmError::Raised { .. }));
    }
    Ok(())
}
