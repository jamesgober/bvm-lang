//! The two memory profiles (DECISIONS D22) on the same PHP program:
//!
//! ```php
//! class Res { public $id; function __destruct() { echo $this->id; } }
//! $a = new Res(1);
//! $a = new Res(2);         // PHP prints 1 here: the old object's last reference
//! echo 9;
//! $x = [1, 2]; $r = &$x[0]; unset($r);
//! $y = $x; $y[0] = 7;      // PHP: $x[0] stays 1 (the copy drops the reference)
//! return $x[0];
//! ```
//!
//! Under `Memory::Rc` the VM prints `19` and returns 1, as PHP does (the
//! second object is destructed when the function returns: `192`). Under the
//! default `Memory::Tracing` no destructor runs until a collection finds the
//! objects, and the copy shares the reference (LSB's rule): it prints `9`
//! and returns 7, then `12` once the host collects and runs the queue.
//!
//! `cargo run --example memory_profiles`

use bvm_lang::{Host, HostCtx, Limits, Memory, Program, Value, Vm};
use bytecode_lang::{
    Field, FieldIdx, Inst, IntTy, Method, ModuleBuilder, Reg, StructDef, TypeDef, ValType,
};

const D: ValType = ValType::Dyn;

fn program() -> Result<(Program, bytecode_lang::FuncId), Box<dyn std::error::Error>> {
    let mut m = ModuleBuilder::new();
    let echo_t = m.func_type(&[ValType::I64], &[]);
    let echo = m.import("php", "echo", echo_t);

    // class Res { public int $id; function __destruct() { echo $this->id; } }
    let res = m.reserve_type();
    let mut d = m.function("Res::__destruct", &[D], &[]);
    let id = d.reg(ValType::I64);
    // The call window: result, then the argument.
    let w = d.regs(&[ValType::I64, ValType::I64]);
    d.emit(Inst::GetField {
        dst: id,
        obj: Reg(0),
        field: FieldIdx(0),
    });
    d.mov(Reg(w.0 + 1), id);
    d.emit(Inst::CallImport {
        dst: w,
        import: echo,
        argc: 1,
    });
    d.ret_void();
    let dtor = m.add_function(d)?;
    let (name, id_n, dtor_n) = (m.string("Res"), m.string("id"), m.string("__destruct"));
    m.define_type(
        res,
        TypeDef::Struct(StructDef {
            name,
            parent: None,
            fields: vec![Field {
                name: id_n,
                ty: ValType::I64,
            }],
            methods: vec![Method {
                name: dtor_n,
                func: dtor,
            }],
        }),
    );

    let arr_t = m.add_type(TypeDef::Array(D));
    let mut f = m.function("main", &[], &[D]);
    let ty = f.type_ref(res);
    let at = f.type_ref(arr_t);
    let (a, v) = (f.reg(D), f.reg(ValType::I64));
    let w = f.regs(&[ValType::I64, ValType::I64]);
    for n in [1, 2] {
        // $a = new Res(n): the `new_struct` overwrites $a.
        f.emit(Inst::NewStruct { dst: a, ty });
        f.emit(Inst::LoadInt {
            dst: v,
            val: n,
            ty: IntTy::I64,
        });
        f.emit(Inst::SetField {
            obj: a,
            field: FieldIdx(0),
            src: v,
        });
    }
    // echo 9;
    f.emit(Inst::LoadInt {
        dst: Reg(w.0 + 1),
        val: 9,
        ty: IntTy::I64,
    });
    f.emit(Inst::CallImport {
        dst: w,
        import: echo,
        argc: 1,
    });
    // $x = [1, 2]; $r = &$x[0]; unset($r); $y = $x; $y[0] = 7; return $x[0];
    let (len, x, y, r, k, e) = (
        f.reg(ValType::I64),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
        f.reg(D),
    );
    f.emit(Inst::NewArray {
        dst: x,
        len,
        ty: at,
    });
    for n in [1, 2] {
        f.emit(Inst::DLoadInt { dst: e, val: n });
        f.emit(Inst::ArrayPush { arr: x, src: e });
    }
    f.emit(Inst::DLoadInt { dst: k, val: 0 });
    f.emit(Inst::DRefIndex {
        dst: r,
        obj: x,
        key: k,
    });
    f.emit(Inst::LoadNil { dst: r });
    f.emit(Inst::Dup { dst: y, src: x });
    f.emit(Inst::DLoadInt { dst: e, val: 7 });
    f.emit(Inst::DSetIndex {
        obj: y,
        key: k,
        src: e,
    });
    f.emit(Inst::DGetIndex {
        dst: e,
        obj: x,
        key: k,
    });
    f.ret(e);
    let main = m.add_function(f)?;

    let mut host = Host::new();
    host.register("php", "echo", |_: &mut HostCtx<'_>, args: &[Value]| {
        print!("{}", args.first().copied().unwrap_or(Value::Nil));
        Ok(Value::Nil)
    });
    Ok((Program::load(m.finish()?, &host)?, main))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (p, main) = program()?;
    for memory in [Memory::Rc, Memory::Tracing] {
        let mut vm = Vm::with_memory(&p, memory);
        vm.set_limits(Limits::untrusted());
        vm.set_destructor("__destruct")?;
        print!("{memory:?}: prints ");
        let x0 = vm.run(main, &[])?;
        print!(" | returns {x0}");
        // Whatever is left: a collection queues it, the queue runs it.
        vm.collect_garbage();
        print!(" | after collecting, prints ");
        let ran = vm.run_finalizers()?;
        println!(" ({ran} destructors)");
    }
    Ok(())
}
