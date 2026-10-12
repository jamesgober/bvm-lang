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

## v2.0.0-alpha.2 - Coroutines (DONE, prepared 2026-10-08)
Every LSB instruction now executes.

Delivered:
- **The coroutine group** (`0xF0`..=`0xFC`, LSB §5.13 rules 1-14): stackful
  suspension (frames copied into the coroutine on `yield`/`await`, back on
  `resume`: LSB §10 question 7 answered as copy-on-suspend), suspension
  through nested calls and hook frames, `resume`/`resume_throw` (including
  on `created`), automatic and explicit keys (`yield_kv`, `coro_key`, rule 10
  as written: the `map_push` rule), return values (`coro_result`),
  `coro_close` with pending `finally` blocks, `CloseIgnored`, and `await`
  during a close, `finally` blocks that suspend with their pending
  completion, `coro_status`/`coro_current`, iteration over coroutines with
  `iter_new`/`diter_new` (rule 7), `spawn` through hook 27 (`NoScheduler`
  without it), coroutine frames counted against the depth limit and
  suspended stacks against the memory budget, traps failing the running
  chain. `coro_new` is checked at load like `call`.
- **Close on drop** (rule 13 as decided in LSB §10 question 9): this heap is
  traced, so the collection that finds a suspended coroutine unreachable
  resurrects and queues it (creation order); queued coroutines close one at
  a time at the next fuel charge point of a run (the fuel counter is set
  aside while a close is pending, so the dispatch loop has no check of its
  own), or from the host with `Vm::run_finalizers`. Drop signal `nil`;
  outcomes discarded; at most once per coroutine. LSB §5.13 rule 13 and its
  GC paragraph rewritten accordingly.
- **Scheduler**: `Host::register_scheduler` (the built-in `spawn` import)
  and `Vm::run_async` (FIFO tasks, `await` of tasks and values, deadlock
  detection, one fuel budget).
- **Fuel rule written into LSB** (new §5.14), including the backward-branch
  and handler-entry charges alpha.1 added, the coroutine charges, and the
  exact point each charge reports `OutOfFuel` at. Indirect calls to imports
  now check arity before charging, as for bytecode callees.
- **Tests**: `coroutines.rs`, `scheduler.rs`, `coroutine_gc.rs`; a
  whole-module reference interpreter (`tests/common/full.rs`, segmented
  coroutine stacks) and a differential property over random six-function
  modules with calls (direct, indirect, dynamic, tail), closures, arrays,
  maps, structs, strings, try/catch, try/finally (including `return` in
  `finally` overriding a pending completion), and coroutines, under random
  fuel and depth limits; mutation-checked (10 of 11 injected bugs caught by
  it, the 11th by `coroutines.rs`).
- **Benches**: generator iteration, coroutine creation, async ping-pong.

### Dependency wiring
- **bytecode-lang `0.2`**, unchanged: the coroutine instructions,
  `CoroState`, `Hook::Spawn`, and the coroutine error kinds were already in
  the format. LSB format 1 has no per-coroutine "close on drop" flag (the
  decision says the format will carry one), so every suspended coroutine is
  closed on drop; the flag arrives with a later format version.
- **gc-lang**: still not wired (reasons under alpha.1). Close on drop needs
  resurrection of unreachable objects during a collection, which the VM heap
  now does and gc-lang 1 does not offer.
- **host-lang**: not wired. Host functions still never call back into
  bytecode, so no host frame can sit between a coroutine and its `yield`;
  `CannotSuspend` across host frames becomes reachable only when host-lang
  adds re-entrancy.

### Known limitations (recorded, not deferred work of this milestone)
- Rule 10 as written (the `map_push` rule) gives `-4` after only
  `yield -5 => x`; PHP's generators give `0` (they start the counter at -1).
  The VM follows LSB; whether LSB should change is an owner's call.
- `call/fib25` measured 3-10% slower than alpha.1 in alternating runs on a
  loaded machine; typed dispatch is at parity. Not isolated. (Isolated and
  fixed in alpha.3: the return path.)

## v2.0.0-alpha.3 - LSB format 2 (DONE, prepared 2026-10-09)
Executes the format bytecode-lang 0.3 publishes, every item of the "Next for
bvm-lang" list in bytecode-lang's v0.3.0 release notes.

