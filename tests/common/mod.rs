//! Shared helpers for the integration tests: build a one-function module,
//! load it, and run it.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use bvm_lang::{Host, Limits, Program, Value, Vm, VmError};
use bytecode_lang::{FuncId, FunctionBuilder, Module, ModuleBuilder, ValType};

/// Builds a module whose function 0 is made by `body`.
pub fn module(
    params: &[ValType],
    results: &[ValType],
    body: impl FnOnce(&mut ModuleBuilder, &mut FunctionBuilder),
) -> Module {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("main", params, results);
    body(&mut m, &mut f);
    m.add_function(f).expect("function builds");
    m.finish().expect("module builds")
}

/// Loads a module with no host functions.
pub fn load(module: Module) -> Program {
    Program::load(module, &Host::new()).expect("module loads")
}

/// Builds, loads, and runs function 0 with `args`.
pub fn eval(
    params: &[ValType],
    results: &[ValType],
    args: &[Value],
    body: impl FnOnce(&mut ModuleBuilder, &mut FunctionBuilder),
) -> Result<Value, VmError> {
    let p = load(module(params, results, body));
    let mut vm = Vm::new(&p);
    vm.run(FuncId(0), args)
}

/// As [`eval`], with limits.
pub fn eval_with(
    params: &[ValType],
    results: &[ValType],
    args: &[Value],
    limits: Limits,
    body: impl FnOnce(&mut ModuleBuilder, &mut FunctionBuilder),
) -> Result<Value, VmError> {
    let p = load(module(params, results, body));
    let mut vm = Vm::new(&p);
    vm.run_with(FuncId(0), args, limits)
}

/// Runs a program's function 0 and returns its result together with the VM
/// (for inspecting objects), via a callback.
pub fn with_vm<R>(
    program: &Program,
    args: &[Value],
    f: impl FnOnce(&mut Vm<'_>, Result<Value, VmError>) -> R,
) -> R {
    let mut vm = Vm::new(program);
    let out = vm.run(FuncId(0), args);
    f(&mut vm, out)
}

/// Shorthand for the `dyn` value type.
pub const D: ValType = ValType::Dyn;
pub const I64: ValType = ValType::I64;
pub const BOOL: ValType = ValType::Bool;
pub const F64: ValType = ValType::F64;
pub const STR: ValType = ValType::Str;

pub mod full;
pub mod lspow;
pub mod reference;
