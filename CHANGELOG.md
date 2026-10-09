<h1 align="center">
    <img width="90px" height="auto" src="https://raw.githubusercontent.com/jamesgober/jamesgober/main/media/icons/hexagon-3.svg" alt="Triple Hexagon">
    <br><b>CHANGELOG</b>
</h1>
<p>
  All notable changes to <code>bvm-lang</code> will be documented in this file. The format is based on <a href="https://keepachangelog.com/en/1.1.0/">Keep a Changelog</a>,
  and this project adheres to <a href="https://semver.org/spec/v2.0.0.html/">Semantic Versioning</a>.
</p>

---

## [Unreleased]

### Added

### Changed

### Fixed

### Security

---

## [2.0.0-alpha.3] - 2026-10-09

**LSB format 2.** The VM now executes the format [`bytecode-lang`](https://crates.io/crates/bytecode-lang) 0.3 defines: dynamic calls that bind to parameter lists (named arguments, variadics, defaults, by-reference parameters decided at run time), host functions as values, PHP references with transparent reference slots, copy-on-write separation for nested writes, OPS v2's `pow`, `abs`, and saturating shifts with the family's shared `ls_pow` routine, `raise` with error payloads, and PHP's generator keys. Still a pre-release: the API freezes at 2.0.0 (see [`docs/STABILITY.md`](docs/STABILITY.md)).

### Breaking

- **Format 2 only.** `bytecode-lang` `0.3` replaces `0.2`; `Program::decode` refuses format 1 bytes (`LoadErrorKind::Decode` with `UnsupportedVersion(1)`): regenerate them. The renamed instructions are `Inst::IBitNot` (was `INot`), `Inst::DBitNot` (was the bitwise `DNot`), and `Inst::DNot` (the logical not, was `DLNot`); `Inst::F32ToInt`/`F64ToInt` take `conv: FloatConv` instead of `op: IntOp`.
- **`VmError::Raised` gains `payload: Value`**: the operand of the `raise` that raised the error (a `NoMatch` carries the unmatched value), `Value::Nil` for an error an instruction raised by itself. Constructing or matching it field by field needs the new field (`..` in patterns). `Display` shows a non-nil payload: `uncaught E0200 NoMatch (42) at f0 @1`.
- **Dynamic-call arity is `ArgumentError` (E0114)**, no longer `TypeError`: `dcall`, and `coro_new_indirect`/`spawn` with a `dyn` callee, bind their arguments to the callee's parameter list (LSB §5.15), and a callee without one takes exactly its parameters.
- **Generator keys follow PHP** (LSB §5.13 rule 10): the automatic key counter starts at -1 and only explicit larger integer keys raise it, so after only `yield -5 => x` the next automatic key is `0` (alpha.2 gave `-4`, the `map_push` rule maps keep).
- **`dup` of a reference is a `TypeError`**; `cell_get`/`cell_set` accept references (kind 14) as well as cells.
- **Heap references carry 15 generation bits** (one bit marks a PHP reference), so a slot is retired after 32,767 reuses rather than 65,535. Unobservable except as slightly more slots in long-running heaps.
- `LoadErrorKind` gains `BadParamList` and `BadCallShape` (the enum is `#[non_exhaustive]`).

### Added

- Execution of the 18 new instructions: `ipow`, `fpow`, `dpow`, `dabs`, `dsep_index`, `dsep_prop`, `dcall_shape`, `dparam_ref`, `dparam_ref_named`, `err_payload`, `raise`, `new_ref`, `dref_index`, `dref_prop`, `dbind_index`, `dbind_prop`, `dunref_index`, `dunref_prop`; hooks `pow` (28), `abs` (29), and `call_shape` (30).
- **Parameter-list binding** (LSB §5.15) for `dcall`, `dcall_shape`, `coro_new_indirect`, and `spawn`: spreads and named spreads flattened, positional/named/rest/rest-map/named-rest binding, `ignore_extra`, the presence mask, by-reference parameters receiving references (a temporary for a non-reference), by-value parameters receiving a reference's value separated, `ArgumentError`, one unit of fuel charged after binding (flattening and binding errors are uncharged). All-positional calls use an allocation-free binder property-tested against `ParamList::bind`; calls with names use `ParamList::bind`. `dcall_shape` on a non-callable calls the `call_shape` hook (or `call` when nothing is named).
- **Import parameter lists and host functions as values**: `load_import` of any import is a function value every dynamic call form accepts; a host function called through a parameter list receives references, rest collections, and the presence mask. `HostCtx::ref_get`, `HostCtx::ref_set`, `HostCtx::elements`, `HostCtx::entries`, `HostCtx::new_array`, and `HostCtx::new_map` let host functions use them.
- **References** (LSB §5.17): kind `reference`; reference slots in arrays, maps, and `dyn` struct fields are transparent to every slot read and write (typed and dynamic, `iter_next`, spreads, and `Vm::elements`/`entries`/`field`); copies share reference slots; references are traced, so cycles through them are collected. `Vm::ref_value`.
- **Copy-on-write separation** (LSB §5.16): arrays and maps carry the `cow` and `aliased` bits of the spec's conforming implementation for tracing collectors; `dsep_*` separate exactly the aliased elements, so `$g[$i % 64][] = $i` copies no contents after its first pass. The decisions follow the bits, not reference counts or collection timing, and are therefore deterministic. They are asked only through `contents_shared` (on the array and map objects) and `Heap::is_aliased`, the seam the reference-counting memory profile (D22, alpha.4) replaces.
- **OPS v2 arithmetic**: `pow` at every integer type under every overflow policy (exact by repeated squaring; `wrap` keeps the low bits; `promote` rounds the exact power, a negative exponent takes the float rule; otherwise `NegativeExponent`, E0006, never a trap), float `pow` by the shared `ls_pow` routine (specified in `_lexersketch/specs/ops-vectors/pow.md`, with a 122-row vector table this VM passes bit for bit), `dabs` with `promote`, and `shift = saturate` on `ishl`/`ishr`/`dshl`/`dshr`.
- **`raise` and error payloads**: `raise kind, src` raises any catchable kind with a payload; `err_payload` reads it; `Vm::error_payload`; payloads are traced.
- Tests: `tests/php_semantics.rs` (PHP programs lowered as a PHP code generator lowers them, with PHP 8.3's results), `tests/format2.rs` (loading, hooks, fuel point, `dparam_ref`, coroutine binding, payloads, reference and separation corners), `tests/pow_vectors.rs` (the spec's vectors through `fpow` and `dpow`); the scalar differential reference gains `pow`, `abs`, and saturating shifts (with its own transcription of `ls_pow` from the spec), and the whole-module reference gains every format 2 feature (independent binder, reference slots, the two copy-on-write bits) with a new PHP-focused property; mutation-checked (32 deliberate bugs; every one that changes behaviour fails the suite).
- Benchmarks `call/dcall_plain_100k`, `call/dcall_bind_100k`, `call/dcall_shape_named_100k`, `php/nested_append_100k`, `php/nested_append_dup_100k`, `php/foreach_by_ref_100k`. Example `php_calls`.

### Changed

- **`call/fib25` is ~7% faster than alpha.1 and ~12% faster than alpha.2**: the alpha.2 call-path regression is isolated to the return path (every continuation was matched, the coroutine body's included, before a typed call's result was written) and fixed by testing that continuation first.
- `Vm::elements`, `Vm::entries`, and `Vm::field` read reference slots as their values.
- Copy-on-write decisions now follow the `cow` bit instead of `Arc` reference counts; the counts only decide whether a physical copy is still needed.
- Format 2 adds `0x27`, `0x48`, `0x95`-`0x9C`, `0xAF`, `0xB5`-`0xBB` to the loader's checks, plus parameter lists, call shapes, and `raise` kinds.

---

## [2.0.0-alpha.2] - 2026-10-09

**Coroutines.** The VM now executes every LSB instruction: the coroutine group (`0xF0`..=`0xFC`) for generators, fibers, and async tasks, with close on drop and a small deterministic scheduler for tests and simple hosts. Still a pre-release: the API freezes at 2.0.0 (see [`docs/STABILITY.md`](docs/STABILITY.md)).

### Breaking

- **`VmError::Unsupported` is removed.** No instruction is unsupported any more. Code matching it can drop the arm.
- **`VmError::Deadlock { waiting }` is new** (the enum is `#[non_exhaustive]`): `Vm::run_async` ends with it when every remaining task waits.
- **`coro_new` is checked at load like `call`**: wrong argument count is `LoadErrorKind::ArityMismatch`, a body with captures is `LoadErrorKind::CalleeHasCaptures` (alpha.1 loaded such modules and stopped at the instruction).
- **Fuel follows LSB §5.14 exactly.** Every coroutine instruction (and `iter_next` over a coroutine) costs one unit. `call_indirect` and `tail_call_indirect` to an import now check the argument count before charging, as they already did for bytecode callees (alpha.1 charged first), so a mismatched call to a host function costs no fuel.

### Added

- Execution of `coro_new`, `coro_new_indirect`, `yield`, `yield_kv`, `await`, `resume`, `resume_throw`, `coro_status`, `coro_current`, `spawn`, `coro_close`, `coro_key`, and `coro_result` (LSB §5.13 rules 1-14): stackful suspension through nested calls and hook frames; automatic and explicit keys; return values; throwing into a coroutine; closing with pending `finally` blocks, `CloseIgnored`, and `await` during a close; iteration over coroutines with `iter_new`/`diter_new`; `InvalidCoroState`, `CannotSuspend`, `NoScheduler`; coroutine frames counted against the depth limit and suspended stacks against the memory budget.
- **Close on drop** (LSB §5.13 rule 13, decided in §10 question 9): a suspended coroutine that becomes unreachable is closed. The heap is traced, so the collection that finds it queues it (alive, with what it reaches), and the VM closes queued coroutines one at a time, oldest first, at the next fuel charge point of a run, with the drop signal `nil`; outcomes are discarded. `Vm::run_finalizers` and `Vm::pending_finalizers` let a host close them explicitly.
- `Host::register_scheduler` and `Vm::run_async`: a single-threaded, deterministic scheduler bound as the `spawn` hook (FIFO tasks; `await` of a task waits for its result or error; `await` of other values passes back the value).
- `Vm::coro_state`.
- Tests: `tests/coroutines.rs` (conformance per instruction and rule), `tests/scheduler.rs`, `tests/coroutine_gc.rs` (tracing, close on drop, budgets); a whole-module reference interpreter (`tests/common/full.rs`, a frame stack per coroutine) and a differential property over random six-function modules with calls, closures, arrays, maps, structs, strings, try/catch, try/finally with `return` in `finally` overriding, and coroutines, mutation-checked.
- Benchmarks `coroutine/generator_iter_100k`, `coroutine/create_finish_100k`, `async/ping_pong_2x50k`. Example `coroutines`.

### Changed

- LSB §5.13 rule 13 is rewritten for the close-on-drop decision and LSB gains §5.14, the fuel rule every tier shares (both in `_lexersketch/specs/LSB.md`).
- A pending close is started from the fuel counter's cold branch, so the dispatch loop has no test of its own for it; the fuel counter now lives in the VM instance during a run.

---

## [2.0.0-alpha.1] - 2026-10-08

**The VM now executes LSB**, the LexerSketch bytecode of [`bytecode-lang`](https://crates.io/crates/bytecode-lang) 0.2: typed and dynamic code, calls, closures, structs, arrays, PHP-style ordered maps, strings, exceptions, a tracing garbage collector, and explicit budgets for untrusted code. A pre-release: the coroutine instructions arrive in alpha.2 and the API freezes at 2.0.0 (see [`docs/STABILITY.md`](docs/STABILITY.md)).

### Breaking

- **The 1.x instruction set is removed.** `Op`, `Chunk`, `Reg`, `Const`, and `Addr` are gone; programs are `bytecode_lang::Module`s (build them with `bytecode_lang::ModuleBuilder`, or decode bytes), loaded with `Program::load`/`Program::decode` and run with `Vm`.
- **`Vm` is now an instance of a program.** `Vm::new()` became `Vm::new(&program)`; `Vm::run(&chunk)` became `Vm::run(func_id, &args)`. `Vm::with_capacity` is removed (the register stack is pooled and sized by the program).
- **`Value` is the VM's own type.** The `value-lang` re-exports (`Value`, `Unpacked`, `Symbol`) are removed and the `value-lang` dependency is dropped: LSB `dyn` ints are 64-bit and references must share one 64-bit encoding with `nil` as null (LSB §2.1), which value-lang 1 (`i32` ints, process-local symbols, no object kind) cannot represent. `bvm_lang::Value` is a plain enum (`Nil`, `Bool`, `Int`, `UInt`, `F32`, `Float`, `Char`, `Obj`).
- **`VmError` is redesigned.** Runtime outcomes carry the OPS/LSB `bytecode_lang::ErrorKind`, the function, and the pc (ISSUES M62): `Raised`, `Thrown`, `Trap`, `Unsupported`, plus entry errors. The 1.x variants (`TypeMismatch`, `DivideByZero`, `IntegerOverflow`, `BadRegister`, `BadConstant`, `BadJump`, `NoTerminator`) are gone: structural faults are now `LoadError`s at load time, and arithmetic faults are `Raised { kind: ArithOverflow | DivByZero | ... }`.
- **Semantics follow OPS, not 1.x's fixed numeric tower.** Each instruction carries its own overflow, division-by-zero, shift, and float-to-int policy; `%` on floats is the truncated remainder (`frem`), with the IEEE remainder available as `fieee_rem` (1.x documented `%` as IEEE remainder but computed fmod; ISSUES M62).
- **The `serde` feature is removed.** Persist bytecode with `bytecode_lang::encode`/`decode`.

### Migration from 1.0

| 1.0 | 2.0 |
|---|---|
| `let mut c = Chunk::new(); c.emit(Op::...)` | `let mut m = ModuleBuilder::new(); let mut f = m.function(name, &params, &results); f.emit(Inst::...)` |
| `Op::LoadInt { dst, val }` (dynamic) | `Inst::DLoadInt { dst, val }` into a `ValType::Dyn` register |
| `Op::Add { dst, lhs, rhs }` (checked, int/float promotion) | `Inst::DAdd { dst, lhs, rhs, pol: Policy::new() }` (or `Inst::IAdd` for typed `i64`) |
| `Op::Lt`, `Op::Eq`, ... | `Inst::DLt`, `Inst::DEq`, ... writing `ValType::Bool` registers |
| `Op::Not` | `Inst::BNot` on `bool` (or `Inst::DLNot` on `dyn`) |
| `Op::Jump`, `JumpIfTrue`, `JumpIfFalse` + `Chunk::patch` | `f.jmp(label)`, `f.jmp_if`, `f.jmp_if_not` with labels resolved by the builder |
| `Op::Return { src }`, `Op::Halt` | `f.ret(src)`, `f.ret_void()` |
| `Vm::new().run(&chunk)` | `Vm::new(&Program::load(m.finish()?, &Host::new())?).run(id, &[])` |
| `VmError::DivideByZero` | `VmError::Raised { kind: ErrorKind::DivByZero, func, pc }` |
| `VmError::BadRegister(r)` at run time | `LoadErrorKind::OutOfRange { what: "register", .. }` at load time |

LSB §11 maps every 1.x `Op` to its LSB instruction.

### Added

- `Program` (`load`, `decode`, `module`, `export`, `location`), `LoadError`, `LoadErrorKind`, `Location`, `MAX_INHERITANCE_DEPTH`, `MAX_CONST_DEPTH`: one linear load-time pass that checks every index, window, target, arity, type-operand kind, `promote` placement, handler, inheritance chain, constant nesting, hook and start signature, and binds imports.
- `Vm` (`new`, `with_limits`, `limits`, `set_limits`, `program`, `run`, `run_with`, `run_export`, `fuel_used`, `heap_bytes`, `heap_objects`, `collections`, `collect_garbage`, `global`, `kind`, `str_bytes`, `new_str`, `elements`, `entries`, `field`, `error_code`) executing every LSB instruction except the coroutine group (`0xF0`..=`0xFC`, which ends the run with `VmError::Unsupported` naming the opcode and pc).
- `Limits`: fuel (H09; charged at safepoints, calls, hook invocations, taken backward branches, and handler entry), heap memory, call depth, register-stack slots.
- `Host`, `HostCtx`, `HostError`: host functions for imports and import-bound hooks.
- `Value`, `Obj`, `VmError` with `kind`, `code`, `location`.
- A VM heap with exact roots, iterative mark and sweep rationed by allocation volume, generation-checked handles, and slot retirement; copy-on-write arrays and maps; PHP array semantics with packed integer lists.
- Software float routines (correctly rounded `sqrt` and `fma`, the rounding family) for `no_std`, property-tested bit-identical to `std`; the IEEE `remainder`; the correctly rounded `i64` quotient for `promote`; CPython's float floor division.
- Tests: per-instruction conformance suites, the OPS conformance table against an `i128` reference, differential property tests against a reference interpreter, PHP-array properties, exception and `finally` suites, budget, GC, and loader suites, fuzz properties over random code and mutated encodings. Benchmarks for dispatch, calls, maps, strings, GC, and loading. Examples `quickstart`, `php_arrays`, `untrusted`.

### Fixed

- ISSUES **H09**: an infinite loop (`jmp` to itself, no safepoint) now ends with the `OutOfFuel` trap under a fuel budget.
- ISSUES **M62**: errors carry the function and pc; register files are sized from declarations, never inferred; code lengths are never truncated through casts; `frem` is documented as the truncated remainder it computes.

### Security

- Hostile modules are refused at load time or contained at run time by fuel, memory, depth, and stack budgets; allocation sizes are checked against the budget before any buffer is requested. Map hashing is keyed per VM (SipHash-1-3 for strings) so programs cannot force collisions.

---

## [1.0.0] - 2026-07-01

**API freeze.** The public surface is now stable under Semantic Versioning &mdash; no breaking change until `2.0.0`. See [`docs/STABILITY.md`](docs/STABILITY.md) for the frozen surface and the compatibility promise. A pre-freeze adversarial review of the API produced the two breaking changes below; everything else is unchanged from `0.2.5`.

### Changed

- **Breaking:** `Chunk::emit` now returns `Addr` (`u32`) instead of `usize`, and `Chunk::patch` now takes `addr: Addr` instead of `usize`. An emitted address feeds straight into a `Jump`/`patch` target with no cast. Migration: drop the `as u32` on back-patched branch targets; if you indexed `code()` with an emitted address, add `as usize`.
- **Breaking:** the `Op::LoadConst` field `konst` was renamed to `index`. This is also the `serde` wire name. Migration: rename the field in `Op::LoadConst { .. }` literals and patterns.
- Added `#[must_use]` to `Vm::run` &mdash; discarding a successful run's `Value` is almost always a mistake.

### Added

- [`docs/STABILITY.md`](docs/STABILITY.md) &mdash; the frozen public surface, what may still change additively within `1.x` (new `Op`/`VmError` variants, new methods), the `serde` wire-format promise, and the MSRV policy.
- `docs/API.md` marked stable.

---

## [0.2.5] - 2026-07-01

Crate rename: **`vm-lang` &rarr; `bvm-lang`.** The name `vm-lang` was already taken on crates.io, so the crate is published as `bvm-lang` and the library imports as `bvm_lang`. No functional change from `0.2.0` &mdash; the API, semantics, and behavior are identical.

### Changed

- Renamed the package from `vm-lang` to `bvm-lang` and the library from `vm_lang` to `bvm_lang`. Update imports to `use bvm_lang::...`.
- Updated crate metadata, README, `docs/API.md`, and examples to the new name.

---

## [0.2.0] - 2026-07-01

The execution core. A register bytecode VM with a `match`-dispatched interpreter loop, built on the `value-lang` `Value` as its operand type. Bytecode is treated as untrusted input: every register, constant, and branch access is checked, and the crate forbids `unsafe`, so a malformed program returns a typed error instead of panicking.

### Added

- `Op` &mdash; the register instruction set: data movement (`Move`, `LoadConst`, `LoadNil`, `LoadBool`, `LoadInt`), arithmetic (`Add`, `Sub`, `Mul`, `Div`, `Rem`, `Neg`), comparison (`Eq`, `Ne`, `Lt`, `Le`, `Gt`, `Ge`), the logical `Not`, control flow (`Jump`, `JumpIfTrue`, `JumpIfFalse`), and termination (`Return`, `Halt`). Each instruction is a fixed 8-byte decoded value.
- `Chunk` &mdash; an assembled program: instructions, a constant pool, and a register file whose size is derived automatically from the highest register any instruction names. `emit` appends and returns an address; `constant` interns a `Value` and returns its index; `patch` back-fills a forward branch once its target is known.
- `Vm` &mdash; the interpreter. `run` executes a chunk and returns its result `Value`; the register file is pooled and reused across runs, so a long-lived VM does not reallocate in steady state. `with_capacity` pre-sizes the file.
- `VmError` &mdash; typed runtime faults (`TypeMismatch`, `DivideByZero`, `IntegerOverflow`) and structural faults (`BadRegister`, `BadConstant`, `BadJump`, `NoTerminator`), each with a `Display` message and a `core::error::Error` impl.
- `Reg`, `Const`, `Addr` type aliases, and the re-exported `Value`, `Unpacked`, and `Symbol` from `value-lang`.
- Numeric semantics: integer arithmetic is overflow-checked; a float operand promotes the result to float; integer division/remainder by zero errors while float division follows IEEE-754.
- `serde` feature: `Serialize`/`Deserialize` for `Op` and `Chunk`, so compiled bytecode can be persisted and reloaded.
- Examples (`expression`, `fibonacci`, `errors`), a Criterion benchmark suite (`expression`, `loop_sum`), integration tests, `serde` round-trip tests, and `proptest` properties for arithmetic, ordering, and the no-panic invariant on arbitrary straight-line bytecode.

### Changed

- Wired `value-lang = "1"` as the runtime operand type; the `std` and `serde` features now forward to it.
- Fixed invalid `keywords`/`categories` TOML in the crate manifest and aligned the `clippy.toml` MSRV (`1.85`) with `rust-version`.

---

## [0.1.0] - 2026-06-18

Initial scaffold and repository bootstrap. No domain logic yet &mdash; this release establishes the structure, tooling, and quality gates the implementation will be built on.

### Added

- `Cargo.toml` with crate metadata, Rust 2024 edition, MSRV 1.85.
- Dual `Apache-2.0 OR MIT` license files.
- `README.md`, `CHANGELOG.md`, and a documentation skeleton.
- `REPS.md` compliance baseline.
- `.github/workflows/ci.yml` CI matrix; `deny.toml`, `clippy.toml`, `rustfmt.toml`.
- `dev/DIRECTIVES.md` and `dev/ROADMAP.md` (committed engineering standards + plan).

[Unreleased]: https://github.com/jamesgober/bvm-lang/compare/v2.0.0-alpha.3...HEAD
[2.0.0-alpha.3]: https://github.com/jamesgober/bvm-lang/compare/v2.0.0-alpha.2...v2.0.0-alpha.3
[2.0.0-alpha.2]: https://github.com/jamesgober/bvm-lang/compare/v2.0.0-alpha.1...v2.0.0-alpha.2
[2.0.0-alpha.1]: https://github.com/jamesgober/bvm-lang/compare/v1.0.0...v2.0.0-alpha.1
[1.0.0]: https://github.com/jamesgober/bvm-lang/compare/v0.2.5...v1.0.0
[0.2.5]: https://github.com/jamesgober/bvm-lang/compare/v0.2.0...v0.2.5
[0.2.0]: https://github.com/jamesgober/bvm-lang/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/bvm-lang/releases/tag/v0.1.0
