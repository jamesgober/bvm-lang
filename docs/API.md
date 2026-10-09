# bvm-lang &mdash; API Reference

> Complete reference for every public item in `bvm-lang`, with examples.
> **Status: 2.0.0-alpha.1, a pre-release.** The surface may still change before
> `2.0.0` (see [Stability](#stability) and [`STABILITY.md`](./STABILITY.md)).
> Instruction semantics are those of LSB (`_lexersketch/specs/LSB.md`) and OPS
> (`_lexersketch/specs/OPS.md`); this file documents the Rust API around them.

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

## Table of contents

- [Overview](#overview)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Concepts](#concepts)
  - [Registers and values](#registers-and-values)
  - [What loading checks](#what-loading-checks)
  - [Errors, traps, and codes](#errors-traps-and-codes)
  - [Fuel and the other budgets](#fuel-and-the-other-budgets)
  - [Hooks](#hooks)
  - [Maps as PHP arrays](#maps-as-php-arrays)
  - [Memory and collection](#memory-and-collection)
  - [Instruction coverage](#instruction-coverage)
- [`Program`](#program)
  - [`Program::load`](#programload)
  - [`Program::decode`](#programdecode)
  - [`Program::module`](#programmodule)
  - [`Program::export`](#programexport)
  - [`Program::location`](#programlocation)
- [`Vm`](#vm)
  - [`Vm::new`](#vmnew)
  - [`Vm::with_limits`](#vmwith_limits)
  - [`Vm::run`](#vmrun)
  - [`Vm::run_with`](#vmrun_with)
  - [`Vm::run_export`](#vmrun_export)
  - [Inspecting values](#inspecting-values)
  - [Heap and fuel statistics](#heap-and-fuel-statistics)
- [`Limits`](#limits)
- [`Value`](#value)
- [`Obj`](#obj)
- [`Host`](#host)
- [`HostCtx`](#hostctx)
- [`HostError`](#hosterror)
- [`VmError`](#vmerror)
- [`LoadError`](#loaderror)
- [`LoadErrorKind`](#loaderrorkind)
- [`Location`](#location)
- [Constants](#constants)
- [Feature flags](#feature-flags)
- [Stability](#stability)

## Overview

`bvm-lang` executes LSB modules. [`Program::load`](#programload) turns a
`bytecode_lang::Module` into a checked, import-bound [`Program`](#program); a
[`Vm`](#vm) runs its functions under [`Limits`](#limits) and returns
[`Value`](#value)s or a [`VmError`](#vmerror). Host functions, registered in a
[`Host`](#host), serve the module's imports and, through imports, its hooks.

## Installation

```toml
[dependencies]
bvm-lang = "=2.0.0-alpha.1"
bytecode-lang = "0.2"
```

## Quick start

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{ExportItem, Inst, IntOp, IntTy, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let mut f = m.function("add", &[ValType::I64, ValType::I64], &[ValType::I64]);
let r = f.reg(ValType::I64);
f.emit(Inst::IAdd { dst: r, lhs: f.param(0), rhs: f.param(1), op: IntOp::new(IntTy::I64) });
f.ret(r);
let add = m.add_function(f).unwrap();
m.export("add", ExportItem::Func(add));

let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&program);
assert_eq!(vm.run_export("add", &[Value::Int(40), Value::Int(2)]), Ok(Value::Int(42)));
```

## Concepts

### Registers and values

Every register is a 64-bit word; its declared LSB type gives it meaning.
Integers are stored sign- or zero-extended and read back narrowed to their
type; `f32` registers hold the float's bits; `bool` holds 0 or 1; `char` a
scalar value. `str`, `ref t`, and `dyn` registers share one encoding (LSB
§2.1), in which the zero word is `nil`:

| `dyn` kind | Encoding |
|---|---|
| `nil`, `bool`, `char` | immediates |
| `int` | inline when in [-2^48, 2^48), otherwise a boxed heap object; always 64-bit to the program |
| `float` | the `f64` bits offset by 2^49; NaN canonicalised (OPS §4) |
| heap objects | a 32-bit slot index and a 16-bit generation |

At the API boundary a register becomes a [`Value`](#value): signed integers
and `dyn` ints as `Value::Int`, unsigned integers as `Value::UInt`, `f64` and
`dyn` floats as `Value::Float`, `f32` as `Value::F32`, heap objects as
`Value::Obj`. Arguments convert the other way and must fit their parameter's
type (`VmError::ArgumentType` otherwise).

Conversions between `dyn` and typed registers (`to_dyn`, `from_dyn`, and every
place LSB applies their rules: `dcall` arguments and results, `set_prop` on
fields, dynamic indexing of typed collections) follow LSB §5.7. One reading
is worth stating: `from_dyn` into `str` accepts `nil` as well as strings,
because `nil` is a value of type `str` (LSB §2.1) and `to_dyn` of a `nil`
string must convert back.

### What loading checks

LSB's verifier arrives with bytecode-lang 0.5. Until then (and for modules
that skip it), [`Program::load`](#programload) proves in one linear pass
every fact the interpreter indexes by, so the dispatch loop never meets an
out-of-range index:

- every register operand, including every register of every call window
  (`call`, `dcall`, `make_closure`, `str_concat_n`, `str_slice`, ...), is
  inside the function's frame;
- every constant, function, import, global, jump-table, name, type, capture,
  and string index is in range; every branch, table, and handler target is an
  instruction;
- the last instruction cannot fall through; direct calls pass their callee's
  parameter count and never target a function with captures;
- `new_struct`/`new_array`/`new_map`/`new_cell` name types of that kind;
  `overflow = promote` appears only on instructions writing a `dyn` register;
  `from_dyn` never targets `ref`; bit casts use 32- or 64-bit integers;
- handlers are non-empty, in range, with `dyn` catch registers, and no tail
  call lies inside one;
- struct parents are structs whose fields prefix the child's, chains are
  acyclic and at most [`MAX_INHERITANCE_DEPTH`](#constants) deep; aggregate
  constants refer only to earlier ones and nest at most
  [`MAX_CONST_DEPTH`](#constants) deep;
- hook callees and the start function have the signatures LSB requires;
  every import has a registered host function.

It does **not** check the verifier's type rules. A module that, say, adds two
`str` registers with `iadd` loads and runs; it computes a meaningless word,
and heap instructions given such a word raise `TypeError` or
`NullReference`. Memory safety never depends on types: every heap access
checks the object's kind and liveness.

### Errors, traps, and codes

A failing instruction **raises** at its own pc without writing its
destination (LSB §4.3). The VM searches that function's handlers in order;
the first covering the pc receives the error value (a `dyn` of kind `error`,
or the thrown value) and execution continues at its target. Otherwise the
frame is popped and the search continues at the caller's call instruction.
An error leaving the entry frame ends the run with
[`VmError::Raised`](#vmerror) (runtime errors: kind, function, and pc of the
raising instruction) or [`VmError::Thrown`](#vmerror) (any other thrown
value).

**Traps** abort without visiting handlers: `OutOfFuel` (E0107),
`OutOfMemory` (E0106), `Unreachable` (E0109), and any OPS error under policy
`trap` (same code as the error). Codes are those of
`bytecode_lang::ErrorKind`; `err_code` reads them from caught error values.

### Fuel and the other budgets

One unit of fuel is charged at every `safepoint`, every call-family
instruction (`call`, `call_indirect`, `call_import`, `tail_call`,
`tail_call_indirect`, `dcall`, and every hook invocation), every taken branch
to a target at or before the branching instruction (`jmp`, `jmp_if`,
`jmp_if_not`, `switch`), and every handler entry. Between two charges a run
executes at most one function's length of instructions, so fuel bounds total
work even for modules a verifier would reject for lacking safepoints.

[`Limits`](#limits) also bounds heap bytes (the `OutOfMemory` trap; large
buffers are checked before they are requested), call depth (hook frames
included), and register-stack slots (both raise the catchable
`StackOverflow`, E0105, at the call).

### Hooks

The dynamic instructions take a built-in fast path (numbers, strings,
collections, structs) and otherwise call the module's hook for that
operation (LSB §5.8), bound to a module function or an import. A function
hook runs as a call (a frame, fuel, depth); an import hook runs the host
function. Results are converted as LSB states: `eq`, `lt`, `le`, `truthy`, and
`has_prop` must return a `dyn` bool (`TypeError` otherwise; `dne` negates),
`len` a `dyn` int, `iter` an array, map, or iterator. With no hook bound, each
instruction has its LSB fallback: `TypeError` for arithmetic, identity for
`deq`, the built-in rule for truthiness, `IndexOutOfBounds`/`KeyNotFound` for
indexing misses, `UndefinedProperty` for properties, `false` for `has_prop`.

### Maps as PHP arrays

A map keeps insertion order. Setting an existing key updates it in place; a
new key goes last. `map_push` inserts under the next integer key, one more
than the largest integer key ever inserted (PHP 8.3: after only `-5`, the
next is `-4`); deletions never lower it, and exhausting `i64` (or the key
type) raises `ArithOverflow`. Iteration follows the live map: deleted
entries are skipped and appended ones visited; iterating a `dup` gives PHP's
value semantics, at O(1) cost until either copy is written. Keys compare per
LSB §2.4: integers, bools, and chars by value, strings bytewise, other objects
by identity, floats by bits with `-0.0` folded into `+0.0` and one NaN, and
for `dyn` keys the kind is part of the key (`1` and `1.0` differ).

While a map's keys are exactly `0, 1, 2, ...` with nothing deleted it is
*packed*: no hash index exists and lookups are a bounds check.

### Memory and collection

Objects live in the VM's heap until unreachable. Collection is a mark and
sweep over the exact roots (reference-typed registers of every frame,
running closures, globals, and the constant and name caches). It runs only
at safepoints (`safepoint`, calls, and allocating instructions, LSB §5.9) and
only once as many bytes have been allocated as survived the previous
collection, so its total cost stays proportional to allocation. A
[`Value::Obj`](#obj) returned by a run stays valid until the next run on the
same VM; a handle to a collected object reads as `nil` (slots are
generation-checked and retired before a generation could wrap, so a stale
handle never aliases a new object).

### Instruction coverage

| LSB group | Opcodes | 2.0.0-alpha.1 |
|---|---|---|
| Moves, constants, globals (§5.1) | `0x00`-`0x0A` | implemented |
| Integer arithmetic and comparison (§5.2) | `0x10`-`0x26` | implemented, every policy |
| Float arithmetic and comparison (§5.3) | `0x30`-`0x47` | implemented |
| Booleans, chars, identity (§5.4) | `0x50`-`0x5A` | implemented |
| Conversions (§5.5) | `0x60`-`0x6E` | implemented |
| Dynamic arithmetic and comparison (§5.6) | `0x70`-`0x85`, `0x94` | implemented, `promote` included |
| Dynamic values, properties, calls (§5.7) | `0x86`-`0x93` | implemented |
| Control flow, calls, exceptions (§5.9) | `0xA0`-`0xAE` | implemented |
| Closures and cells (§5.10) | `0xB0`-`0xB4` | implemented |
| Typed heap objects (§5.10) | `0xC0`-`0xD4` | implemented |
| Strings (§5.11) | `0xE0`-`0xE6` | implemented |
| Coroutines (§5.13) | `0xF0`-`0xFC` | **unsupported**: loads, ends the run with `VmError::Unsupported` (alpha.2) |
| Hooks (§5.8) | codes 0-27 | all but `spawn` (27, coroutines) |

## `Program`

```rust,ignore
pub struct Program { /* private */ }
```

A loaded module: checked, bound to its host functions, and ready to run.
Immutable; `Send` and `Sync`, so one program can serve VMs on many threads.

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::ModuleBuilder;

let mut m = ModuleBuilder::new();
let mut f = m.function("f", &[], &[]);
f.ret_void();
let id = m.add_function(f).unwrap();
let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
std::thread::scope(|s| {
    for _ in 0..4 {
        let program = &program;
        s.spawn(move || assert!(Vm::new(program).run(id, &[]).is_ok()));
    }
});
```

### `Program::load`

```rust,ignore
pub fn load(module: Module, host: &Host) -> Result<Program, LoadError>
```

Checks `module` (see [What loading checks](#what-loading-checks)) and binds
each import to the function `host` registers under the import's module and
name. Linear in the module's size.

**Errors.** A [`LoadError`](#loaderror) naming the first problem, with the
function and pc where it applies.

```rust
use bvm_lang::{Host, LoadErrorKind, Program};
use bytecode_lang::{Inst, ModuleBuilder};

let mut m = ModuleBuilder::new();
let mut f = m.function("f", &[], &[]);
f.emit(Inst::Nop {}); // falls off the end
m.add_function(f).unwrap();
let err = Program::load(m.finish().unwrap(), &Host::new()).unwrap_err();
assert_eq!(err.kind(), &LoadErrorKind::FallsThrough);
assert_eq!(err.to_string(), "f0 @0: last instruction falls through");
```

### `Program::decode`

```rust,ignore
pub fn decode(bytes: &[u8], host: &Host) -> Result<Program, LoadError>
```

Decodes LSB bytes with bytecode-lang's default budgets, then loads.

**Errors.** [`LoadErrorKind::Decode`](#loaderrorkind) for malformed bytes;
otherwise as [`load`](#programload).

```rust
use bvm_lang::{Host, LoadErrorKind, Program};
use bytecode_lang::ModuleBuilder;

let bytes = bytecode_lang::encode(&ModuleBuilder::new().finish().unwrap());
assert!(Program::decode(&bytes, &Host::new()).is_ok());
let err = Program::decode(&bytes[..10], &Host::new()).unwrap_err();
assert!(matches!(err.kind(), LoadErrorKind::Decode(_)));
```

### `Program::module`

```rust,ignore
pub fn module(&self) -> &Module
```

The module the program was loaded from.

```rust
use bvm_lang::{Host, Program};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
assert!(p.module().functions().is_empty());
```

### `Program::export`

```rust,ignore
pub fn export(&self, name: &str) -> Option<FuncId>
```

The function exported under `name` (globals and types exported under that
name are not functions and give `None`).

```rust
use bvm_lang::{Host, Program};
use bytecode_lang::{ExportItem, ModuleBuilder};

let mut m = ModuleBuilder::new();
let mut f = m.function("main", &[], &[]);
f.ret_void();
let id = m.add_function(f).unwrap();
m.export("main", ExportItem::Func(id));
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
assert_eq!(p.export("main"), Some(id));
assert_eq!(p.export("other"), None);
```

### `Program::location`

```rust,ignore
pub fn location(&self, func: FuncId, pc: u32) -> Option<Location<'_>>
```

The source location of an instruction, from the module's line table: the row
with the greatest pc at or below `pc`. Pairs with
[`VmError::location`](#vmerror).

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::{Inst, ModuleBuilder};

let mut m = ModuleBuilder::new();
let file = m.string("app.mox");
let mut f = m.function("main", &[], &[]);
f.set_location(file, 12, 5);
f.emit(Inst::Unreachable {});
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let err = Vm::new(&p).run(id, &[]).unwrap_err();
let (func, pc) = err.location().unwrap();
assert_eq!(p.location(func, pc).unwrap().to_string(), "app.mox:12:5");
```

## `Vm`

```rust,ignore
pub struct Vm<'p> { /* private */ }
```

An instance of a [`Program`](#program): its heap, globals, default limits, and
pooled register stack. The first run initialises the globals from their
initialisers and runs the module's start function (if either fails, the next
run tries again); later runs see the globals and heap objects earlier runs
left. A `Vm` is `Send`.

### `Vm::new`

```rust,ignore
pub fn new(program: &'p Program) -> Vm<'p>
```

An instance with [`Limits::new`](#limits).

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let vm = Vm::new(&p);
assert_eq!(vm.limits(), bvm_lang::Limits::new());
```

### `Vm::with_limits`

```rust,ignore
pub fn with_limits(program: &'p Program, limits: Limits) -> Vm<'p>
pub fn limits(&self) -> Limits
pub fn set_limits(&mut self, limits: Limits)
pub fn program(&self) -> &'p Program
```

An instance whose runs use `limits` by default; `limits`/`set_limits` read and
replace them; `program` returns the program.

```rust
use bvm_lang::{Host, Limits, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::with_limits(&p, Limits::new().with_fuel(10_000));
assert_eq!(vm.limits().fuel(), 10_000);
vm.set_limits(Limits::new());
assert_eq!(vm.limits().fuel(), u64::MAX);
```

### `Vm::run`

```rust,ignore
pub fn run(&mut self, func: FuncId, args: &[Value]) -> Result<Value, VmError>
```

Calls `func` with `args` under the VM's limits. A void function returns
`Value::Nil`.

**Errors.** [`VmError`](#vmerror): `NoSuchFunction`, `NeedsClosure` (the
function has captures), `ArgumentCount`, `ArgumentType`, `GlobalInit`, or the
run's own outcome (`Raised`, `Thrown`, `Trap`, `Unsupported`).

```rust
use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{Inst, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let mut f = m.function("not", &[ValType::Bool], &[ValType::Bool]);
let r = f.reg(ValType::Bool);
f.emit(Inst::BNot { dst: r, src: f.param(0) });
f.ret(r);
let not = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
assert_eq!(vm.run(not, &[Value::Bool(true)]), Ok(Value::Bool(false)));
assert_eq!(vm.run(not, &[Value::Int(1)]), Err(VmError::ArgumentType { index: 0 }));
assert_eq!(vm.run(not, &[]), Err(VmError::ArgumentCount { expected: 1, found: 0 }));
```

### `Vm::run_with`

```rust,ignore
pub fn run_with(&mut self, func: FuncId, args: &[Value], limits: Limits) -> Result<Value, VmError>
```

As [`run`](#vmrun) under `limits` for this run only. Fuel starts at the
budget each run; the memory budget applies to the whole heap (including what
earlier runs left).

```rust
use bvm_lang::{Host, Limits, Program, Vm, VmError};
use bytecode_lang::{ErrorKind, Inst, ModuleBuilder};

let mut m = ModuleBuilder::new();
let mut f = m.function("f", &[], &[]);
let me = f.id();
let w = f.reg(bytecode_lang::ValType::Dyn);
f.emit(Inst::Call { dst: w, func: me, argc: 0 }); // unbounded recursion
f.ret_void();
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let err = Vm::new(&p).run_with(id, &[], Limits::new().with_depth(100)).unwrap_err();
assert_eq!(err, VmError::Raised { kind: ErrorKind::StackOverflow, func: id, pc: 0 });
```

### `Vm::run_export`

```rust,ignore
pub fn run_export(&mut self, name: &str, args: &[Value]) -> Result<Value, VmError>
```

Runs the function exported under `name`; `VmError::NoSuchExport` if none.
See [Quick start](#quick-start).

### Inspecting values

```rust,ignore
pub fn kind(&self, v: Value) -> Kind
pub fn str_bytes(&self, v: Value) -> Option<&[u8]>
pub fn elements(&self, v: Value) -> Option<Vec<Value>>
pub fn entries(&self, v: Value) -> Option<Vec<(Value, Value)>>
pub fn field(&self, v: Value, index: usize) -> Option<Value>
pub fn error_code(&self, v: Value) -> Option<u32>
pub fn global(&self, id: GlobalId) -> Option<Value>
pub fn new_str(&mut self, bytes: &[u8]) -> Result<Value, VmError>
```

Read what runs return: a value's dynamic kind, a string's bytes, an array's
elements, a map's entries in insertion order, a struct's field, a runtime
error value's code, a global's current value; `new_str` allocates a string to
pass to a run (the `OutOfMemory` trap if the budget is spent).

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{Const, Inst, Kind, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let k = m.constant(Const::Bytes(b"k".to_vec()));
let v = m.constant(Const::Int(5));
let map = m.constant(Const::Map(vec![(k, v)]));
let mut f = m.function("f", &[], &[ValType::Dyn]);
let r = f.reg(ValType::Dyn);
f.emit(Inst::DLoadConst { dst: r, k: map });
f.ret(r);
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
let out = vm.run(id, &[]).unwrap();
assert_eq!(vm.kind(out), Kind::Map);
let entries = vm.entries(out).unwrap();
assert_eq!(vm.str_bytes(entries[0].0), Some(&b"k"[..]));
assert_eq!(entries[0].1, Value::Int(5));
```

### Heap and fuel statistics

```rust,ignore
pub fn fuel_used(&self) -> u64
pub fn heap_bytes(&self) -> usize
pub fn heap_objects(&self) -> usize
pub fn collections(&self) -> u64
pub fn collect_garbage(&mut self)
```

Fuel the last run consumed; bytes charged to the memory budget; live objects
(exact after a collection); collections so far; and an explicit collection
keeping only what globals and caches reach.

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
let s = vm.new_str(b"temporary").unwrap();
assert_eq!(vm.heap_objects(), 1);
vm.collect_garbage();
assert_eq!((vm.heap_objects(), vm.collections()), (0, 1));
assert_eq!(vm.kind(s), bytecode_lang::Kind::Nil); // the stale handle reads as nil
```

## `Limits`

```rust,ignore
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits { /* private */ }
impl Limits {
    pub const fn new() -> Limits;
    pub const fn with_fuel(self, fuel: u64) -> Limits;
    pub const fn with_memory(self, bytes: usize) -> Limits;
    pub const fn with_depth(self, frames: usize) -> Limits;   // at least 1
    pub const fn with_stack(self, slots: usize) -> Limits;
    pub const fn fuel(&self) -> u64;
    pub const fn memory(&self) -> usize;
    pub const fn depth(&self) -> usize;
    pub const fn stack(&self) -> usize;
}
```

The budgets of a run (see [Fuel and the other budgets](#fuel-and-the-other-budgets)).
Defaults (`new`, `Default`): unlimited fuel, 1 GiB of heap, 10,000 frames,
4 Mi register slots (32 MiB). Set fuel and memory for untrusted code; the
memory budget is only as hard as the host can honour, so keep it below the
memory actually available.

```rust
use bvm_lang::Limits;

let l = Limits::default().with_fuel(1_000).with_memory(64 << 20).with_depth(256).with_stack(1 << 20);
assert_eq!((l.fuel(), l.memory(), l.depth(), l.stack()), (1_000, 64 << 20, 256, 1 << 20));
```

## `Value`

```rust,ignore
#[non_exhaustive]
pub enum Value { Nil, Bool(bool), Int(i64), UInt(u64), F32(f32), Float(f64), Char(char), Obj(Obj) }
```

A register's contents at the API boundary (see
[Registers and values](#registers-and-values)). `as_int` (also unsigned values
that fit), `as_float` (also `F32`), `as_bool`, and `is_nil` read it;
`Display` prints scalars and `<obj>`; `From` converts `bool`, `i64`, `u64`,
`f64`, and `char`.

```rust
use bvm_lang::Value;

assert_eq!(Value::from(3i64).as_int(), Some(3));
assert_eq!(Value::UInt(9).as_int(), Some(9));
assert_eq!(Value::F32(0.5).as_float(), Some(0.5));
assert_eq!(Value::Bool(true).to_string(), "true");
assert!(Value::default().is_nil());
```

## `Obj`

```rust,ignore
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Obj(/* private */);
```

A handle to a heap object of one [`Vm`](#vm); equality is identity. Valid
until the next run on that VM, or until collected (then it reads as `nil`).

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
let (a, b) = (vm.new_str(b"x").unwrap(), vm.new_str(b"x").unwrap());
assert!(matches!((a, b), (Value::Obj(x), Value::Obj(y)) if x != y));
```

## `Host`

```rust,ignore
#[derive(Clone, Default)]
pub struct Host { /* private */ }
impl Host {
    pub fn new() -> Host;
    pub fn register<F>(&mut self, module: &str, name: &str, f: F) -> &mut Host
    where F: Fn(&mut HostCtx<'_>, &[Value]) -> Result<Value, HostError> + Send + Sync + 'static;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

Host functions by import module and name. A host function receives its
arguments converted from the import's declared parameter types; its result is
converted to the declared result type (`TypeError` at the call if it does not
fit; ignored for a void import). It runs to completion without re-entering the
VM, so no collection happens during it.

```rust
use bvm_lang::{Host, HostError, Program, Value, Vm};
use bytecode_lang::{ErrorKind, Inst, ModuleBuilder, ValType};

let mut host = Host::new();
host.register("math", "isqrt", |_, args| match args {
    [Value::Int(n)] if *n >= 0 => Ok(Value::Int(n.isqrt())),
    _ => Err(HostError::Raise(ErrorKind::TypeError)),
});
let mut m = ModuleBuilder::new();
let sig = m.func_type(&[ValType::I64], &[ValType::I64]);
let isqrt = m.import("math", "isqrt", sig);
let mut f = m.function("f", &[ValType::I64], &[ValType::I64]);
let w = f.regs(&[ValType::I64, ValType::I64]);
f.mov(bytecode_lang::Reg(w.0 + 1), f.param(0));
f.emit(Inst::CallImport { dst: w, import: isqrt, argc: 1 });
f.ret(w);
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &host).unwrap();
assert_eq!(Vm::new(&p).run(id, &[Value::Int(99)]), Ok(Value::Int(9)));
```

## `HostCtx`

```rust,ignore
pub struct HostCtx<'a> { /* private */ }
impl HostCtx<'_> {
    pub fn str_bytes(&self, v: Value) -> Option<&[u8]>;
    pub fn new_str(&mut self, bytes: &[u8]) -> Result<Value, HostError>;
    pub fn kind(&self, v: Value) -> Kind;
    pub fn error_code(&self, v: Value) -> Option<u32>;
}
```

What a host function can do with the VM while it runs. `new_str` fails with
`HostError::Raise(ErrorKind::OutOfMemory)` (a trap) when the budget is spent.

```rust
use bvm_lang::{Host, Value};

let mut host = Host::new();
host.register("str", "upper", |ctx, args| {
    let s = args.first().and_then(|v| ctx.str_bytes(*v)).unwrap_or(b"").to_ascii_uppercase();
    ctx.new_str(&s)
});
```

## `HostError`

```rust,ignore
#[non_exhaustive]
pub enum HostError { Raise(ErrorKind), Throw(Value) }
```

`Raise` raises a runtime error of that kind at the calling instruction
(catchable unless the kind is `OutOfMemory`, `OutOfFuel`, or `Unreachable`,
which trap); `Throw` throws a value, as `throw` would.

```rust
use bvm_lang::{HostError, Value};
use bytecode_lang::ErrorKind;

assert_eq!(HostError::Raise(ErrorKind::DivByZero).to_string(), "host raised E0002 DivByZero");
assert_eq!(HostError::Throw(Value::Nil).to_string(), "host threw nil");
```

## `VmError`

```rust,ignore
#[non_exhaustive]
pub enum VmError {
    Raised { kind: ErrorKind, func: FuncId, pc: u32 },
    Thrown { value: Value, func: FuncId, pc: u32 },
    Trap { kind: ErrorKind, func: FuncId, pc: u32 },
    Unsupported { opcode: Opcode, func: FuncId, pc: u32 },
    GlobalInit { global: GlobalId, kind: ErrorKind },
    NoSuchFunction(FuncId),
    NoSuchExport,
    NeedsClosure(FuncId),
    ArgumentCount { expected: usize, found: usize },
    ArgumentType { index: usize },
}
impl VmError {
    pub fn kind(&self) -> Option<ErrorKind>;
    pub fn code(&self) -> Option<u32>;
    pub fn location(&self) -> Option<(FuncId, u32)>;
}
```

How a run ended without a result. `Raised` is an uncaught runtime error (its
location is where it was raised, even if a handler caught and rethrew it);
`Thrown` an uncaught non-error value; `Trap` a trap; `Unsupported` a coroutine
instruction (alpha.2); `GlobalInit` a global initialiser that does not fit
its global; the rest are entry problems. `kind`, `code`, and `location` read
the common parts.

```rust
use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{Inst, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let mut f = m.function("f", &[ValType::Dyn], &[]);
f.emit(Inst::Throw { src: f.param(0) });
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let err = Vm::new(&p).run(id, &[Value::Int(7)]).unwrap_err();
assert_eq!(err, VmError::Thrown { value: Value::Int(7), func: id, pc: 0 });
assert_eq!((err.kind(), err.location()), (None, Some((id, 0))));
```

## `LoadError`

```rust,ignore
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError { /* private */ }
impl LoadError {
    pub fn kind(&self) -> &LoadErrorKind;
    pub fn func(&self) -> Option<FuncId>;
    pub fn pc(&self) -> Option<u32>;
}
```

Why [`Program::load`](#programload) refused a module, and where. `Display`
prints `f<id> @<pc>: <reason>`.

## `LoadErrorKind`

```rust,ignore
#[non_exhaustive]
pub enum LoadErrorKind {
    Decode(DecodeError),
    UnresolvedImport { module: String, name: String },
    OutOfRange { what: &'static str, index: u32 },
    BadSignature, ParamMismatch, EmptyCode, FallsThrough,
    ArityMismatch { expected: u32, found: u32 },
    CalleeHasCaptures, PromoteNotDynamic,
    WrongTypeKind { expected: &'static str },
    BadModifier, InvalidHandler, CatchNotDyn, TailCallInTry,
    BadParent, InheritanceTooDeep, BadConstant,
    BadHook(Hook), BadStart,
}
```

Each variant is one rule of [What loading checks](#what-loading-checks);
`OutOfRange::what` is one of `"register"`, `"constant"`, `"function"`,
`"import"`, `"global"`, `"table"`, `"name"`, `"type ref"`, `"capture"`,
`"branch target"`, `"table target"`, `"type"`, `"string"`.

```rust
use bvm_lang::{Host, LoadErrorKind, Program};
use bytecode_lang::{Callee, Hook, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let mut h = m.function("h", &[ValType::Dyn], &[ValType::Dyn]);
h.ret(h.param(0));
let id = m.add_function(h).unwrap();
m.hook(Hook::Eq, Callee::Func(id)); // `eq` takes two operands
let err = Program::load(m.finish().unwrap(), &Host::new()).unwrap_err();
assert_eq!(err.kind(), &LoadErrorKind::BadHook(Hook::Eq));
```

## `Location`

```rust,ignore
pub struct Location<'a> { pub file: &'a str, pub line: u32, pub column: u32 }
```

A line-table position; `Display` prints `file:line:column`. See
[`Program::location`](#programlocation).

## Constants

```rust,ignore
pub const MAX_INHERITANCE_DEPTH: usize = 256;
pub const MAX_CONST_DEPTH: u32 = 64;
```

The deepest struct inheritance chain and aggregate-constant nesting a module
may declare.

```rust
assert_eq!(bvm_lang::MAX_INHERITANCE_DEPTH, 256);
assert_eq!(bvm_lang::MAX_CONST_DEPTH, 64);
```

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `std` | yes | Float `sqrt`/`fma`/rounding from the standard library (hardware where available), and a random per-VM hashing key. Without it the crate is `no_std` + `alloc`, computes the same float results in software (property-tested equal), and uses a fixed key. |

## Stability

2.0.0-alpha.1 is a pre-release: names and signatures may change before 2.0.0,
each change recorded in the CHANGELOG. Instruction semantics follow LSB and
OPS and change only with them. See [`STABILITY.md`](./STABILITY.md).
