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

[Unreleased]: https://github.com/jamesgober/bvm-lang/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/jamesgober/bvm-lang/compare/v0.2.5...v1.0.0
[0.2.5]: https://github.com/jamesgober/bvm-lang/compare/v0.2.0...v0.2.5
[0.2.0]: https://github.com/jamesgober/bvm-lang/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/bvm-lang/releases/tag/v0.1.0
