<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>bvm-lang</b>
    <br>
    <sub><sup>BYTECODE VM</sup></sub>
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
        <strong>bvm-lang</strong> is a small, fast <b>register-based bytecode virtual machine</b> &mdash; the execution engine an interpreted language runs on once its source has been compiled to instructions. You assemble a program as a <code>Chunk</code> of <code>Op</code> instructions over a constant pool, then run it with a <code>Vm</code>, which returns a <code>Value</code>.
    </p>
    <p>
        Being a <em>register machine</em> rather than a stack machine, each instruction names the registers it reads and writes, so a program runs in far fewer dispatch steps than the stack-machine equivalent. Instructions are fixed-size, already-decoded values; execution is a single <code>match</code>-based loop over a slice of them. The whole crate is <b>safe Rust</b> &mdash; <code>unsafe</code> is forbidden &mdash; and it treats the bytecode it runs as <b>untrusted input</b>: a malformed program returns a typed error, never a panic.
    </p>
    <p>
        The runtime value type is the eight-byte NaN-boxed <code>Value</code> from <a href="https://docs.rs/value-lang">value-lang</a>, so the VM speaks the same value as the rest of the <code>-lang</code> language-construction family.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition).
    </p>
    <blockquote>
        <strong>Status: pre-1.0, in active development (<code>v0.2.5</code>).</strong> The execution core is implemented; see <a href="./docs/API.md"><code>docs/API.md</code></a>. The public API is finalized across the 0.x series and frozen at <code>1.0.0</code>. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> and <a href="./dev/ROADMAP.md"><code>dev/ROADMAP.md</code></a>.
    </blockquote>
</div>

<hr>
<br>

## Performance First

`bvm-lang` is a *decoded-instruction* interpreter: each `Op` is a fixed 8-byte value, so the loop never parses a byte stream at runtime, and the central `match` lowers to a jump table. A register design issues fewer instructions per unit of work than a stack design, and the `Vm` pools its register file across runs so steady-state execution is allocation-free.

Latest local Criterion means (`cargo bench`, Windows x86_64, Rust stable), reusing one `Vm`:

- **Straight-line expression** (10 instructions): ~14 ns/run &mdash; roughly **1.4 ns per instruction**.
- **Counted loop** (`sum 1..=1000`, ~5 000 instructions executed): ~7 µs/run &mdash; again ~1.4 ns per instruction dispatched.

Numbers are indicative and hardware-dependent; run `cargo bench` to reproduce on your target.

<br>
<hr>
<br>

## Features

- **Register instruction set** &mdash; three-address arithmetic, comparison, logic, branches, and termination in one compact `Op` enum.
- **Automatic register sizing** &mdash; a `Chunk` derives its register-file size from the instructions it holds; you never declare or over-provision it.
- **Overflow-checked integers** &mdash; integer arithmetic never wraps silently; a fault is reported instead. A float operand promotes the expression to float.
- **No panics on bad bytecode** &mdash; out-of-range registers, constants, and branch targets are checked and returned as typed `VmError`s. `unsafe` is forbidden crate-wide.
- **`value-lang` values** &mdash; `nil`, bool, `i32`, `f64`, and interned symbols in one eight-byte `Copy` type.
- **`no_std`** &mdash; needs only `alloc`. Optional `serde` support persists compiled bytecode.

<br>
<hr>
<br>

## Installation

```toml
[dependencies]
bvm-lang = "0.2"
```

Or:

```bash
cargo add bvm-lang
```

To persist compiled bytecode, enable `serde`:

```toml
[dependencies]
bvm-lang = { version = "0.2", features = ["serde"] }
```

<br>
<hr>
<br>

## Quick Start

Compile and run `(2 + 3) * 4`:

```rust
use bvm_lang::{Chunk, Op, Vm};

fn main() {
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
}
```

More runnable programs live in [`examples/`](./examples) &mdash; run them with `cargo run --example expression`, `--example fibonacci`, and `--example errors`. The full surface, instruction reference, and semantics are documented in [`docs/API.md`](./docs/API.md).

<br>
<hr>
<br>

## Standards

This crate is built to **REPS** (Rust Efficiency &amp; Performance Standards): performance as a hard constraint, `Result`-based error handling with no panics in library code, `#![deny(missing_docs)]` and a strict Clippy profile, cross-platform parity (Linux, macOS, Windows), and property-tested core invariants. See [`REPS.md`](./REPS.md) for the full standard.

<br>

## Contributing

See [`dev/DIRECTIVES.md`](./dev/DIRECTIVES.md) for engineering standards and the definition of done. Before a PR: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean.

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