Delivered:
- **Loading format 2**: format 1 refused (`UnsupportedVersion(1)`); the new
  modifier layouts (`Policy`, `IntOp`, `FloatConv`); parameter lists of
  functions and imports checked (valid, fitting the signature, names in the
  string table, by-reference parameters `dyn` or `ref` to a `cell dyn`:
  `LoadErrorKind::BadParamList`), call shapes checked
  (`LoadErrorKind::BadCallShape`) and `dcall_shape` windows inside the frame,
  `raise` limited to catchable kinds, hooks 28-30 checked.
- **The 18 new instructions and the three renamed ones** execute.
- **Transparent reference slots** (LSB §5.17) in every dynamic slot read and
  write: `dget_index`, `dset_index`, `get_prop`, `set_prop`, `get_field`,
  `set_field`, `array_get`/`set`/`push`/`pop`, `map_get`/`find`/`set`/`push`,
  `iter_next`, `dsep_*`, spreads, and the host and inspection APIs. A
  reference word carries a flag bit, so the check is one compare on the word
  (plus the slot's type when it fires).
- **Copy-on-write separation** (LSB §5.16) with the `cow`/`aliased` bits of
  the spec's tracing-collector implementation, deterministic (no reference
  counts, no collection timing).
- **Parameter-list binding** in `dcall`, `dcall_shape`, `coro_new_indirect`,
  and `spawn` (an allocation-free positional binder property-tested against
  `ParamList::bind`, which binds calls with names), fuel charged after
  binding, the `call_shape` hook, `ArgumentError`.
- **Import parameter lists** and host functions as first-class values, with
  `HostCtx` access to references and containers.
- **OPS v2**: `ipow`/`fpow`/`dpow`/`dabs`, `shift = saturate`,
  `NegativeExponent`; the shared `ls_pow` routine, specified with a vector
  table in `_lexersketch/specs/ops-vectors/pow.md` (written in this
  milestone: pseudocode, constants, 122 vectors, verification).
- **`raise` and error payloads** (`err_payload`, `VmError::Raised { payload }`,
  `Vm::error_payload`).
- **Rule 10 PHP generator keys.**
- **Tests**: PHP-semantics conformance (`tests/php_semantics.rs`), format 2
  conformance (`tests/format2.rs`), the `ls_pow` vectors
  (`tests/pow_vectors.rs`); both differential references extended to every
  item above (the whole-module reference with its own binder written from
  §5.15 and its own model of the two bits), a PHP-focused whole-program
  property; mutation-checked (see the release notes).
- **The alpha.2 `call/fib25` regression isolated and fixed**: the return
  path matched every continuation before writing a typed call's result;
  testing that one first makes `fib(25)` ~7% faster than alpha.1 (min-of-50
  harness, alternated processes). A `map_set` slowdown found the same way
  (a separate lookup for the reference-slot check) was folded back into the
  one lookup.
- **Benches**: dynamic calls through the binder (positional and named),
  the nested-append loop with separation against the `dup`-per-write
  lowering it replaces, `foreach` by reference.

### Dependency wiring
- **bytecode-lang `0.3`** (crates.io): format 2, `ParamList::bind`. The
  binder is the shared statement of the rules; the VM calls it for named
  calls and is differential-tested against it for positional ones.
- **gc-lang, value-lang, host-lang**: unchanged (reasons under alpha.1 and
  alpha.2).

### Moved to 2.0.0-alpha.4 (recorded per the anti-deferral rule)
- **The reference-counting memory profile** (DECISIONS D22): refcounts plus a
  cycle collector, selected by `[runtime] memory = "rc"`, with PHP's
  deterministic destructor and close-on-drop timing and its reference
  unbinding on copy (refcount-1 references revert to values). Reason: D22
  sequences it as alpha.4; alpha.3 is format 2 on the tracing heap. The seam
  is in place: every copy-on-write and separation decision goes through
  `contents_shared` (array and map objects) and `Heap::is_aliased`, which a counted heap answers
  with "the count is above one".

