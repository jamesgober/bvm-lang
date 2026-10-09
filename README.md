<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>bvm-lang</b>
    <br>
    <sub><sup>LSB BYTECODE VM</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/bvm-lang"><img alt="Crates.io" src="https://img.shields.io/crates/v/bvm-lang"></a>
    <a href="https://crates.io/crates/bvm-lang"><img alt="Downloads" src="https://img.shields.io/crates/d/bvm-lang?color=%230099ff"></a>
    <a href="https://docs.rs/bvm-lang"><img alt="docs.rs" src="https://img.shields.io/docsrs/bvm-lang"></a>
    <a href="https://github.com/jamesgober/bvm-lang/actions"><img alt="CI" src="https://github.com/jamesgober/bvm-lang/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>bvm-lang</strong> is the virtual machine that runs <b>LSB</b>, the LexerSketch bytecode defined by <a href="https://crates.io/crates/bytecode-lang"><code>bytecode-lang</code></a>. Hand it a module &mdash; built in memory or decoded from bytes &mdash; and it checks it, binds its imports to your host functions, and runs it: typed code with unboxed integers and floats, dynamic code with PHP-style ordered hash-map arrays and language hooks, closures, structs with inheritance, exceptions with <code>finally</code>, stackful coroutines for generators, fibers, and async tasks, and a tracing garbage collector.
    </p>
    <p>
        It is a <em>register machine</em> executing decoded eight-byte instructions in one dispatch loop, and it is built to run code you do not trust: every index the interpreter uses is checked once at load time, every run has a fuel budget, a memory budget, and call-depth and stack limits, and every failure is a value carrying its error code, function, and instruction. Integer arithmetic follows the shared OPS specification bit for bit under every overflow, division-by-zero, shift, and float-conversion policy, including PHP's <code>promote</code>.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). <code>no_std</code>-compatible (needs only <code>alloc</code>), <code>#![forbid(unsafe_code)]</code>, one dependency from the family: <a href="https://crates.io/crates/bytecode-lang"><code>bytecode-lang</code></a>.
    </p>
    <blockquote>
        <strong>2.0.0-alpha.2 is a pre-release.</strong> 2.0 replaces the 1.0 instruction set (<code>Op</code>/<code>Chunk</code>) with LSB, and alpha.2 executes all of it, the coroutine group included. The API freezes at 2.0.0 after a real consumer has used it. See <a href="./docs/STABILITY.md"><code>docs/STABILITY.md</code></a> for what the alphas promise and <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> for migrating from 1.0.
    </blockquote>
</div>

<hr>
<br>

## The model

