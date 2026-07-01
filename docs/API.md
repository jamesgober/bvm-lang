<h1 align="center" id="top">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>bvm-lang</b><br>
    <sub><sup>API REFERENCE</sup></sub>
</h1>
<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>API</span>
        <span>&nbsp;│&nbsp;</span>
        <a href="../CHANGELOG.md" title="Changelog"><b>CHANGELOG</b></a>
    </sup>
</div>
<br>

> **Status: stable (`v1.0.0`).** The public surface documented here is **frozen** under Semantic Versioning: no breaking change until a `2.0.0`. See [`STABILITY.md`](./STABILITY.md) for the exact frozen surface and the compatibility promise.

`bvm-lang` is a register-based bytecode virtual machine. You assemble a program as a [`Chunk`](#chunk) of [`Op`](#op) instructions over a constant pool, then execute it with a [`Vm`](#vm), which returns a [`Value`](#value). Every register holds one `Value` &mdash; the eight-byte NaN-boxed type from [`value-lang`](https://docs.rs/value-lang). Bytecode is treated as untrusted input: a malformed program yields a typed [`VmError`](#vmerror), never a panic, and the crate forbids `unsafe`.

<br>

## Table of Contents

- **[Installation](#installation)**
- **[Quick Start](#quick-start)**
- **[Execution Model](#execution-model)**
- **[Public API](#public-api)**
  - [`Value`, `Unpacked`, `Symbol`](#value)
  - [`Op`](#op)
  - [Type aliases: `Reg`, `Const`, `Addr`](#type-aliases)
  - [`Chunk`](#chunk)
  - [`Vm`](#vm)
  - [`VmError`](#vmerror)
- **[Instruction Reference](#instruction-reference)**
- **[Semantics](#semantics)**
- **[Worked Examples](#worked-examples)**
- **[Feature Flags](#feature-flags)**
- **[Example Pointers](#example-pointers)**

<br>
<hr>

## Installation

Add the crate to your `Cargo.toml`:

```toml
[dependencies]
bvm-lang = "0.2"
```

Or from the terminal:

```bash
cargo add bvm-lang
```

The default build depends only on `value-lang` (and its interner). To persist compiled bytecode, enable `serde`:

```toml
[dependencies]
bvm-lang = { version = "0.2", features = ["serde"] }
```

The crate is `no_std`-compatible (it needs `alloc`); disable default features to drop `std`.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Quick Start

Compile and run `(2 + 3) * 4`:

```rust
use bvm_lang::{Chunk, Op, Vm};

let mut chunk = Chunk::new();
chunk.emit(Op::LoadInt { dst: 0, val: 2 });
chunk.emit(Op::LoadInt { dst: 1, val: 3 });
chunk.emit(Op::Add { dst: 0, lhs: 0, rhs: 1 }); // r0 = 2 + 3
chunk.emit(Op::LoadInt { dst: 1, val: 4 });
chunk.emit(Op::Mul { dst: 0, lhs: 0, rhs: 1 }); // r0 = 5 * 4
chunk.emit(Op::Return { src: 0 });

let mut vm = Vm::new();
let result = vm.run(&chunk).expect("well-formed program");
assert_eq!(result.as_int(), Some(20));
```

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Execution Model

`bvm-lang` is a **register machine**. Unlike a stack machine, which pushes and pops an implicit operand stack, every instruction names the registers it reads and writes. A three-address `Add { dst, lhs, rhs }` computes `dst = lhs + rhs` in one instruction, where a stack machine would issue push/push/add/store. Fewer instructions means fewer trips through the dispatch loop.

- **Registers** are the VM's working storage, indexed from zero. A chunk's register file is sized automatically to the highest index any of its instructions names, so you never declare it.
- **The constant pool** holds `Value`s too large or too varied to encode inline (arbitrary floats, interned symbols). Instructions read them by index with `LoadConst`.
- **The program counter** walks the code array. Branches set it to an absolute address; every other instruction falls through to the next.
- **Termination** is explicit: `Return` yields a register's value, `Halt` yields `nil`. Running off the end of the code is a fault, not an implicit return.

A `Vm` owns the register file and reuses it across runs, so executing many chunks (or the same chunk many times) does not reallocate. A `Chunk` is immutable execution input and holds no VM state, so one chunk can be run on several threads by separate `Vm`s.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Public API

<h3 id="value"><code>Value</code>, <code>Unpacked</code>, <code>Symbol</code></h3>

The runtime value type is re-exported from [`value-lang`](https://docs.rs/value-lang). A [`Value`] is an eight-byte, `Copy` NaN-boxed handle that represents one of five kinds: `nil`, a boolean, a 32-bit integer, a 64-bit float, or an interned [`Symbol`]. Every register holds one.

Construct values with the associated functions, and read them back with the `as_*` accessors (which return `Option`) or by matching on [`Unpacked`]:

```rust
use bvm_lang::{Unpacked, Value};

let a = Value::int(7);
let b = Value::float(2.5);
let c = Value::bool(true);
let d = Value::nil();

assert_eq!(a.as_int(), Some(7));
assert_eq!(b.as_float(), Some(2.5));

// Match on every kind at once.
assert_eq!(b.unpack(), Unpacked::Float(2.5));
```

| Constructor | Kind |
| --- | --- |
| `Value::nil()` | the unit value |
| `Value::bool(b)` | a boolean |
| `Value::int(i32)` | a 32-bit signed integer |
| `Value::float(f64)` | a double-precision float |
| `Value::sym(Symbol)` | an interned symbol handle |

See the [`value-lang` documentation](https://docs.rs/value-lang) for the full accessor and predicate set. `bvm-lang` produces and consumes these values but does not add methods to them.

<br>

<h3 id="op"><code>Op</code></h3>

A single decoded instruction. `Op` is `Copy` and occupies 8 bytes; a program is a slice of them. The variants group into data movement, arithmetic, comparison, logic, control flow, and termination. Fields are register indices ([`Reg`](#type-aliases)), a constant index ([`Const`](#type-aliases)), a branch target ([`Addr`](#type-aliases)), or an inline immediate.

Construct instructions directly with struct-variant syntax and hand them to [`Chunk::emit`](#chunk):

```rust
use bvm_lang::{Chunk, Op};

let mut chunk = Chunk::new();
chunk.emit(Op::LoadInt { dst: 0, val: 10 });
chunk.emit(Op::LoadInt { dst: 1, val: 4 });
chunk.emit(Op::Sub { dst: 0, lhs: 0, rhs: 1 }); // r0 = 10 - 4
chunk.emit(Op::Return { src: 0 });
```

The full variant list and per-instruction semantics are in the [Instruction Reference](#instruction-reference) below. `Op` is `#[non_exhaustive]`: future minor versions may add instructions, so a `match` over it needs a wildcard arm.

<br>

<h3 id="type-aliases">Type aliases: <code>Reg</code>, <code>Const</code>, <code>Addr</code></h3>

Three aliases name the roles operands play. They document intent at call sites; all three are plain integers.

| Alias | Underlying | Role |
| --- | --- | --- |
| `Reg` | `u16` | a register index (up to 65 536 registers per chunk) |
| `Const` | `u16` | a constant-pool index, used by `LoadConst` |
| `Addr` | `u32` | an absolute instruction address, used as a branch target |

```rust
use bvm_lang::{Addr, Const, Reg};

let dst: Reg = 0;
let index: Const = 3;
let target: Addr = 12;
```

<br>

<h3 id="chunk"><code>Chunk</code></h3>

An assembled program: its instructions, its constant pool, and the size of the register file they operate on. A `Chunk` is what you hand to [`Vm::run`](#vm). It derives `Clone`, `Debug`, `Default`, and `PartialEq` (and `Serialize`/`Deserialize` under the `serde` feature).

The register-file size is tracked for you: every emitted instruction widens the file to cover the highest register it names, so a chunk built through this API can never address a register outside its own file.

#### Constructors

**`Chunk::new() -> Chunk`** &mdash; an empty chunk: no instructions, no constants, a zero-width register file.

```rust
use bvm_lang::Chunk;

let chunk = Chunk::new();
assert!(chunk.is_empty());
assert_eq!(chunk.registers(), 0);
```

#### Building

**`emit(&mut self, op: Op) -> Addr`** &mdash; append `op` and return its address (its index in the code array). The address is an [`Addr`](#type-aliases), so it feeds straight into a `Jump`/`patch` target with no cast. That address is what a branch targets and what [`patch`](#patch) rewrites.

```rust
use bvm_lang::{Chunk, Op};

let mut chunk = Chunk::new();
let addr = chunk.emit(Op::LoadInt { dst: 0, val: 41 });
assert_eq!(addr, 0);
assert_eq!(chunk.registers(), 1); // naming r0 sized the file to one slot
```

**`constant(&mut self, value: Value) -> Option<u16>`** &mdash; add `value` to the constant pool and return its index for [`LoadConst`](#instruction-reference). Constants are not deduplicated. Returns `None` only if the pool is already at its 65 536-entry maximum.

```rust
use bvm_lang::{Chunk, Op, Value};

let mut chunk = Chunk::new();
let k = chunk.constant(Value::float(3.5)).expect("pool has room");
chunk.emit(Op::LoadConst { dst: 0, index: k });
```

**`patch(&mut self, addr: Addr, op: Op) -> bool`** &mdash; overwrite the instruction at `addr`, typically to fill in a forward branch whose target was unknown when it was first emitted. Returns `true` if `addr` was in range. The register file is re-derived after a patch.

The forward-branch pattern &mdash; emit a placeholder, remember its address, patch it once the landing is known:

```rust
use bvm_lang::{Chunk, Op};

let mut chunk = Chunk::new();
chunk.emit(Op::LoadBool { dst: 0, val: false });
let branch = chunk.emit(Op::JumpIfFalse { cond: 0, target: 0 }); // target unknown
chunk.emit(Op::LoadInt { dst: 1, val: 1 }); // skipped when r0 is false
let landing = chunk.emit(Op::Return { src: 1 }); // emit returns the landing address
assert!(chunk.patch(branch, Op::JumpIfFalse { cond: 0, target: landing }));
```

#### Inspection

| Method | Returns | Meaning |
| --- | --- | --- |
| `code(&self)` | `&[Op]` | the instructions, in address order |
| `constants(&self)` | `&[Value]` | the constant pool |
| `registers(&self)` | `u16` | register-file size (highest index named + 1) |
| `len(&self)` | `usize` | number of instructions |
| `is_empty(&self)` | `bool` | whether there are no instructions |

<br>

<h3 id="vm"><code>Vm</code></h3>

The interpreter. A `Vm` owns a register file that is reset and reused on every run, so a long-lived instance executing many chunks reaches an allocation-free steady state.

#### Constructors

**`Vm::new() -> Vm`** &mdash; a VM with an empty register file. The file grows to fit the first chunk and is reused thereafter.

**`Vm::with_capacity(registers: u16) -> Vm`** &mdash; a VM whose register file is pre-allocated for at least `registers` slots, avoiding a growth reallocation on the first run of a chunk that size.

```rust
use bvm_lang::Vm;

let mut vm = Vm::new();
let mut primed = Vm::with_capacity(64);
```

#### Execution

**`run(&mut self, chunk: &Chunk) -> Result<Value, VmError>`** &mdash; execute `chunk` from its first instruction and return the value it yields. A `Return` yields its register's value; a `Halt` yields `nil`. The register file is reset to `nil` before execution, so a run never observes residue from a previous one.

```rust
use bvm_lang::{Chunk, Op, Vm};

let mut chunk = Chunk::new();
chunk.emit(Op::LoadInt { dst: 0, val: 6 });
chunk.emit(Op::LoadInt { dst: 1, val: 7 });
chunk.emit(Op::Mul { dst: 0, lhs: 0, rhs: 1 });
chunk.emit(Op::Return { src: 0 });

let mut vm = Vm::new();
assert_eq!(vm.run(&chunk).unwrap().as_int(), Some(42));
```

Faults are returned, not panicked:

```rust
use bvm_lang::{Chunk, Op, Vm, VmError};

let mut chunk = Chunk::new();
chunk.emit(Op::LoadInt { dst: 0, val: 1 });
chunk.emit(Op::LoadInt { dst: 1, val: 0 });
chunk.emit(Op::Div { dst: 0, lhs: 0, rhs: 1 });
chunk.emit(Op::Return { src: 0 });

let mut vm = Vm::new();
assert_eq!(vm.run(&chunk), Err(VmError::DivideByZero));
```

One VM, many chunks &mdash; the register file is reused across calls:

```rust
use bvm_lang::{Chunk, Op, Vm};

let mut vm = Vm::new();
for n in 1..=3 {
    let mut chunk = Chunk::new();
    chunk.emit(Op::LoadInt { dst: 0, val: n });
    chunk.emit(Op::Return { src: 0 });
    assert_eq!(vm.run(&chunk).unwrap().as_int(), Some(n));
}
```

<br>

<h3 id="vmerror"><code>VmError</code></h3>

Every way a run can fail. `VmError` derives `Debug`, `Clone`, `PartialEq`, `Eq`, implements `Display`, and implements `core::error::Error`. It is `#[non_exhaustive]`.

Errors split into two groups. **Runtime faults** come from executing a well-formed instruction against operands that break its contract. **Structural faults** mean the bytecode itself is malformed &mdash; a correct compiler never emits them, but corrupt or hostile input can, and the VM reports rather than trusts them.

| Variant | Group | Raised when |
| --- | --- | --- |
| `TypeMismatch { op: &'static str }` | runtime | an operand has the wrong kind (e.g. adding a bool, branching on a non-bool). `op` names the operation. |
| `DivideByZero` | runtime | integer `Div`/`Rem` with a zero divisor. |
| `IntegerOverflow` | runtime | a checked integer operation overflowed `i32`. |
| `BadRegister(u16)` | structural | a register index addressed a slot outside the file. |
| `BadConstant(u16)` | structural | a `LoadConst` referenced a missing pool slot. |
| `BadJump(u32)` | structural | a branch target was outside the code. |
| `NoTerminator` | structural | control reached the end of the code without `Return`/`Halt`. |

Handling faults explicitly:

```rust
use bvm_lang::{Chunk, Op, Vm, VmError};

let mut chunk = Chunk::new();
chunk.emit(Op::LoadBool { dst: 0, val: true });
chunk.emit(Op::LoadInt { dst: 1, val: 1 });
chunk.emit(Op::Add { dst: 0, lhs: 0, rhs: 1 });
chunk.emit(Op::Return { src: 0 });

match Vm::new().run(&chunk) {
    Ok(value) => println!("result: {value:?}"),
    Err(VmError::TypeMismatch { op }) => println!("`{op}` got a bad operand"),
    Err(other) => println!("failed: {other}"),
}
```

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Instruction Reference

All operands are register indices unless noted. `dst` is written; `lhs`/`rhs`/`src`/`cond` are read.

### Data movement

| Instruction | Effect |
| --- | --- |
| `Move { dst, src }` | `dst = src` |
| `LoadConst { dst, index }` | `dst = constants[index]` |
| `LoadNil { dst }` | `dst = nil` |
| `LoadBool { dst, val }` | `dst = val` (inline `bool`) |
| `LoadInt { dst, val }` | `dst = val` (inline `i32`) |

### Arithmetic (numeric operands)

| Instruction | Effect | Faults |
| --- | --- | --- |
| `Add { dst, lhs, rhs }` | `dst = lhs + rhs` | `IntegerOverflow`, `TypeMismatch` |
| `Sub { dst, lhs, rhs }` | `dst = lhs - rhs` | `IntegerOverflow`, `TypeMismatch` |
| `Mul { dst, lhs, rhs }` | `dst = lhs * rhs` | `IntegerOverflow`, `TypeMismatch` |
| `Div { dst, lhs, rhs }` | `dst = lhs / rhs` | `DivideByZero`, `IntegerOverflow`, `TypeMismatch` |
| `Rem { dst, lhs, rhs }` | `dst = lhs % rhs` | `DivideByZero`, `IntegerOverflow`, `TypeMismatch` |
| `Neg { dst, src }` | `dst = -src` | `IntegerOverflow`, `TypeMismatch` |

### Comparison (numeric operands, boolean result)

| Instruction | Effect |
| --- | --- |
| `Eq { dst, lhs, rhs }` | `dst = (lhs == rhs)` |
| `Ne { dst, lhs, rhs }` | `dst = (lhs != rhs)` |
| `Lt { dst, lhs, rhs }` | `dst = (lhs < rhs)` |
| `Le { dst, lhs, rhs }` | `dst = (lhs <= rhs)` |
| `Gt { dst, lhs, rhs }` | `dst = (lhs > rhs)` |
| `Ge { dst, lhs, rhs }` | `dst = (lhs >= rhs)` |

`Eq`/`Ne` accept any operands (numbers compare by value; other kinds compare within their kind). The four orderings require numeric operands and fault with `TypeMismatch` otherwise.

### Logic

| Instruction | Effect | Faults |
| --- | --- | --- |
| `Not { dst, src }` | `dst = !src` (`src` must be a bool) | `TypeMismatch` |

### Control flow (targets are absolute addresses)

| Instruction | Effect |
| --- | --- |
| `Jump { target }` | `pc = target` |
| `JumpIfTrue { cond, target }` | `pc = target` if `cond` is `true`, else fall through |
| `JumpIfFalse { cond, target }` | `pc = target` if `cond` is `false`, else fall through |

`cond` must be a boolean; otherwise `TypeMismatch`. A target outside the code is `BadJump`.

### Termination

| Instruction | Effect |
| --- | --- |
| `Return { src }` | stop, yielding the value in `src` |
| `Halt` | stop, yielding `nil` |

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Semantics

**Numeric tower.** Arithmetic and ordering treat integers and floats as one tower. Two integers produce an integer; if either operand is a float, the result is a float. Integer results are **overflow-checked** &mdash; a fault is reported rather than a silent wrap.

```rust
use bvm_lang::{Chunk, Op, Vm, VmError};

// i32::MAX + 1 does not wrap; it faults.
let mut chunk = Chunk::new();
chunk.emit(Op::LoadInt { dst: 0, val: i32::MAX });
chunk.emit(Op::LoadInt { dst: 1, val: 1 });
chunk.emit(Op::Add { dst: 0, lhs: 0, rhs: 1 });
chunk.emit(Op::Return { src: 0 });
assert_eq!(Vm::new().run(&chunk), Err(VmError::IntegerOverflow));
```

**Division by zero.** Integer `Div`/`Rem` by zero is a `DivideByZero` fault. Float division by zero follows IEEE-754 and yields an infinity or NaN, not a fault.

**Mixed operands promote.** An integer combined with a float widens to float:

```rust
use bvm_lang::{Chunk, Op, Value, Vm};

// 1 + 0.5 = 1.5
let mut chunk = Chunk::new();
let half = chunk.constant(Value::float(0.5)).unwrap();
chunk.emit(Op::LoadInt { dst: 0, val: 1 });
chunk.emit(Op::LoadConst { dst: 1, index: half });
chunk.emit(Op::Add { dst: 0, lhs: 0, rhs: 1 });
chunk.emit(Op::Return { src: 0 });
assert_eq!(Vm::new().run(&chunk).unwrap().as_float(), Some(1.5));
```

**Equality.** `Eq` compares numbers by value across int and float (`1` equals `1.0`), and compares other kinds within their kind (`nil` to `nil`, bool to bool, symbol to symbol) &mdash; never equal across kinds. Float equality is IEEE-754, so `NaN` equals nothing, including itself.

**Conditions are booleans.** `Not`, `JumpIfTrue`, and `JumpIfFalse` require a boolean operand and fault with `TypeMismatch` on anything else. There is no implicit truthiness.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Worked Examples

### A counted loop

Sum `1..=5` with a back-edge and a back-patched exit branch (result: `15`):

```rust
use bvm_lang::{Chunk, Op, Vm};

let mut chunk = Chunk::new();
chunk.emit(Op::LoadInt { dst: 0, val: 0 }); // sum
chunk.emit(Op::LoadInt { dst: 1, val: 1 }); // i
chunk.emit(Op::LoadInt { dst: 2, val: 5 }); // limit
chunk.emit(Op::LoadInt { dst: 3, val: 1 }); // step
let top = chunk.emit(Op::Le { dst: 4, lhs: 1, rhs: 2 });
let exit_branch = chunk.emit(Op::JumpIfFalse { cond: 4, target: 0 });
chunk.emit(Op::Add { dst: 0, lhs: 0, rhs: 1 }); // sum += i
chunk.emit(Op::Add { dst: 1, lhs: 1, rhs: 3 }); // i += 1
chunk.emit(Op::Jump { target: top });           // back-edge (top is already an Addr)
let exit = chunk.emit(Op::Return { src: 0 });
chunk.patch(exit_branch, Op::JumpIfFalse { cond: 4, target: exit });

assert_eq!(Vm::new().run(&chunk).unwrap().as_int(), Some(15));
```

### A branch on a comparison

`if 3 == 4 then 10 else 20` (result: `20`):

```rust
use bvm_lang::{Chunk, Op, Vm};

let mut chunk = Chunk::new();
chunk.emit(Op::LoadInt { dst: 0, val: 3 });
chunk.emit(Op::LoadInt { dst: 1, val: 4 });
chunk.emit(Op::Eq { dst: 2, lhs: 0, rhs: 1 });      // r2 = (3 == 4) = false
chunk.emit(Op::JumpIfFalse { cond: 2, target: 6 }); // false -> else
chunk.emit(Op::LoadInt { dst: 0, val: 10 });
chunk.emit(Op::Return { src: 0 });
chunk.emit(Op::LoadInt { dst: 0, val: 20 });        // address 6
chunk.emit(Op::Return { src: 0 });

assert_eq!(Vm::new().run(&chunk).unwrap().as_int(), Some(20));
```

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Feature Flags

| Feature | Default | Effect |
| --- | --- | --- |
| `std` | yes | links `std`; forwards `std` to `value-lang`. Disable for `no_std` (the crate still needs `alloc`). |
| `serde` | no | derives `Serialize`/`Deserialize` for `Op` and `Chunk`, so compiled bytecode can be persisted and reloaded. |

With `serde`, a chunk round-trips through any serde format:

```rust,ignore
let json = serde_json::to_string(&chunk)?;
let restored: bvm_lang::Chunk = serde_json::from_str(&json)?;
```

The serialized form uses serde's default externally-tagged encoding, so the `Op` variant names and field names (`dst`, `src`, `lhs`, `rhs`, `index`, `val`, `target`, `cond`) are part of the format. That representation is stable within the `1.x` series for the instructions available at serialization time; a newer instruction added in a later `1.x` will not deserialize on an older version. See [`STABILITY.md`](./STABILITY.md).

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Example Pointers

Runnable programs in [`examples/`](../examples):

- `expression.rs` &mdash; assemble and evaluate `(2 + 3) * 4 - 10 / 2`. Run: `cargo run --example expression`.
- `fibonacci.rs` &mdash; iterative Fibonacci with a loop and a back-patched exit branch. Run: `cargo run --example fibonacci`.
- `errors.rs` &mdash; how each `VmError` surfaces (divide-by-zero, overflow, type mismatch, missing terminator). Run: `cargo run --example errors`.

<br>
<hr>

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. Licensed under <code>Apache-2.0 OR MIT</code>.</sub>