### Known limitations (recorded, not deferred work of this milestone)
- LSB keeps a reference slot until `dunref_*` unbinds it; PHP drops a
  refcount-1 reference when the array is copied. The tracing profile cannot
  observe the count (LSB §5.17 "the one divergence"); alpha.4's RC profile
  can.
- `ls_pow` is correctly rounded except within about 2^-85 of a rounding
  boundary (one such case is in the vector table); deterministic either way.

## v2.0.0-alpha.4 - Reference-counting memory profile (DONE, prepared 2026-10-12)
DECISIONS D22, ISSUES P30 (the VM side), P33, and P34's runtime side.

- [x] Refcounts plus a cycle collector behind the `contents_shared`/
      `is_aliased` seam, selected per program (`memory = "rc"`).
- [x] PHP's reference unbinding on copy, destructor and close-on-drop
      timing at the last reference.
- [x] Fuel-proportional parameter binding (P33).

Delivered:
- **The RC profile** (`Memory::Rc`, `Vm::with_memory`, `Vm::memory`):
  counts in the existing slots (a `u32` and three collector bits in the
  slot's padding, so the slot did not grow), kept by `Heap::inc`/`dec` at
  every store of a counted location: reference-typed registers (the
  dispatch loop's `w!` macro; the loop is compiled once per profile, so the
  tracing loop is unchanged), frames' arguments and closures (adopted at
  push, released at pop, tail call, unwind, and abnormal end), suspended
  coroutine frames (moved, not recounted), object slots (fresh contents
  adopted at allocation; struct, closure, and coroutine children through
  `Heap::adopt`), array and map *storage* counted once per `Arc` (adopted
  by the container that creates it, released by the one that drops the
  last `Arc`, recounted by a copy-on-write copy), globals, caches (held for
  the VM's life), the resume chain, the scheduler, and host values (pinned
  until the next run or `collect_garbage`, the RC version of "valid until
  the next run").
- **Deferred freeing at instruction boundaries**: a count reaching zero
  puts the word on the zero list; the RC dispatch loop settles it before
  the next instruction (`Machine::settle`), so no word an instruction holds
  goes stale mid-instruction, and order inside an instruction never
  matters. Settling is iterative (the list is the work list) and follows
  PHP's order (frames release their last register first so the first
  settles first; objects release children in reverse for the same reason).
  Destructors and drop-closes started there push a *floor* on the zero
  list, so objects that died together are finalized one after another,
  each with what its freeing releases, never nested in each other, while
  what the running code drops is finalized inside it (PHP). Queued
  finalizations start one at a time.
- **The cycle collector** (`src/cycle.rs`): synchronous trial deletion
  (Bacon and Rajan 2001), with shared storage as nodes keyed by `Arc`
  address, finalizable garbage rescued with what it reaches (marked
  destructed/finalized, queued), PHP's adaptive root threshold, work lists
  everywhere, one unit of fuel per 64 visits. Runs at the existing `gc!`
  points (the start of allocating instructions, where every live object is
  counted), on a full buffer, a tight budget after a recount, or
  `collect_garbage`.
- **Memory accounting** under RC: frees refund what is certainly theirs, so
  the estimate is never low; exact recounts rationed like tracing
  collections (a `dup`-heavy loop no longer drifts into a spurious
  `OutOfMemory`; a test fails without the rationing).
- **Destructors in both profiles** (`Vm::set_destructor`, validated:
  `VmError::BadDestructor`): RC at the last reference; tracing through the
  finalization queue after the coroutines (the collector scans the slots
  for unmarked structs with a destructor only when one is set).
- **Close on drop at the last reference** (RC), through the same settle.
- **PHP's unbinding on copy** (RC): copy-on-write copies and struct `dup`
  replace reference slots whose box has count 1 by its value (separated by
  `coll::share` when an array or map); writes through such a slot of
  shared contents copy first. `MemoryStats::unbound` counts them.