- A **[`Program`](./docs/API.md#program)** is a loaded module. [`Program::load`](./docs/API.md#programload) takes a `bytecode_lang::Module` (or [`Program::decode`](./docs/API.md#programdecode) its bytes), checks every fact the interpreter relies on, and binds each import to a function registered in a **[`Host`](./docs/API.md#host)**. It is immutable and shareable across threads.
- A **[`Vm`](./docs/API.md#vm)** is an instance of a program: a garbage-collected heap, the globals, and the **[`Limits`](./docs/API.md#limits)** its runs execute under. [`Vm::run`](./docs/API.md#vmrun) calls a function with **[`Value`](./docs/API.md#value)** arguments and returns its result.
- A **[`VmError`](./docs/API.md#vmerror)** is how a run ends without one: an uncaught error with its OPS/LSB code and location, an uncaught throw, or a trap. A **[`LoadError`](./docs/API.md#loaderror)** says why a module was refused.
- **[`Vm::run_async`](./docs/API.md#vmrun_async)** runs a function as the main task of a small deterministic scheduler (registered with [`Host::register_scheduler`](./docs/API.md#hostregister_scheduler)) that drives the tasks `spawn` makes.

<br>

What it guarantees, and how each guarantee is checked:

| Guarantee | How it is held |
|---|---|
| Every LSB instruction is implemented to LSB's semantics. | At least one conformance test per instruction in `tests/inst_*.rs` and `tests/coroutines.rs` (each test names the instructions it covers). |
| Coroutines follow LSB §5.13: stackful suspension through nested calls and hook frames, keys and return values, `resume_throw`, `coro_close` with pending `finally` blocks (including `CloseIgnored` and an `await` during the close), iteration, `CannotSuspend`, `InvalidCoroState`, `NoScheduler`. | `tests/coroutines.rs`: one or more tests per rule, `finally` across a suspension included, and the exact fuel each coroutine instruction costs. |
| A dropped suspended coroutine is closed (LSB §5.13 rule 13): queued by the collection that finds it, closed oldest first at the next fuel charge point or by `Vm::run_finalizers`. | `tests/coroutine_gc.rs`: closes run once, in creation order, inside a run and from the host; refusals and errors are discarded; created and finished coroutines run nothing; self-referencing coroutines are freed; 100,000 dropped coroutines stay inside a 2 MiB budget; suspended stacks count against the budget. |
| `spawn`/`await` work through the built-in scheduler, deterministically. | `tests/scheduler.rs`: FIFO interleaving, results and errors through `await`, deadlock detection, one fuel budget per `run_async`. |
| Integer and float operations match OPS bit for bit under every policy. | `tests/ops_table.rs` runs every operation at every integer type under every policy combination over the edge values {0, ±1, ±2, ±7, MIN, MAX, MIN+1, MAX-1, ...} and every float operation over IEEE edge values (signed zeros, subnormals, ±MAX, ±inf, NaN, halves), against a reference written in `i128` and `f64` in the test itself. |
| Random programs agree with an independent interpreter. | `tests/differential.rs`: thousands of random straight-line and branching programs (loops included) against `tests/common/reference.rs`, and 3,000 random six-function modules (calls of every kind, closures, arrays, maps, structs, strings, try/catch, try/finally with `return` in `finally` overriding, generators, `resume`/`resume_throw`/`coro_close`, keys, iteration) against a separate whole-module reference in `tests/common/full.rs` that keeps a frame stack per coroutine; results, error kinds and locations, globals, and fuel used must match under random fuel and depth limits. Mutation-checked: 10 of 11 deliberate coroutine/exception/closure bugs fail it (the 11th, an automatic-key rule bug, is caught by `tests/coroutines.rs`). |
| Maps behave like PHP arrays. | `tests/php_arrays.rs` checks random insert/push/delete sequences against a PHP 8.3 model, plus key kinds, signed zero and NaN keys, iteration while mutating, and value semantics through `dup`. |
| `finally` works as LSB lowers it. | `tests/exceptions.rs` runs the canonical lowering for every exit (normal, return, throw, runtime error, break) with a plain finally, one that returns (overriding), and one that throws (replacing). |
| Untrusted modules cannot panic, hang, or exhaust host memory. | `tests/untrusted.rs` runs random code over every opcode and randomly mutated encodings under tight limits; `tests/limits.rs` covers infinite loops without safepoints, infinite recursion, wide frames, hostile array lengths, and string doubling. |
| Garbage is reclaimed; live objects are not. | `tests/gc.rs`: a 50,000-link chain survives collections, cycles are collected, a loop allocating 200,000 arrays stays inside a 2 MiB budget, and every root kind keeps its objects. |

<hr>
<br>

## Installation

```toml
[dependencies]
bvm-lang = "=2.0.0-alpha.2"
bytecode-lang = "0.2"
```

Without the standard library:

```toml
[dependencies]
bvm-lang = { version = "=2.0.0-alpha.2", default-features = false }
```

<hr>
<br>

## Quick start

Build a module with `bytecode-lang`, load it, run it:

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{Inst, IntOp, IntTy, ModuleBuilder, Reg, ValType};

// fn sum_to(n: i64) -> i64 { let mut s = 0; for i in 0..=n { s += i } s }
let mut m = ModuleBuilder::new();
let mut f = m.function("sum_to", &[ValType::I64], &[ValType::I64]);
let (s, i, one, more) = (f.reg(ValType::I64), f.reg(ValType::I64), f.reg(ValType::I64), f.reg(ValType::Bool));
let op = IntOp::new(IntTy::I64);
f.emit(Inst::LoadInt { dst: one, val: 1, ty: IntTy::I64 });
let (top, done) = (f.label(), f.label());
f.bind(top);
f.emit(Inst::ILe { dst: more, lhs: i, rhs: Reg(0), ty: IntTy::I64 });
f.jmp_if_not(more, done);
f.emit(Inst::IAdd { dst: s, lhs: s, rhs: i, op });
f.emit(Inst::IAdd { dst: i, lhs: i, rhs: one, op });
f.emit(Inst::Safepoint {}); // fuel is charged here; the GC may run here
f.jmp(top);
f.bind(done);
f.ret(s);
let sum_to = m.add_function(f).unwrap();

let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&program);
assert_eq!(vm.run(sum_to, &[Value::Int(100)]), Ok(Value::Int(5050)));
```

### Dynamic code: a PHP array

`dyn` registers, an ordered map, `$a[] = v` (`map_push`), and `foreach`
(`diter_new` + `iter_next`):

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{Inst, ModuleBuilder, Policy, TypeDef, ValType};

let d = ValType::Dyn;
let mut m = ModuleBuilder::new();
let map_t = m.add_type(TypeDef::Map { key: d, value: d });
let mut f = m.function("main", &[], &[d]);
let mt = f.type_ref(map_t);
let (a, v, it, sum, has) = (f.reg(d), f.reg(d), f.reg(d), f.reg(d), f.reg(ValType::Bool));
f.emit(Inst::NewMap { dst: a, ty: mt });
for x in [10, 20, 30] {
    f.emit(Inst::DLoadInt { dst: v, val: x });
    f.emit(Inst::MapPush { map: a, src: v }); // keys 0, 1, 2
}
f.emit(Inst::DLoadInt { dst: sum, val: 0 });
f.emit(Inst::DIterNew { dst: it, src: a });
let (top, done) = (f.label(), f.label());
f.bind(top);
f.emit(Inst::IterNext { has, iter: it, val: v });
f.jmp_if_not(has, done);
f.emit(Inst::DAdd { dst: sum, lhs: sum, rhs: v, pol: Policy::new() });
f.emit(Inst::Safepoint {});
f.jmp(top);
f.bind(done);
f.ret(sum);
let main = m.add_function(f).unwrap();

let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
assert_eq!(Vm::new(&program).run(main, &[]), Ok(Value::Int(60)));
```

### Generators

A coroutine suspends at `yield`, at any depth of calls below its body, and
`resume` continues it with a value:

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{Inst, ModuleBuilder, Policy, ValType};

let d = ValType::Dyn;
let mut m = ModuleBuilder::new();
// gen() { x = yield 1; return x + 1 }
let mut g = m.function("gen", &[], &[d]);
let (one, x) = (g.reg(d), g.reg(d));
g.emit(Inst::DLoadInt { dst: one, val: 1 });
g.emit(Inst::Yield { dst: x, src: one });
g.emit(Inst::DAdd { dst: x, lhs: x, rhs: one, pol: Policy::new() });
g.ret(x);
let generator = m.add_function(g).unwrap();
// main() { c = gen(); a = c.resume(nil); b = c.resume(41); return a + b }
let mut f = m.function("main", &[], &[d]);
let (c, a, b, sent) = (f.reg(d), f.reg(d), f.reg(d), f.reg(d));
f.emit(Inst::CoroNew { dst: c, func: generator, argc: 0 });
f.emit(Inst::Resume { dst: a, coro: c, src: sent }); // a = 1
f.emit(Inst::DLoadInt { dst: sent, val: 41 });
f.emit(Inst::Resume { dst: b, coro: c, src: sent }); // b = 42 (returned)
f.emit(Inst::DAdd { dst: a, lhs: a, rhs: b, pol: Policy::new() });
f.ret(a);
let main = m.add_function(f).unwrap();

let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
assert_eq!(Vm::new(&program).run(main, &[]), Ok(Value::Int(43)));
```

`foreach` over a generator is `diter_new` plus `iter_next`; async tasks are
`spawn` and `await` under [`Vm::run_async`](./docs/API.md#vmrun_async). The
[`coroutines`](./examples/coroutines.rs) example shows both.

### Untrusted code

Budgets turn a hostile program into an error value with a location:

```rust
use bvm_lang::{Host, Limits, Program, Vm, VmError};
use bytecode_lang::{ErrorKind, Inst, ModuleBuilder, Target};

let mut m = ModuleBuilder::new();
let mut f = m.function("spin", &[], &[]);
f.emit(Inst::Jmp { target: Target(0) }); // no safepoint: fuel still bounds it
let spin = m.add_function(f).unwrap();
let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();

let limits = Limits::new().with_fuel(1_000_000).with_memory(16 << 20).with_depth(512);
let err = Vm::with_limits(&program, limits).run(spin, &[]).unwrap_err();
assert_eq!(err, VmError::Trap { kind: ErrorKind::OutOfFuel, func: spin, pc: 0 });
assert_eq!(err.code(), Some(107)); // E0107
```

<hr>
<br>

## Examples

Runnable programs in [`examples/`](./examples):

| Example | What it shows |
|---|---|
| [`quickstart`](./examples/quickstart.rs) | Build, encode to bytes, decode, load, and run a typed loop. |
| [`php_arrays`](./examples/php_arrays.rs) | Mox/PHP-style code: an ordered array, `$a[] = v`, `foreach`, and PHP's loose `==` supplied by the host as a hook. |
| [`coroutines`](./examples/coroutines.rs) | A PHP generator with keys consumed by `foreach`, and two async tasks taking turns under `run_async`. |
| [`untrusted`](./examples/untrusted.rs) | Fuel, memory, and error locations from the line table for hostile code. |

<hr>
<br>

## Performance

The interpreter executes bytecode-lang's decoded `Inst` values directly: a function body is a flat slice walked by a program counter, and one `match` dispatches. The loop is kept small (common instructions inline, the rest out of line) so its state lives in machine registers; the register stack is owned by the loop while it runs; integer instructions test for `i64` before any other type; dynamic arithmetic and comparison take an inline-int fast path that never touches the heap; maps whose keys are `0, 1, 2, ...` stay *packed* (no hash index at all, like PHP's packed arrays). Calls push a frame onto a flat stack, never the Rust stack.

Measured with the benchmarks in [`benches/`](./benches), Windows x86_64, Rust stable, release profile, on a machine shared with other builds (treat as indicative):

| Benchmark | What it measures | Time | Per unit |
|---|---|---:|---:|
| `dispatch/typed_loop5` | 100,001 iterations of `ile`, `jmp_if_not`, `iadd`, `iadd`, `jmp` (500k instructions) | ~0.75-0.90 ms | ~1.5-1.8 ns/instruction |
| `dispatch/dyn_loop5` | the same loop over `dyn` registers (`dle`, `dadd`) | ~0.85-0.93 ms | ~1.7-1.9 ns/instruction |
| `dispatch/*_loop6` | the same loops with the `safepoint` a verified loop carries | ~1.0-1.1 ms | ~1.7-1.8 ns/instruction |
| `call/fib25` | recursive `fib(25)`: 242,785 calls | ~5.7-6.8 ms | ~25-28 ns/call, body included |
| `map/int_keys_100k_set_get` | 100k `$a[$i] = $i` then 100k reads (packed) | ~7.5 ms | ~37 ns/operation, loop included |
| `map/str_keys_2k_growing_set_get` | 2k inserts and 2k reads with distinct, growing string keys | ~1.0 ms | ~255 ns/operation (key building dominates) |
| `string/concat_eq_slice_100k` | 100k rounds of concatenate, compare, UTF-8 slice | ~10.5 ms | ~105 ns/round |
| `gc/alloc_1m_live_10k` | 1M short-lived arrays, 10k kept live in a map, 16 MiB budget | ~112 ms | ~112 ns/allocation, collection included |
| `load/100k_instructions` | `Program::load` of 1,000 functions of 100 instructions | ~1.6-1.8 ms | ~16-18 ns/instruction |
| `coroutine/generator_iter_100k` | `foreach` over a 100k-element generator (one resume and one yield per element) | ~6.5-6.9 ms | ~65-69 ns/element |
| `coroutine/create_finish_100k` | 100k coroutines created, resumed to a yield, resumed to their return | ~19-20 ms | ~190-200 ns/coroutine, collection included |
| `async/ping_pong_2x50k` | two tasks taking turns through the built-in scheduler, 100k `await`s | ~4.5-4.8 ms | ~45-48 ns/await, scheduler included |

**Against 1.0.** The 1.0 `loop_sum/100000` benchmark runs the same five-instruction loop shape; run back to back with these on the same machine it measured ~0.78-0.83 ms when quiet and ~1.15-1.2 ms under load, against ~0.75-0.90 ms and ~1.15-1.47 ms for `typed_loop5`. Dispatch is therefore on par with 1.0's (within run-to-run noise for typed code, up to ~15% slower for dynamic code), while each instruction now carries its own OPS policy and fuel is charged on back edges. Ranges are the spread of several runs on a machine shared with other builds; the numbers are indicative, not a guarantee.

**Against 2.0.0-alpha.1.** Coroutines add no test to the dispatch loop: closing dropped coroutines rides on the fuel counter's cold branch, and a coroutine's return goes through the existing continuation dispatch. Measured in alternating runs against an alpha.1 build, `typed_loop5` is at parity, and `call/fib25` was slower in every pair, by 3-10% (6.4-7.9 ms against 6.4-7.2 ms on a heavily loaded machine). The cause was not isolated: the remaining differences on the call path are the continuation check for a coroutine's body frame on return and the fuel counter living in the machine rather than in a local.

```bash
cargo bench --bench bench
```

<hr>
<br>

## Design notes

- **One 64-bit slot per register.** Every register, of every type, is a 64-bit word whose meaning comes from its declared LSB type. `dyn`, `str`, and `ref` registers share one encoding (LSB §2.1) in which the all-zero word is `nil`, so every LSB default (`false`, `0`, `+0.0`, `U+0000`, `nil`) is zero and new frames and objects are zero-filled.
- **Load-time checks, run-time speed.** LSB's verifier is not written yet (bytecode-lang 0.5), and its decoder checks structure only. The loader therefore proves, once, every index the interpreter uses; the dispatch loop then indexes registers, globals, tables, and type lists without fallible lookups. It does not check the verifier's type discipline: a word of the wrong type decodes to *some* value, and heap accesses check object kinds, so an ill-typed module computes garbage or raises `TypeError` but cannot escape the VM.
- **Fuel without a verifier.** LSB §5.14 charges fuel at safepoints, calls, hook invocations, taken backward branches, handler entries, and coroutine instructions, so no loop can avoid it, verified or not, and every tier reports `OutOfFuel` at the same pc.
- **Coroutines copy on suspension.** A running coroutine's frames sit on the one flat frame stack above its resumer's; a `yield` moves them (registers included) into the coroutine object and a `resume` moves them back. The dispatch loop is unchanged (a register read is still one indexed load), and once a coroutine's buffers have grown to its deepest stack a switch allocates nothing.
- **Close on drop from a finalization queue.** This heap is traced, so a coroutine's last reference going away is not observable when it happens. The collection that finds a suspended coroutine unreachable keeps it alive and queues it; while a close is queued, the fuel counter is set aside so the next charge takes its cold branch, which starts the close before that instruction. Closing costs nothing when nothing is queued.
- **Precise errors.** A failing instruction breaks out before writing its destination, records its pc, and the unwinder searches that function's handlers in order, then each caller's at its call instruction.
- **Copy-on-write aggregates.** Arrays and maps share their storage between `dup` copies and between loads of one constant; the first write copies. PHP's value semantics are a `dup` per assignment, O(1) until written.
- **Hostile-input budgets everywhere.** Constant nesting (64), inheritance depth (256), call depth, register stack, heap bytes, and fuel are all bounded; collection and constant materialisation are iterative or depth-capped.

<hr>
<br>

## Testing

```bash
cargo test                       # unit + integration + property + doctests
cargo test --no-default-features # no_std + alloc (software float routines)
cargo clippy --all-targets --all-features -- -D warnings
cargo bench --bench bench
```

Every `rust` example in this README and in [`docs/API.md`](./docs/API.md) is compiled and run as a doctest. The software float routines used without `std` (correctly rounded `sqrt` and `fma`, the rounding family) are property-tested against the standard library bit for bit, and the OPS table runs in both configurations.

<hr>
<br>

## Contributing

See [`REPS.md`](./REPS.md) for the engineering standards every change is held to, and [`dev/ROADMAP.md`](./dev/ROADMAP.md) for the roadmap. Before a PR: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean.

<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> &mdash; <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> &mdash; <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>James Gober <me@jamesgober.com>.</strong></sup>
</div>
