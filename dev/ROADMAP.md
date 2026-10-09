# bvm-lang - Roadmap

> Path from scaffold to a stable 1.0, then to 2.0: the VM that executes LSB. Hard parts are front-loaded; each phase has hard exit criteria.
> Master plans: ../../_strategy/LANG_COLLECTION.md and ../../_lexersketch/ROADMAP.md (item 4.4).
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

## v0.1.0 - Scaffold (DONE)
Compiles, CI green, structure correct, no domain logic.
- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt.

## v0.2.0 - Core (DONE)
A register bytecode VM over value-lang `Value`s: `Op`, `Chunk`, `Vm`, `VmError`.

## v1.0.0 - API freeze (DONE)
The 0.2 surface frozen. Superseded by 2.0: its `Op`/`Chunk` instruction set is
replaced by LSB (ISSUES M52, F07), and the 1.0 audit found no fuel (H09) and no
`pc` in errors (M62).

## v2.0.0-alpha.1 - Execute LSB (DONE, prepared 2026-10-08)
The VM becomes the T1 tier of LexerSketch (ARCHITECTURE §7): it executes
bytecode-lang modules, typed and dynamic code alike, under explicit budgets.

Delivered:
- **Loading** (`Program::load`, `Program::decode`): one linear pass checks every
  fact the interpreter indexes by: register operands and every register of every
  call window, constant/function/import/global/table/name/type/capture indices,
  branch, table, and handler targets, no fall-through, direct-call arity, no
  direct call of a capturing function, type operands of the right kind,
  `promote` only into `dyn` (V-T8), valid modifiers, `dyn` catch registers, no
  tail call in a try region (V-CF4), struct parents (acyclic, at most 256 deep,
  field prefixes), aggregate-constant order and depth (at most 64), hook and start
  signatures; imports bound to a `Host`. Precise `LoadError` (kind, function, pc).
- **Execution** of every LSB instruction except the coroutine group: moves,
  constants (typed and `dyn`, aggregates materialised once per representation and
  shared copy-on-write), globals; every OPS integer op at every type under every
  policy, bit-exact; every float op (correctly rounded `sqrt`/`fma`, IEEE
  `remainder`, OPS `min`/`max`, total order); conversions; booleans, chars,
  identity; the dynamic numeric fast path with `promote` (correctly rounded
  quotient), CPython float floor division and modulo, exact int/float comparison,
  truthiness, concatenation; `to_dyn`/`from_dyn`/`type_of`/`is_kind`/`is_type`/
  `cast`; dynamic indexing, properties (fields, inherited methods, map keys),
  `dcall`, `diter_new`, `dlen`; all 27 executable hooks (function or import);
  branches, `switch`, calls (direct, indirect, host, tail, indirect tail), returns,
  `throw`/`err_code`, `safepoint`, `unreachable`; closures and cells; structs;
  arrays; ordered hash maps with PHP semantics (insertion order, in-place update,
  next integer key, packed-list fast mode, iteration that skips deleted and
  visits appended entries, kind-sensitive keys); typed and dynamic iterators;
  `dup`; byte strings with UTF-8-checked slicing.
- **Exceptions** (LSB §4.3): handler regions searched in order, errors crossing
  frames raised at the call, precise unwind points (a raising instruction never
  writes its destination), traps that skip handlers, the canonical `finally`
  lowering tested including `return`-in-`finally` overriding and errors in
  `finally` replacing the pending completion.
- **Budgets** (H09): `Limits` with fuel (charged at safepoints, call-family
  instructions, taken backward branches, and handler entry, so even unverified
  loops are bounded), heap memory (`OutOfMemory` trap, checked before any large
  buffer is requested), call depth and register-stack slots (`StackOverflow`,
  catchable). `VmError` carries the OPS/LSB `ErrorKind`, function, and pc (M62).
- **Memory management**: a VM-specific heap (below) with exact roots (reference
  registers of every frame, running closures, globals, constant and name caches),
  an iterative mark and sweep, generation-checked references that read as `nil`
  once stale, and slot retirement before a generation could wrap.
- **Host interface**: `Host` (register functions by import module and name),
  `HostCtx` (read and create strings, inspect values), `HostError` (raise a kind
  or throw a value). Host functions bind imports and, through imports, hooks.
