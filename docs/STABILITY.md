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

As of **`1.0.0`**, the public API of `bvm-lang` is **frozen**. The crate follows [Semantic Versioning](https://semver.org/): the surface listed below will not change in a breaking way within the `1.x` series. A breaking change requires a `2.0.0`.

## What "frozen" covers

The following are the stable, public surface. Their names, shapes, and documented behavior are the contract:

### Types

- **`Vm`** — `new`, `with_capacity`, `run`.
- **`Chunk`** — `new`, `emit`, `constant`, `patch`, `code`, `constants`, `registers`, `len`, `is_empty`.
- **`Op`** — the instruction enum. Marked `#[non_exhaustive]`.
- **`VmError`** — the error enum. Marked `#[non_exhaustive]`.
- **Type aliases** — `Reg` (`u16`), `Const` (`u16`), `Addr` (`u32`).
- **Re-exports** — `Value`, `Unpacked`, `Symbol` from [`value-lang`](https://docs.rs/value-lang) `1.x`.

### Behavior

- The numeric semantics: one integer/float tower, overflow-checked integer arithmetic, integer division/remainder-by-zero as `DivideByZero`, float division following IEEE-754, boolean-only branch conditions, and the equality rules — all as documented in [`API.md`](./API.md).
- The safety guarantee: executing any `Chunk`, well-formed or malformed, returns a `Result` and never panics or triggers undefined behavior. `unsafe` is forbidden crate-wide.
- The register-file sizing rule: a `Chunk` sizes itself to the highest register any emitted instruction names.

## What is allowed to change within `1.x`

These are **not** breaking under SemVer and may appear in a `1.x` minor release:

- **New `Op` variants.** `Op` is `#[non_exhaustive]`, so new instructions can be added. Downstream `match`es already require a wildcard arm. Code that constructs only existing variants is unaffected.
- **New `VmError` variants.** `VmError` is `#[non_exhaustive]` for the same reason.
- **New inherent methods** on `Vm` or `Chunk` (for example, a pre-sizing constructor, an execution fuel limit, or call-frame support), and **new types** (for example, wiring `gc-lang` or `ir-lang`), added additively.
- Performance improvements, internal refactors, and documentation changes with no observable behavioral change.

## The `serde` format

Behind the optional `serde` feature, `Op` and `Chunk` derive `Serialize`/`Deserialize` using serde's default **externally-tagged** encoding. This bakes the `Op` variant names and their field names (`dst`, `src`, `lhs`, `rhs`, `index`, `val`, `target`, `cond`) into the wire format.

The format promise for `1.x`:

- A `Chunk` serialized by one `1.x` version deserializes on any `1.x` version **that supports every instruction it contains**.
- Because new instructions may be added in a `1.x` minor (see above), a chunk that uses a newer instruction will fail to deserialize on an older `1.x`. This is expected; forward compatibility to older versions is not promised.
- Variant and field identifiers will not be renamed within `1.x`.

The `serde` representation is a convenience for persisting compiled bytecode, not a long-term archival format; if you need one, pin the `bvm-lang` minor version you serialized with.

## What is not covered

- Private modules and items (anything not re-exported from the crate root).
- The exact `Display` text of `VmError` messages — the variants and their meaning are stable, but the human-readable strings may be reworded.
- Benchmark numbers and internal performance characteristics (though regressions are tracked; see the CHANGELOG).
- The MSRV: `1.85`. Raising the MSRV is treated as a minor, not a breaking, change, and will be noted in the CHANGELOG.

<br>
<hr>

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. Licensed under <code>Apache-2.0 OR MIT</code>.</sub>