- **The seam**: `contents_shared` and `Heap::is_aliased` keep answering
  with LSB §5.16's bits in both profiles. A count-based answer (`cow` and a
  second `Arc` holder; `aliased` and a count above one, which LSB allows)
  was built first and failed the RC differential test: on a generated
  program that stores a container without `dup` (outside the value
  discipline LSB's guarantee assumes) it separated differently from
  tracing and the reference, and a count also sees dead temporary
  registers as holders. Counts decide only whether a copy is physical,
  which is where PHP's unbinding happens.
- **Saturation**: a count past `rc_max` (`u32::MAX`) sticks (`STUCK`), the
  object is never freed again, and the run traps `OutOfMemory` at the next
  boundary; tested with the ceiling lowered.
- **P33**: `bind_named` (hashed `NameIndex` per callable with more than 8
  named parameters, built at load; ordered set for rest names), property-
  tested against `ParamList::bind` with and without the index;
  `charge_items` (`n / 64` extra units for a flattened call of `n` items,
  before binding); empty rest collections share the machine's empty
  storage until written.
- **`MemoryStats`/`Vm::memory_stats`.**
- **Tests**: `tests/memory_rc.rs` (26), `tests/binding.rs` (3), RC
  differential properties over both whole-module generators (with a
  coverage guard), RC fuzzing in `tests/untrusted.rs`, and two debug-build
  invariant checks every RC run and collection performs (an exact recount
  of every count; nothing unreachable kept), both mutation-checked.
- **Benches**: `profile/*/{tracing,rc}` and `call/dcall_spread_named_200k`.

### Dependency wiring
- **bytecode-lang `0.3`**, unchanged. Format 2 has no `[runtime]` section,
  no memory-profile flag, and no destructor flag, so both are chosen by
  the host: `Vm::with_memory` (the runner maps `[runtime] memory`) and
  `Vm::set_destructor` (the language's destructor method name). Recorded
  for LSB (owner/LSB maintainer's call, not this crate's): a module-level
  `memory` profile and destructor method once LSB carries runtime
  metadata; D22's "specified per profile in LSB" text for destructor
  timing, reference unbinding (§5.17), and close-on-drop (§5.13 rule 13
  already has it); and §5.14 additions for the two new charges (`n / 64`
  per large dynamic call, one per 64 cycle-collector visits under RC).
- **gc-lang, value-lang, host-lang**: unchanged (reasons under alpha.1 and
  alpha.2). gc-lang 1 has no counts either; the RC heap stays VM-specific.

### Known limitations (recorded, not deferred work of this milestone)
- Errors escaping a destructor are discarded (as LSB §5.13 rule 13 does for
  drop-closes); PHP raises them where the object died. Raising them there
  would need a precise point after an instruction has completed; revisit
  with the consumer (Mox).
- Objects still reachable when a VM is dropped are not destructed; PHP's
  shutdown pass is left to the host (`collect_garbage` +
  `run_finalizers`).
- A destructor whose frame is suspended by a `yield` (a destructor running
  inside a coroutine that suspends) keeps the zero list's floor until it
  resumes or the run ends: objects that died before it are freed late, not
  lost.
- RC counts registers the way the tracing collector roots them: a dead
  register still holding a value keeps it alive until overwritten or its
  frame returns, so destructor timing follows the code generator's register
  reuse (PHP frees temporaries at once). bcgen-lang can clear dead
  registers (`load_nil`) where PHP timing matters.
- A program building a long chain one node at a time fills the
  possible-roots buffer (each node loses its register holder), so the cycle
  collector runs O(sqrt(n)) times over the chain built so far: O(n^1.5)
  work in all, fuel-charged (PHP behaves the same; measured in
  `tests/memory_rc.rs`).
- Tracing against alpha.3 (alternating harness, minimum of four rounds):
  `map/int_keys_100k_set_get` ~4% slower and `php/foreach_by_ref_100k`
  ~5-8% slower, the cost left of routing the shared store helpers by
  profile (the rest is at parity or faster: `fib(25)` ~8% faster). Not
  recovered in this milestone.
- The exact recount and the reachability check run in debug builds only
  (and only on heaps of at most 50,000 slots); release builds trust the
  counts.

## Moved to 2.0.0 (recorded per the anti-deferral rule)
- `[runtime] cycles = "leak"` (LSF2 §17: RC without a cycle collector) is
  not exposed: it was not in this milestone's scope, and with no consumer
  asking for it the switch would be untested surface. Adding it is one flag
  that keeps `collect_cycles` from running automatically.

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
