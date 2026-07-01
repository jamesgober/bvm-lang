# bvm-lang - Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
> Master plan: ../../_strategy/LANG_COLLECTION.md
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

## v0.1.0 - Scaffold (DONE)
Compiles, CI green, structure correct, no domain logic.
- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt.

## v0.2.0 - Core (THE HARD PART, NOT DEFERRED) (DONE)
A register bytecode VM and dispatch loop - the execution engine for interpreted languages.
Dependencies (wires value, gc, ir) are wired here, when first used.
Exit criteria:
- [x] Every public item has rustdoc + a runnable example.
- [x] Core invariants property-tested (full DIRECTIVES + API authored at this stage).

Delivered: `Op` (register instruction set), `Chunk` (auto-sized register file +
constant pool + back-patching), `Vm` (pooled register file, `match` dispatch),
`VmError` (typed structural + runtime faults, no panics on malformed bytecode).
`value-lang` is wired as the operand type (`Value`) — the "when first used"
dependency for this phase. `unsafe` stays forbidden; the dispatch loop is checked
against untrusted bytecode.

**Dependency deferral (anti-deferral rule).** `gc-lang` and `ir-lang` are not yet
wired: the core VM executes register bytecode over `Value` and has no heap-object
or IR-lowering path to use them from. They wire in additive 1.x work — GC when
heap-allocated objects (strings, arrays) land, IR when direct IR execution or an
`ir -> Chunk` lowering lands. Neither is a deferral of the v0.2.0 core; both are
new surface, recorded here so the move is explicit.

## Additive 1.x (post-freeze, non-breaking)
Candidate additions, each independently shippable without touching the frozen core:
- Function calls / multiple chunks (call frames, a call stack).
- Execution fuel / step limit for hard bounds on untrusted programs.
- `gc-lang` wiring for heap-allocated object values.
- `ir-lang` wiring: direct execution or lowering to `Chunk`.

## v1.0.0 - API freeze (DONE)
Public surface stable and frozen until 2.0.
- [x] docs/API.md marked stable; SemVer promise recorded (docs/STABILITY.md).
- [x] Full test + benchmark suite green on all three platforms.

Pre-freeze adversarial API review resolved two blockers before locking:
`Chunk::emit`/`patch` now speak `Addr` (was `usize`), so a back-patched branch
target needs no cast; and `Op::LoadConst`'s field was renamed `konst -> index`
(a cleaner permanent public + serde-wire name). `#[must_use]` added to `Vm::run`.
These are the breaking changes the 1.0 major bump carries; the surface is frozen
from here. serde wire-format stability documented in docs/STABILITY.md.
