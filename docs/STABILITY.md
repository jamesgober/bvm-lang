<h1 align="center" id="top">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>bvm-lang</b><br>
    <sub><sup>STABILITY &amp; SEMVER PROMISE</sup></sub>
</h1>
<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./API.md" title="API Reference"><b>API</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>STABILITY</span>
    </sup>
</div>
<br>

`bvm-lang` **2.0** is a new major version: the 1.x instruction set (`Op`, `Chunk`) is gone, replaced by LSB, the LexerSketch bytecode of [`bytecode-lang`](https://crates.io/crates/bytecode-lang). The 1.x line stays frozen as it was released; it receives no further features.

## 2.0.0-alpha.5 is a pre-release

Per decision D18 (a crate is frozen only after a real consumer has exercised it), the 2.0 API is **not frozen yet**. It freezes at `2.0.0`, after:

1. the coroutine instructions land (done in `2.0.0-alpha.2`),
2. LSB format 2 executes (done in `2.0.0-alpha.3`) and the reference-counting memory profile of decision D22 lands (done in `2.0.0-alpha.4`), and
3. a real consumer (Mox, through the LexerSketch app) has run end to end on it.

Until then, names and signatures may change between alpha releases. Every change will be listed in the CHANGELOG with migration notes. Depend on an exact alpha version (`bvm-lang = "=2.0.0-alpha.5"`).

## What the alphas already promise

These are properties of the implementation that will not be weakened before or after 2.0.0:

- **Semantics are LSB's and OPS's.** Every executed instruction behaves as `_lexersketch/specs/LSB.md` and `OPS.md` define it. Where the VM and the specification disagree, the VM has a bug. A change to an instruction's meaning comes with a new LSB format version, never silently.
- **No panics, no unsafe.** `#![forbid(unsafe_code)]`; no module, however malformed or hostile, can make loading or running panic or read outside the VM. Loading refuses what the interpreter cannot index safely; everything else is a value (`LoadError`, `VmError`).
- **Bounded work.** With finite `Limits`, every run ends: fuel bounds instructions (and, under `Memory::Rc`, the cycle collector's work and the binding of large dynamic calls), the memory budget bounds the heap, depth and stack limits bound frames.
- **The tracing profile is the default.** `Vm::new` and `Vm::with_limits` keep the tracing collector; a program gets reference counting only when its host asks for `Memory::Rc`.
- **Precise errors.** A failing instruction raises at its own pc without writing its destination; `VmError` carries the OPS/LSB error code, the function, and the pc.
- **Determinism.** Results do not depend on the platform, the `std` feature, or hash seeds: map order is insertion order, and float results are IEEE (software and hardware paths are bit-identical). Under the tracing profile collection timing is observable in exactly two ways, both finalization: a dropped suspended coroutine's pending `finally` blocks (LSB §5.13 rule 13) and an object's destructor run when a collection has found them unreachable. For a given module, input, and `Limits` on a fresh `Vm`, collections (and so those finalizations) happen at the same points on every run; they are not part of what other tiers must reproduce. Under `Memory::Rc` they run at the last reference, a function of the program alone (and the cycle collector's runs are a function of the program and the budget).
- **The profiles differ only where LSB says they may.** Destructor and drop-close timing, reference unbinding on copy (PHP's rule under RC, LSB's under tracing), and the RC cycle collector's fuel. Everything else (results, errors and their locations, globals, other fuel, which copies are made) is the same under both, and is tested so on random programs.
- **Fuel is LSB §5.14's,** plus three charges recorded for LSB: an instruction pays one unit per 256 bytes it copies, hashes, or compares, a dynamic call of `n >= 64` flattened items costs `n / 64` units more (ISSUES P33), and the RC cycle collector charges one unit per 64 objects it visits. Every tier charges the same points; a run under a budget stops at the same function and pc (except where an RC cycle collection landed).
- **Copy-on-write decisions are deterministic and the same in both profiles.** Which writes copy contents and which elements `dsep_*`/`dref_*` replace by a copy follow LSB §5.16's two bits, never reference counts or collection timing. Under RC the counts decide only whether a copy is physical, which is where PHP's reference unbinding happens.

## What may change before 2.0.0

- The public types' shapes: `Value` and `VmError` are `#[non_exhaustive]` (new variants may appear), `Limits` gains fields only through new methods, `LoadErrorKind` is `#[non_exhaustive]`.
- The host interface (`Host`, `HostCtx`, `HostError`): host-lang will replace or extend it; the seam is deliberately small.
- Inspection methods on `Vm` may be reorganised once a consumer's needs are known.
- The built-in scheduler's surface (`Host::register_scheduler`, `Vm::run_async`, `VmError::Deadlock`) is the minimum a test harness and a simple host need; host-lang may replace it with a richer interface.
- Close-on-drop's host controls (`Vm::run_finalizers`, `Vm::pending_finalizers`) and the drop signal (`nil`) follow LSB §5.13 rule 13 and change only with it (a future format version adds a per-coroutine "close on drop" flag).
- The memory-profile surface (`Memory`, `MemoryStats`, `Vm::with_memory`, `Vm::memory`, `Vm::memory_stats`, `Vm::set_destructor`, `VmError::BadDestructor`) is new in alpha.4: how a program selects its profile and names its destructor may move into the module once LSB carries a `[runtime]` section, and destructor errors may come to be raised where the object died (PHP's behaviour) rather than discarded.
- Fuel follows LSB §5.14 and changes only with it (the three charges above are proposed for it). The byte rate of the data-work charge (256 bytes per unit) may be tuned.
- The `Limits` presets (`new`, `untrusted`, `trusted`) may be retuned; each stays finite except `trusted`'s fuel.

## MSRV

Rust **1.85** (edition 2024). An MSRV increase is a minor-version change after 2.0.0 and is always noted in the CHANGELOG.

## Dependencies

`bytecode-lang` `0.3` (LSB format 2; format 1 files are refused and must be regenerated). The 1.x dependency on `value-lang` was dropped (see the CHANGELOG and `dev/ROADMAP.md` for why).