- **Tests**: per-instruction conformance suites, the OPS table (every op x
  policy x edge value x integer type, every float op over IEEE edge values,
  the dynamic path under every policy including `promote`) against an independent
  `i128` reference, differential property tests of random straight-line and
  branching programs against a reference interpreter (whole register file and
  fuel compared; mutation-checked), PHP-array properties against a model,
  exception and `finally` suites, budget and GC suites, loader rejections, and
  fuzz properties over random code and mutated encodings.
- **Benches**: dispatch (typed and dynamic, 1.0's loop shape), calls (`fib(25)`),
  maps (integer and string keys), strings, GC churn, loading.

### Dependency wiring
- **bytecode-lang `0.2`** (crates.io): the instruction set, module model, and
  budgeted decoder. The VM has no instruction set of its own any more.
- **value-lang: dropped.** LSB constants are format values, not value-lang values
  (LSB §1.6), and value-lang 1 cannot be the register representation: its ints are
  `i32` (LSB `dyn` ints are 64-bit), its symbols are process-local, and it has no
  heap-object kind (ISSUES M63), so it cannot carry a reference. The VM's own
  64-bit encoding (`src/dynv.rs`: `nil` is the zero word, 49-bit inline ints with
  boxed overflow, generation-checked 48-bit references, NaN-canonical floats)
  satisfies LSB §2.1's single-representation constraint. Revisit at value-lang
  2.0 if it gains an object kind and 64-bit ints.
- **gc-lang: not wired; a VM-specific heap instead.** gc-lang 1's `Gc<T>` handle is
  64 bits (index plus 32-bit generation) with private fields and no conversion to
  or from raw bits, so it cannot live in a 64-bit register slot next to floats and
  ints or inside a NaN-box, which LSB §2.1 requires of every reference. It also
  has no byte accounting (the memory budget needs it) and is generic over one
  object type traced without access to the program's layouts (struct reference
  fields, capture types), which the VM's exact tracing needs. The VM heap keeps
  gc-lang's proven design points (slot vector, free list, generation stamps,
  iterative mark, pooled work list) and adds 16-bit generations packed into the
  reference word, slot retirement before wrap (stale handles never alias, which
  the in-flight gc-lang 1.0.1 generation fix addresses there), and byte
  accounting. Revisit when gc-lang 2.0 (ARCHITECTURE §7) exposes raw-handle
  conversion and accounting.

### Moved to 2.0.0-alpha.2 (recorded per the anti-deferral rule)
- **The coroutine group (`0xF0`..=`0xFC`)**: `coro_new`, `coro_new_indirect`,
  `yield`, `yield_kv`, `await`, `resume`, `resume_throw`, `coro_status`,
  `coro_current`, `spawn`, `coro_close`, `coro_key`, `coro_result`, iteration over
  coroutines (LSB §5.13 rule 7), and the close-on-drop decision (LSB §10.9: closed
  at the next safepoint for tracing-GC runtimes). Reason: the coordinator scoped
  alpha.1 to everything else; this is a scope split, not a dependency block.
  alpha.1 loads modules containing them and ends a run that reaches one with
  `VmError::Unsupported` naming the opcode, function, and pc. Frames already live
  in a flat stack with explicit continuations (`Cont`), which is what stackful
  suspension needs: a suspended coroutine's frames move to the coroutine object.

## v2.0.0-alpha.2 - Coroutines
- [ ] Every instruction of LSB §5.13 with rules 1-14, keys and automatic keys,
      `finally` across suspension, `CannotSuspend` across host frames.
- [ ] Close on drop (LSB §10.9 decision) from a finalisation queue at safepoints.
- [ ] Coroutine iteration (`iter_new`/`diter_new` over a coroutine).
- [ ] Differential tests of generators and async against a reference.

## v2.0.0 - Stable (per D18: after coroutines and a real consumer)
- [ ] A real consumer runs end to end on it (Mox through the LexerSketch app).
- [ ] bytecode-lang 0.5 verifier integrated: verified modules may skip the
      loader's overlapping checks; the VM's own checks stay for unverified input.
- [ ] API review against that consumer; freeze, with STABILITY.md updated.

## Later (measured before built)
- Incremental or generational collection (today: stop-the-world, cost linear in
  the heap, rationed so total work stays proportional to allocation).
- Request-scoped arena allocation for Mox (LANGUAGES.md: shared-nothing requests).
- Fewer allocations per array (today an array is a slot plus an `Arc` plus a
  buffer); packed maps with holes, as PHP keeps them.
- Load-time quickening and fused compare-and-branch (LSB §10.4).
- Host re-entrancy (host functions calling back into bytecode) with host-lang.
