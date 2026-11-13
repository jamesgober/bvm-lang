# bvm-lang &mdash; API Reference

> Complete reference for every public item in `bvm-lang`, with examples.
> **Status: 2.0.0-alpha.4, a pre-release** executing LSB format 2
> (bytecode-lang 0.3) under two memory profiles, tracing and reference
> counting. The surface may still change before
> `2.0.0` (see [Stability](#stability) and [`STABILITY.md`](./STABILITY.md)).
> Instruction semantics are those of LSB (`_lexersketch/specs/LSB.md`) and OPS
> (`_lexersketch/specs/OPS.md`); this file documents the Rust API around them.

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

## Table of contents

- [Overview](#overview)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Concepts](#concepts)
  - [Registers and values](#registers-and-values)
  - [What loading checks](#what-loading-checks)
  - [Errors, traps, and codes](#errors-traps-and-codes)
  - [Fuel and the other budgets](#fuel-and-the-other-budgets)
  - [Hooks](#hooks)
  - [Maps as PHP arrays](#maps-as-php-arrays)
  - [Dynamic calls and parameter lists](#dynamic-calls-and-parameter-lists)
  - [Copy-on-write and separation](#copy-on-write-and-separation)
  - [References](#references)
  - [`pow` and the shared `ls_pow`](#pow-and-the-shared-ls_pow)
  - [Coroutines](#coroutines)
  - [Tasks and the built-in scheduler](#tasks-and-the-built-in-scheduler)
  - [Memory and collection](#memory-and-collection)
  - [Memory profiles](#memory-profiles)
  - [Destructors](#destructors)
  - [Instruction coverage](#instruction-coverage)
- [`Program`](#program)
  - [`Program::load`](#programload)
  - [`Program::decode`](#programdecode)
  - [`Program::module`](#programmodule)
  - [`Program::export`](#programexport)
  - [`Program::location`](#programlocation)
- [`Vm`](#vm)
  - [`Vm::new`](#vmnew)
  - [`Vm::with_limits`](#vmwith_limits)
  - [`Vm::with_memory`](#vmwith_memory)
  - [`Vm::set_destructor`](#vmset_destructor)
  - [`Vm::set_hash_seed`](#vmset_hash_seed)
  - [`Vm::run`](#vmrun)
  - [`Vm::run_with`](#vmrun_with)
  - [`Vm::run_export`](#vmrun_export)
  - [`Vm::run_async`](#vmrun_async)
  - [`Vm::run_finalizers`](#vmrun_finalizers)
  - [Inspecting values](#inspecting-values)
  - [Heap and fuel statistics](#heap-and-fuel-statistics)
- [`Limits`](#limits)
- [`Memory`](#memory)
- [`MemoryStats`](#memorystats)
- [`Value`](#value)
- [`Obj`](#obj)
- [`Host`](#host)
- [`HostCtx`](#hostctx)
- [`HostError`](#hosterror)
- [`VmError`](#vmerror)
- [`LoadError`](#loaderror)
- [`LoadErrorKind`](#loaderrorkind)
- [`Location`](#location)
- [Constants](#constants)
- [Feature flags](#feature-flags)
- [Stability](#stability)

## Overview

`bvm-lang` executes LSB modules. [`Program::load`](#programload) turns a
`bytecode_lang::Module` into a checked, import-bound [`Program`](#program); a
[`Vm`](#vm) runs its functions under [`Limits`](#limits) and returns
[`Value`](#value)s or a [`VmError`](#vmerror). Host functions, registered in a
[`Host`](#host), serve the module's imports and, through imports, its hooks.

## Installation

```toml
[dependencies]
bvm-lang = "=2.0.0-alpha.4"
bytecode-lang = "0.3"
```

## Quick start

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{ExportItem, Inst, IntOp, IntTy, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let mut f = m.function("add", &[ValType::I64, ValType::I64], &[ValType::I64]);
let r = f.reg(ValType::I64);
f.emit(Inst::IAdd { dst: r, lhs: f.param(0), rhs: f.param(1), op: IntOp::new(IntTy::I64) });
f.ret(r);
let add = m.add_function(f).unwrap();
m.export("add", ExportItem::Func(add));

let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&program);
assert_eq!(vm.run_export("add", &[Value::Int(40), Value::Int(2)]), Ok(Value::Int(42)));
```

## Concepts

### Registers and values

Every register is a 64-bit word; its declared LSB type gives it meaning.
Integers are stored sign- or zero-extended and read back narrowed to their
type; `f32` registers hold the float's bits; `bool` holds 0 or 1; `char` a
scalar value. `str`, `ref t`, and `dyn` registers share one encoding (LSB
§2.1), in which the zero word is `nil`:

| `dyn` kind | Encoding |
|---|---|
| `nil`, `bool`, `char` | immediates |
| `int` | inline when in [-2^48, 2^48), otherwise a boxed heap object; always 64-bit to the program |
| `float` | the `f64` bits offset by 2^49; NaN canonicalised (OPS §4) |
| heap objects | a 32-bit slot index, a 15-bit generation, and a flag set exactly for a PHP reference (kind `reference`, LSB §5.17), so a slot access tells a reference slot from a value with one compare on the word |

At the API boundary a register becomes a [`Value`](#value): signed integers
and `dyn` ints as `Value::Int`, unsigned integers as `Value::UInt`, `f64` and
`dyn` floats as `Value::Float`, `f32` as `Value::F32`, heap objects as
`Value::Obj`. Arguments convert the other way and must fit their parameter's
type (`VmError::ArgumentType` otherwise).

Conversions between `dyn` and typed registers (`to_dyn`, `from_dyn`, and every
place LSB applies their rules: `dcall` arguments and results, `set_prop` on
fields, dynamic indexing of typed collections) follow LSB §5.7. One reading
is worth stating: `from_dyn` into `str` accepts `nil` as well as strings,
because `nil` is a value of type `str` (LSB §2.1) and `to_dyn` of a `nil`
string must convert back.

### What loading checks

LSB's verifier arrives with bytecode-lang 0.5. Until then (and for modules
that skip it), [`Program::load`](#programload) proves in one linear pass
every fact the interpreter indexes by, so the dispatch loop never meets an
out-of-range index:

- every register operand, including every register of every call window
  (`call`, `dcall`, `make_closure`, `str_concat_n`, `str_slice`, ...), is
  inside the function's frame;
- every constant, function, import, global, jump-table, name, type, capture,
  and string index is in range; every branch, table, and handler target is an
  instruction;
- the last instruction cannot fall through; direct calls pass their callee's
  parameter count and never target a function with captures;
- `new_struct`/`new_array`/`new_map`/`new_cell` name types of that kind;
  `overflow = promote` appears only on instructions writing a `dyn` register;
  `from_dyn` never targets `ref`; bit casts use 32- or 64-bit integers;
- handlers are non-empty, in range, with `dyn` catch registers, and no tail
  call lies inside one;
- struct parents are structs whose fields prefix the child's, chains are
  acyclic and at most [`MAX_INHERITANCE_DEPTH`](#constants) deep; aggregate
  constants refer only to earlier ones and nest at most
  [`MAX_CONST_DEPTH`](#constants) deep;
- hook callees and the start function have the signatures LSB requires;
  every import has a registered host function;
- every parameter list (of a function or an import) is valid and fits its
  signature (LSB §5.15: kinds in order, names present and unique and in the
  string table, no default on a rest parameter, rest parameters `dyn`,
  by-reference parameters `dyn` or `ref` to a `cell dyn`, the `i64` presence
  mask last when a parameter has a default); every call shape is valid
  (at most 255 entries, nothing positional after a named entry, no repeated
  name, names in the string table) and every `dcall_shape` window fits the
  frame; `raise` names a catchable error kind;
- the module is LSB format 2: [`Program::decode`](#programdecode) refuses
  format 1 bytes (`UnsupportedVersion(1)`), which must be regenerated.

It does **not** check the verifier's type rules. A module that, say, adds two
`str` registers with `iadd` loads and runs; it computes a meaningless word,
and heap instructions given such a word raise `TypeError` or
`NullReference`. Memory safety never depends on types: every heap access
checks the object's kind and liveness.

### Errors, traps, and codes

A failing instruction **raises** at its own pc without writing its
destination (LSB §4.3). The VM searches that function's handlers in order;
the first covering the pc receives the error value (a `dyn` of kind `error`,
or the thrown value) and execution continues at its target. Otherwise the
frame is popped and the search continues at the caller's call instruction.
An error leaving the entry frame ends the run with
[`VmError::Raised`](#vmerror) (runtime errors: kind, payload, function, and
pc of the raising instruction) or [`VmError::Thrown`](#vmerror) (any other
thrown value).

`raise kind, src` raises any catchable error kind with `src` as its
**payload** (LSB §5.9, §6): how a code generator raises `NoMatch` (E0200)
with the unmatched value, or `ArgumentError` from a prologue. `err_payload`
reads it back (`nil` for an error an instruction raised by itself, and for a
value that is not an error), as do [`Vm::error_payload`](#inspecting-values)
and `VmError::Raised { payload, .. }`.

**Traps** abort without visiting handlers: `OutOfFuel` (E0107),
`OutOfMemory` (E0106), `Unreachable` (E0109), and any OPS error under policy
`trap` (same code as the error). Codes are those of
`bytecode_lang::ErrorKind`; `err_code` reads them from caught error values.

### Fuel and the other budgets

Fuel follows LSB §5.14, the rule every tier shares so that a run under a
budget stops at the same function and pc on each. One unit is charged at
every `safepoint`, every call-family instruction (`call`, `call_indirect`,
`call_import`, `tail_call`, `tail_call_indirect`, `dcall`, `dcall_shape`, and
every hook invocation), every taken branch to a target at or before the branching
instruction (`jmp`, `jmp_if`, `jmp_if_not`, `switch`), every handler entry,
every coroutine instruction (`spawn` once more for its hook), and every
`iter_next` over a coroutine. `call`, `tail_call`, `call_import`, `safepoint`,
and the coroutine instructions charge before anything else;
`call_indirect` and `tail_call_indirect` charge once the callee is known to
be callable with that many arguments (a `nil` callee, a non-function, or an
arity mismatch raises uncharged); `dcall` and `dcall_shape` charge once the
callee is known to be callable and its arguments bind to its parameter list
(LSB §5.15 step 4: a flattening or binding error raises uncharged, a
conversion error after binding is charged; on a non-callable the hook's
invocation is the charge). Between two charges a
run executes at most one function's length of instructions, so fuel bounds
total work even for modules a verifier would reject for lacking safepoints.

Three charges go beyond LSB §5.14 (all recorded for LSB in the ROADMAP):

- **Data work.** An instruction that copies, hashes, or compares data
  pays one unit per 256 bytes on top of any unit it already costs: every
  byte it builds or copies (a concatenation, a slice, a new array, a struct
  copied by `dup`, the copy that separates shared copy-on-write storage),
  every string key it hashes or compares in a map, and every string it
  compares (`str_eq`, `str_cmp`, and the string cases of `deq`, `dlt` and
  their kin). Work a host function does is the host's and is not charged. The work is
  recorded as it happens and paid when the instruction finishes, so a
  short program that copies a megabyte per iteration spends fuel in
  proportion to the bytes, not the iterations. When what is left cannot
  pay, the run ends with `OutOfFuel` at that instruction, which has
  completed. The charge is rounded down per instruction, so an
  instruction that moves less than 256 bytes costs nothing extra and
  ordinary code keeps the exact fuel LSB §5.14 states.
  [`Limits::with_data_fuel(false)`](#limits) turns this charge off.

- **Large dynamic calls** (ISSUES P33). A `dcall_shape` whose flattened
  call has `n` items is charged `n / 64` units more, right after flattening
  and before binding (so a spread of a million items cannot be bound for
  one unit, and binding errors after a large flattening are charged these
  units); the `OutOfFuel` trap there has no effect. Calls of fewer than 64
  items, every call whose arguments are a window among them, cost exactly
  what LSB says.
- **The RC cycle collector** charges one unit per 64 objects it visits (at
  least one per automatic collection), so a program cannot make collection
  work unbounded; a budget it spends traps at the next charge point. This
  happens only under [`Memory::Rc`](#memory), when its possible-roots buffer
  fills, so where it lands depends on allocation, like the tracing
  collector's timing.

[`Limits`](#limits) also bounds heap bytes (the `OutOfMemory` trap; large
buffers are checked before they are requested; suspended coroutines' stacks
count), call depth (hook frames and running coroutines' frames included),
and register-stack slots (both raise the catchable `StackOverflow`, E0105,
at the call or `resume`).

### Hooks

The dynamic instructions take a built-in fast path (numbers, strings,
collections, structs) and otherwise call the module's hook for that
operation (LSB §5.8), bound to a module function or an import. A function
hook runs as a call (a frame, fuel, depth); an import hook runs the host
function. Format 2 adds `pow` (28, the `dpow` slow path), `abs` (29, `dabs`),
and `call_shape` (30): a `dcall_shape` of a non-callable calls it with the
callee, the positional arguments as a `dyn` array, and the named ones as a
`dyn` map (a repeated name is `ArgumentError`), or, with it unbound and no
argument named, the `call` hook with the positional arguments. Results are converted as LSB states: `eq`, `lt`, `le`, `truthy`, and
`has_prop` must return a `dyn` bool (`TypeError` otherwise; `dne` negates),
`len` a `dyn` int, `iter` an array, map, or iterator. With no hook bound, each
instruction has its LSB fallback: `TypeError` for arithmetic, identity for
`deq`, the built-in rule for truthiness, `IndexOutOfBounds`/`KeyNotFound` for
indexing misses, `UndefinedProperty` for properties, `false` for `has_prop`.

### Maps as PHP arrays

A map keeps insertion order. Setting an existing key updates it in place; a
new key goes last. `map_push` inserts under the next integer key, one more
than the largest integer key ever inserted (PHP 8.3: after only `-5`, the
next is `-4`); deletions never lower it, and exhausting `i64` (or the key
type) raises `ArithOverflow`. Iteration follows the live map: deleted
entries are skipped and appended ones visited; iterating a `dup` gives PHP's
value semantics, at O(1) cost until either copy is written. Keys compare per
LSB §2.4: integers, bools, and chars by value, strings bytewise, other objects
by identity, floats by bits with `-0.0` folded into `+0.0` and one NaN, and
for `dyn` keys the kind is part of the key (`1` and `1.0` differ).

While a map's keys are exactly `0, 1, 2, ...` with nothing deleted it is
*packed*: no hash index exists and lookups are a bounds check.

### Dynamic calls and parameter lists

A function or an import may carry a parameter list (LSB §5.15,
`bytecode_lang::ParamList`): one entry per signature parameter with a kind
(positional-only, normal, named-only, rest, rest map, named rest), a name, a
by-reference flag, and a default flag, plus `ignore_extra` (PHP user
functions). A `dcall_shape` site names a call shape (positional, named,
spread, named spread per window register); `dcall` is the all-positional
case. The VM binds exactly as `ParamList::bind` states it:

1. a spread array gives one positional item per element, a spread map one
   per entry (positional for an `int` key, named for a `str` key, otherwise
   `ArgumentError`), a named spread its `str`-keyed entries; spreading
   anything else is `TypeError`; spread elements read reference slots as
   their values;
2. positional items fill the positional parameters in order, extras go to
   the `rest`/`rest_map` parameter, are dropped under `ignore_extra`, or are
   an `ArgumentError`; named items fill the parameter of that name, else the
   named rest, else the rest map, else `ArgumentError`; a positional item
   after a named one, a parameter given twice, and a missing parameter
   without a default are `ArgumentError` (E0114);
3. one unit of fuel is charged;
4. a by-reference parameter receives a reference (the argument itself when
   it is one, else a new reference holding it); a by-value parameter given a
   reference receives its value, separated as by `dup` when it is an array
   or map; arguments convert to their parameter types by `from_dyn` rules
   (`TypeError`); a `rest` parameter receives a new `dyn` array, `rest_map`
   and `rest_named` a new `dyn` map; the presence mask (bit `i` set when
   parameter `i` received an argument) is the last argument when any
   parameter has a default, so the callee computes the others' defaults.

A callee without a list takes exactly its parameters, positionally. All-
positional calls take a binder that allocates nothing; calls with named items
take a binder that finds each named parameter through a hashed name index
built at load time for lists of more than eight named parameters (a scan
for smaller ones) and checks names repeated in a rest collection with an
ordered set, so binding `n` items is O(n log n) whatever the list (ISSUES
P33). Both are property-tested against `ParamList::bind`, slot for slot and
presence bit for presence bit. A rest parameter that receives nothing gets
an empty collection sharing one empty storage, which it copies on its first
write.
`coro_new_indirect` and `spawn` with a `dyn` callee bind the same way.
`dparam_ref`/`dparam_ref_named` answer, before an argument is evaluated,
whether the callee takes it by reference (false for non-callables, callees
without a list, negative positions, and arguments nothing takes); they never
raise and are not charged.

`load_import` makes any import a function value. A host function called
through a parameter list receives the bound arguments in signature order:
references for by-reference parameters (read and write them with
[`HostCtx::ref_get`/`ref_set`](#hostctx)), the rest collections, and the
presence mask.

### Copy-on-write and separation

Arrays and maps share their contents between a container and its `dup`s
(and the loads of an aggregate constant), copied on the first write. Each
carries LSB §5.16's two bits: `cow` ("my contents may be shared": set on both
sides by `dup`, cleared by the first write) and `aliased` ("another
container's contents may hold me": set on every array and map in contents a
write to a `cow` container copies, and in the fields of a struct or cell
`dup` copies). `dsep_index`/`dsep_prop` write the outer container, then
replace the element by an O(1) `dup` exactly when it is aliased, so a nested
write `$a[$k][] = $v` is visible through nothing else, and the steady loop
`$g[$i % 64][] = $i` copies no contents after its first pass. The decisions
follow the bits, never reference counts or collection timing, so they are
deterministic; which object `dsep_*` returns when nothing was shared is not
specified by LSB, and value-semantics code never asks.

Both [memory profiles](#memory-profiles) make these decisions with the
bits. LSB §5.16 would let a reference-counted runtime separate exactly when
a count is above one instead; that gives the same contents for code that
keeps the value discipline, but not for code that stores a container without
`dup` (the differential tests generate such code and found a case), and it
would count a dead temporary register as a second holder and copy on every
pass of a loop. With the bits, the profiles separate alike on every program.
Counts decide only whether a copy is *physical* (another container still
holds the storage), which is where [`Memory::Rc`](#memory) unbinds
references (next section).

### References

A reference (kind `reference`, LSB §5.17) is a box holding one `dyn` value,
read and written with `cell_get`/`cell_set` and made by `new_ref`. `dref_index`/
`dref_prop` turn a slot (an array element, a map value, a `dyn` struct field)
into one, separating the slot's value first (`&$a['k']`, `foreach ($a as
&$v)`; an absent map key is appended holding `nil`), `dbind_index`/
`dbind_prop` bind a slot to an existing reference (`$a['k'] = &$x`), and
`dunref_index`/`dunref_prop` turn a reference slot back into a value slot
(separated as by `dup`) where a code generator proves the last other holder
is gone. A reference slot is transparent: every instruction reading the slot
as a value (`dget_index`, `get_prop`, `get_field`, `array_get`, `map_get`,
`map_find`, `array_pop`, `iter_next`, `dsep_*`, spreads, and
[`Vm::elements`/`entries`/`field`](#inspecting-values)) reads the
reference's value, and every value write (`dset_index`, `set_prop`,
`set_field`, `array_set`, `map_set`) writes through it; a reference given as
a value to store stores its value. A copy of a container shares its
reference slots (PHP's behaviour while the reference is held elsewhere);
`dup` of a reference itself is a `TypeError`. References, like cells, are
traced, so reference cycles are collected.

**Unbinding on copy** (LSB §5.17 "the one divergence"; ISSUES P34). PHP
also drops a reference when an array is copied and nothing else holds it
(its refcount is 1): after `foreach ($a as &$v) {} unset($v);`, `$b = $a;
$b[0] = 7;` leaves `$a[0]` alone. The tracing profile cannot see counts, so
it keeps LSB's rule: the copy shares every reference slot until `dunref_*`
unbinds it. Under [`Memory::Rc`](#memory) every copy of contents (the
copy-on-write copy a write makes, and `dup` of a struct, PHP's `clone`)
replaces each reference slot whose reference only the copied contents hold
by a plain slot of the reference's value (separated as by `dup` when an
array or map), exactly PHP's `zend_array_dup` rule, and a write through a
reference slot of shared contents makes the copy first (PHP's
`SEPARATE_ARRAY`). A reference still bound elsewhere (`$v` above, not
unset) stays shared, in both profiles. This covers PHP's semantics even
when a code generator never emits `dunref_*` (bcgen-lang 0.3 does not).
[`MemoryStats::unbound`](#memorystats) counts the unbindings.

### `pow` and the shared `ls_pow`

`ipow` is OPS v2's exact integer power under the overflow policy (`wrap`
keeps the low bits; a negative exponent is `NegativeExponent`, E0006, under
every policy, `trap` included). `dpow` takes it on two ints, with `promote`
giving the `f64` nearest the exact power and, for a negative exponent, the
float rule (PHP's `2 ** -1` is `0.5`). Float powers (`fpow`, `dpow` with a
float) use `ls_pow`, the family's shared routine specified operation by
operation in `_lexersketch/specs/ops-vectors/pow.md` so every tier produces
the same bits; this VM's implementation passes the spec's vector table.
`dabs` is `abs` with `abs(i64::MIN)` per the overflow policy (`promote`:
`2^63` as an `f64`). The shift policy `saturate` is PHP's `<<`/`>>`: an
amount at least the width gives `0` (or `-1` for `>>` of a negative value),
and a negative amount is `ShiftOutOfRange`.

### Coroutines

The coroutine group (LSB §5.13) is stackful and asymmetric. `coro_new`
(or `coro_new_indirect` with a closure) makes a coroutine; `resume` runs it
until it `yield`s, `await`s, returns, or fails, and that is what `resume`
writes (or raises). A `yield` may be any number of calls (hook calls
included) below the coroutine's body: every frame above the body is
suspended with it. Values crossing a suspension are `dyn`.

- **Keys** (`yield_kv`, `coro_key`, `iter_key`): `yield` uses the automatic
  key of PHP's generators (LSB rule 10, format 2): a counter starting at -1,
  raised by every larger explicit integer key, and `yield` takes one more
  (`ArithOverflow` past `i64::MAX`). After only `yield -5 => x` the next
  automatic key is `0`; maps differ on purpose (`map_push` after only `-5`
  gives `-4`, as PHP 8.3 arrays do).
- **Throwing in** (`resume_throw`) raises at the suspension point, so a `try`
  around the `yield` catches it; on a `created` coroutine nothing runs, it
  fails, and the error is raised at the `resume_throw`.
- **Closing** (`coro_close c, signal`) raises `signal` at the suspension point
  so pending `finally` blocks run. The signal escaping (the same value) or a
  return is success (`returned`); another error fails it and is raised at
  the closer; a `yield` while closing leaves it suspended there and raises
  `CloseIgnored` (E0113) at the closer; an `await` while closing suspends
  normally and the closer drives it with `resume`.
- **Iteration**: `iter_new` and `diter_new` accept a coroutine; `iter_next`
  resumes it with `nil` (has = true and the value on `yielded`, false once
  `returned`, `TypeError` on `awaiting`, the error on failure).
- **Errors**: resuming a `running` (itself or a resumer), `returned`, or
  `failed` coroutine is `InvalidCoroState` (E0110), as is `coro_result`
  before it returned; `yield`/`await` with no running coroutine is
  `CannotSuspend` (E0111); `spawn` without a `spawn` hook is `NoScheduler`
  (E0112). A coroutine body must be a bytecode function: a closure of an
  import given to `coro_new_indirect` or `spawn` is a `TypeError` (a host
  function runs to completion and cannot be suspended). `dup` of a coroutine
  is a `TypeError`.
- **Host frames.** LSB raises `CannotSuspend` when a host frame lies between
  a `yield` and its coroutine. Host functions here never call back into
  bytecode, so there is never such a frame; `CannotSuspend` is raised exactly
  when no coroutine is running.
- **Traps** end the whole run; coroutines that were running then are
  `failed` (their frames are gone).

Switching is a copy: a suspending coroutine's frames and registers move into
the coroutine object, and resuming copies them back on top of the resumer
(LSB §10 question 7). Once a coroutine's buffers have grown to its deepest
stack, switching allocates nothing. A suspended stack's bytes count against
the memory budget; its frames count against the depth limit again when it
is resumed.

### Tasks and the built-in scheduler

`spawn` hands a new coroutine to the module's `spawn` hook (LSB hook 27).
Bind that hook to a module function (a scheduler written in bytecode), to
your own import, or to the VM's built-in scheduler with
[`Host::register_scheduler`](#hostregister_scheduler), which
[`Vm::run_async`](#vmrun_async) drives. The built-in scheduler is
single-threaded and deterministic: tasks run one at a time in FIFO order,
`await` of a task waits for its result (or its error), `await` of any other
value resumes at the back of the queue with that value, and the run ends when
the main task does.

### Memory and collection

Objects live in the VM's heap until unreachable. Collection is a mark and
sweep over the exact roots (reference-typed registers of every frame,
running closures, running coroutines, globals, the scheduler's tasks, and
the constant and name caches); a suspended coroutine is traced through its
frames' reference-typed registers. It runs only at safepoints (`safepoint`,
calls, coroutine instructions, and allocating instructions, LSB §5.9) and
only once as many bytes have been allocated as survived the previous
collection, so its total cost stays proportional to allocation. A
[`Value::Obj`](#obj) returned by a run stays valid until the next run on the
same VM; a handle to a collected object reads as `nil` (slots are
generation-checked and retired before a generation could wrap, so a stale
handle never aliases a new object).

**Close on drop** (LSB §5.13 rule 13). A suspended coroutine (`yielded` or
`awaiting`) that becomes unreachable is closed, its pending `finally` blocks
running as `coro_close` would run them, with the drop signal `nil`. This heap
is traced, not reference-counted, so the moment the last reference goes away
is not observable when it happens; instead the collection that finds the
coroutine unreachable keeps it (and what it reaches) alive and queues it.
Queued coroutines are closed one at a time, oldest (by creation) first, at
the next fuel charge point of a run that comes before its instruction does
anything (the instruction then runs as if just reached), or when the host
calls [`Vm::run_finalizers`](#vmrun_finalizers). The close's outcome is
discarded: its return value, an error escaping it (as CPython discards an
exception raised while a dropped generator closes), and a refusal (a
`yield`). Each coroutine is closed on drop at most once; one that refuses or
awaits is freed when next found unreachable. Created, returned, and failed
coroutines run no code and are freed. Collection timing is this VM's own, so
when a close runs is not something other tiers reproduce.

All of this is the tracing profile, the default. The reference-counting
profile frees objects at their last reference instead; see the next
section.

### Memory profiles

A VM runs under one of two memory profiles, chosen when it is made
([`Vm::with_memory`](#vmwith_memory), [`Memory`](#memory)): the language's
`[runtime] memory` key (DECISIONS D22). LSB format 2 has no module flag for
it, so the runner chooses (`"gc"` is [`Memory::Tracing`](#memory), `"rc"` is
[`Memory::Rc`](#memory)).

| | `Memory::Tracing` (default) | `Memory::Rc` |
|---|---|---|
| Freeing | mark and sweep, after enough allocation | at the last reference, before the next instruction |
| Garbage cycles | traced | trial deletion (Bacon and Rajan's synchronous collector, as PHP's) when 10,000 possible roots are buffered, fuel-charged |
| Dropped suspended coroutine closed | at the next fuel charge point after the collection that finds it | at the last reference, before the next instruction |
| [Destructors](#destructors) | after the collection that finds the object | at the last reference, before the next instruction (PHP's timing) |
| Copying contents holding a reference only they hold | the copy shares the reference (LSB) | the copy holds the value (PHP) |
| `dsep_*` and copy-on-write decisions | the `cow`/`aliased` bits | the same bits (counts decide only whether a copy is physical) |

Everything else is identical: results, errors and their locations, globals,
fuel (outside the RC cycle collector's charges), and limits. The
differential tests run the same random programs under both and compare them
with the reference interpreters wherever none of the rows above applied.

**How RC counts.** Every holder of a reference word counts once: a
reference-typed register of a live or suspended frame, a frame's closure, a
reference-typed slot of an object (array and map storage counts its words
once however many containers share it), a global, a constant cache, a
running or queued coroutine, the scheduler, and a value handed to the host
(a run's result, an uncaught error, `new_str`'s string: held until the next
run or [`collect_garbage`](#heap-and-fuel-statistics)). A count reaching zero
frees nothing at once: the object waits on a list the VM settles before the
next instruction, so no word an instruction holds can go stale while it
runs, and freeing is iterative whatever the depth of what dies (a
million-node chain is freed in one go without recursion). Objects die in
PHP's order: a function's variables in declaration order when it returns, an
array's elements first to last, an object before what it holds. Counts are
kept in place over the existing generation-checked slots; there is no raw
pointer and no `unsafe`. A count that would pass `u32::MAX` sticks there
(the object is never freed again) and the run ends with the `OutOfMemory`
trap at the next instruction boundary.

**Cycles.** A garbage cycle keeps its counts above zero, so every object
whose count drops to a value above zero is buffered as a possible root. When
10,000 are buffered (at an allocating instruction), or when the budget is
tight, the collector marks gray what they reach, subtracting internal
references, restores what is still held from outside, and frees the rest.
Shared array and map storage is a node of its own, so cycles through `dup`ed
containers are found. A collection that frees fewer than 100 objects raises
the threshold by 10,000 (up to 1,000,000), as PHP does, so a program whose
buffered roots are all live cannot make every collection wasted. Every phase
uses work lists (a million-node ring is collected without recursion). Objects
in a garbage cycle that need code (a destructor, a suspended coroutine to
close) are kept, with what they reach, until that code has run; the next
collection frees them if they are still garbage.
[`collect_garbage`](#heap-and-fuel-statistics) runs a full collection.

**Memory accounting.** Freeing refunds an object's bytes at once (storage
when its last holder goes), so a loop that makes and drops objects stays
inside a small budget without collections. A `dup` is charged its share of
storage it does not own, so between recounts the estimate can only be high,
never low; recounts are rationed like tracing collections (after as much was
charged as was held at the last one).

### Destructors

[`Vm::set_destructor`](#vmset_destructor) names the destructor method of
struct types (PHP's `__destruct`): every struct type whose methods, its own
or inherited, include one of that name has it called with the instance as
its one argument when the instance dies. LSB format 2 has no destructor
flag; the method is named by the language's convention.

- Under [`Memory::Rc`](#memory): at the last reference, before the next
  instruction, as PHP calls `__destruct` (reassignment, `unset`, scope exit,
  an array element overwritten, the array holding it destroyed); for an
  object in a garbage cycle, when the cycle collector finds it. Objects that
  die together are destructed one after another, each followed by what its
  freeing releases (never nested inside each other); what a destructor
  itself drops is destructed inside it, as in PHP.
- Under [`Memory::Tracing`](#memory): the collection that finds the object
  unreachable keeps it and queues it after the dropped coroutines; it runs
  at the next fuel charge point of a run, or on
  [`run_finalizers`](#vmrun_finalizers).

In both, a destructor runs at most once per object; its result is
discarded; an error escaping it is discarded (as a drop-close's is; PHP
would raise it where the object died, see the release notes); a trap ends
the run; it is charged fuel like any code; one the depth or register limits
cannot hold is skipped (the object is freed all the same). An object a
destructor stores somewhere lives on and is not destructed again. Objects
still reachable when the VM is dropped are not destructed: PHP's shutdown
pass is the host's call (let go, [`collect_garbage`](#heap-and-fuel-statistics),
[`run_finalizers`](#vmrun_finalizers)).

### Instruction coverage

All 200 instructions of LSB format 2 execute.

| LSB group | Opcodes | 2.0.0-alpha.4 |
|---|---|---|
| Moves, constants, globals (§5.1) | `0x00`-`0x0A` | implemented; `load_import` of any import, parameter list included |
| Integer arithmetic and comparison (§5.2) | `0x10`-`0x27` | implemented, every policy, `shift = saturate`, `ipow` |
| Float arithmetic and comparison (§5.3) | `0x30`-`0x48` | implemented, `fpow` by `ls_pow` |
| Booleans, chars, identity (§5.4) | `0x50`-`0x5A` | implemented |
| Conversions (§5.5) | `0x60`-`0x6E` | implemented (`f32_to_int`/`f64_to_int` take `FloatConv`) |
| Dynamic arithmetic and comparison (§5.6) | `0x70`-`0x85`, `0x94`-`0x96` | implemented, `promote` included, `dpow`, `dabs`, `dnot`/`dbit_not` |
| Dynamic values, properties, calls (§5.7) | `0x86`-`0x93`, `0x97`-`0x9B` | implemented: separation, `dcall_shape`, parameter-list binding, `dparam_ref*` |
| Control flow, calls, exceptions (§5.9) | `0xA0`-`0xAF`, `0x9C` | implemented, `raise` and `err_payload` |
| Closures, cells, references (§5.10, §5.17) | `0xB0`-`0xBB` | implemented, transparent reference slots |
| Typed heap objects (§5.10) | `0xC0`-`0xD4` | implemented |
| Strings (§5.11) | `0xE0`-`0xE6` | implemented |
| Coroutines (§5.13) | `0xF0`-`0xFC` | implemented: rules 1-14, PHP generator keys, close, iteration, close on drop |
| Hooks (§5.8) | codes 0-30 | implemented, `pow`, `abs`, `call_shape` included |

## `Program`

```rust,ignore
pub struct Program { /* private */ }
```

A loaded module: checked, bound to its host functions, and ready to run.
Immutable; `Send` and `Sync`, so one program can serve VMs on many threads.

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::ModuleBuilder;

let mut m = ModuleBuilder::new();
let mut f = m.function("f", &[], &[]);
f.ret_void();
let id = m.add_function(f).unwrap();
let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
std::thread::scope(|s| {
    for _ in 0..4 {
        let program = &program;
        s.spawn(move || assert!(Vm::new(program).run(id, &[]).is_ok()));
    }
});
```

### `Program::load`

```rust,ignore
pub fn load(module: Module, host: &Host) -> Result<Program, LoadError>
```

Checks `module` (see [What loading checks](#what-loading-checks)) and binds
each import to the function `host` registers under the import's module and
name. Linear in the module's size.

**Errors.** A [`LoadError`](#loaderror) naming the first problem, with the
function and pc where it applies.

```rust
use bvm_lang::{Host, LoadErrorKind, Program};
use bytecode_lang::{Inst, ModuleBuilder};

let mut m = ModuleBuilder::new();
let mut f = m.function("f", &[], &[]);
f.emit(Inst::Nop {}); // falls off the end
m.add_function(f).unwrap();
let err = Program::load(m.finish().unwrap(), &Host::new()).unwrap_err();
assert_eq!(err.kind(), &LoadErrorKind::FallsThrough);
assert_eq!(err.to_string(), "f0 @0: last instruction falls through");
```

### `Program::decode`

```rust,ignore
pub fn decode(bytes: &[u8], host: &Host) -> Result<Program, LoadError>
```

Decodes LSB bytes with bytecode-lang's default budgets, then loads.

**Errors.** [`LoadErrorKind::Decode`](#loaderrorkind) for malformed bytes;
otherwise as [`load`](#programload).

```rust
use bvm_lang::{Host, LoadErrorKind, Program};
use bytecode_lang::ModuleBuilder;

let bytes = bytecode_lang::encode(&ModuleBuilder::new().finish().unwrap());
assert!(Program::decode(&bytes, &Host::new()).is_ok());
let err = Program::decode(&bytes[..10], &Host::new()).unwrap_err();
assert!(matches!(err.kind(), LoadErrorKind::Decode(_)));
```

### `Program::module`

```rust,ignore
pub fn module(&self) -> &Module
```

The module the program was loaded from.

```rust
use bvm_lang::{Host, Program};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
assert!(p.module().functions().is_empty());
```

### `Program::export`

```rust,ignore
pub fn export(&self, name: &str) -> Option<FuncId>
```

The function exported under `name` (globals and types exported under that
name are not functions and give `None`).

```rust
use bvm_lang::{Host, Program};
use bytecode_lang::{ExportItem, ModuleBuilder};

let mut m = ModuleBuilder::new();
let mut f = m.function("main", &[], &[]);
f.ret_void();
let id = m.add_function(f).unwrap();
m.export("main", ExportItem::Func(id));
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
assert_eq!(p.export("main"), Some(id));
assert_eq!(p.export("other"), None);
```

### `Program::location`

```rust,ignore
pub fn location(&self, func: FuncId, pc: u32) -> Option<Location<'_>>
```

The source location of an instruction, from the module's line table: the row
with the greatest pc at or below `pc`. Pairs with
[`VmError::location`](#vmerror).

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::{Inst, ModuleBuilder};

let mut m = ModuleBuilder::new();
let file = m.string("app.mox");
let mut f = m.function("main", &[], &[]);
f.set_location(file, 12, 5);
f.emit(Inst::Unreachable {});
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let err = Vm::new(&p).run(id, &[]).unwrap_err();
let (func, pc) = err.location().unwrap();
assert_eq!(p.location(func, pc).unwrap().to_string(), "app.mox:12:5");
```

## `Vm`

```rust,ignore
pub struct Vm<'p> { /* private */ }
```

An instance of a [`Program`](#program): its heap, globals, default limits,
memory profile, and pooled register stack. The first run initialises the
globals from their initialisers and runs the module's start function (if
either fails, the next run tries again); later runs see the globals and heap
objects earlier runs left. A `Vm` is `Send`.

### `Vm::new`

```rust,ignore
pub fn new(program: &'p Program) -> Vm<'p>
```

An instance with [`Limits::new`](#limits): finite fuel and memory.

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let vm = Vm::new(&p);
assert_eq!(vm.limits(), bvm_lang::Limits::new());
assert!(vm.limits().fuel() < u64::MAX); // the defaults are finite
```

For code you do not control, prefer
`Vm::with_limits(&program, Limits::untrusted())`.

### `Vm::with_limits`

```rust,ignore
pub fn with_limits(program: &'p Program, limits: Limits) -> Vm<'p>
pub fn limits(&self) -> Limits
pub fn set_limits(&mut self, limits: Limits)
pub fn program(&self) -> &'p Program
```

An instance whose runs use `limits` by default; `limits`/`set_limits` read and
replace them; `program` returns the program.

```rust
use bvm_lang::{Host, Limits, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::with_limits(&p, Limits::new().with_fuel(10_000));
assert_eq!(vm.limits().fuel(), 10_000);
vm.set_limits(Limits::trusted());
assert_eq!(vm.limits().fuel(), u64::MAX);
```

### `Vm::with_memory`

```rust,ignore
pub fn with_memory(program: &'p Program, memory: Memory) -> Vm<'p>
pub fn memory(&self) -> Memory
```

An instance under a [memory profile](#memory-profiles), with
[`Limits::new`](#limits) (change them with `set_limits`); `memory` reads
the profile, which is fixed for the VM's life. `Vm::new` and
`Vm::with_limits` use [`Memory::Tracing`](#memory).

```rust
use bvm_lang::{Host, Memory, Program, Vm};
use bytecode_lang::{Const, Inst, ModuleBuilder, ValType};

// t = s . s; t = nil -- under RC the concatenation dies at once.
let mut m = ModuleBuilder::new();
let k = m.constant(Const::Bytes(b"temporary".to_vec()));
let mut f = m.function("f", &[], &[]);
let (s, t) = (f.reg(ValType::Dyn), f.reg(ValType::Dyn));
f.emit(Inst::DLoadConst { dst: s, k });
f.emit(Inst::DConcat { dst: t, lhs: s, rhs: s });
f.emit(Inst::LoadNil { dst: t });
f.ret_void();
let f = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();

let mut rc = Vm::with_memory(&p, Memory::Rc);
rc.run(f, &[]).unwrap();
assert_eq!(rc.heap_objects(), 1); // the constant's string (its cache holds it)
let mut gc = Vm::new(&p);
gc.run(f, &[]).unwrap();
assert_eq!(gc.heap_objects(), 2); // the concatenation waits for a collection
```

### `Vm::set_destructor`

```rust,ignore
pub fn set_destructor(&mut self, name: &str) -> Result<usize, VmError>
```

Names the destructor method of struct types (see
[Destructors](#destructors)) and returns how many struct types got one
(their own method of that name, or an inherited one). A name no method has
(the empty name, say) clears every destructor.

**Errors.** [`VmError::BadDestructor`](#vmerror) when a method the name finds
cannot take the instance as its one argument: it has a different number of
parameters, its parameter is neither `dyn` nor a `ref` to the struct or an
ancestor, or it has captures. Nothing changes then.

```rust
use bvm_lang::{Host, Memory, Program, Value, Vm};
use bytecode_lang::{Inst, IntOp, IntTy, Method, ModuleBuilder, StructDef, TypeDef, ValType};

let mut m = ModuleBuilder::new();
let (name, dtor) = (m.string("Res"), m.string("__destruct"));
let res = m.reserve_type();
let log = m.global("log", ValType::I64, true, None);
// __destruct($this) { log = log * 10 + 1 }
let mut d = m.function("Res::__destruct", &[ValType::Dyn], &[]);
let (v, ten, one) = (d.reg(ValType::I64), d.reg(ValType::I64), d.reg(ValType::I64));
let op = IntOp::new(IntTy::I64);
d.emit(Inst::GetGlobal { dst: v, global: log });
d.emit(Inst::LoadInt { dst: ten, val: 10, ty: IntTy::I64 });
d.emit(Inst::LoadInt { dst: one, val: 1, ty: IntTy::I64 });
d.emit(Inst::IMul { dst: v, lhs: v, rhs: ten, op });
d.emit(Inst::IAdd { dst: v, lhs: v, rhs: one, op });
d.emit(Inst::SetGlobal { global: log, src: v });
d.ret_void();
let d = m.add_function(d).unwrap();
m.define_type(res, TypeDef::Struct(StructDef {
    name,
    methods: vec![Method { name: dtor, func: d }],
    ..Default::default()
}));
// main() { $o = new Res; $o = null; log = log * 10 + 9; return log }
let mut f = m.function("main", &[], &[ValType::I64]);
let (o, r, nine, ten) = (f.reg(ValType::Dyn), f.reg(ValType::I64), f.reg(ValType::I64), f.reg(ValType::I64));
let ty = f.type_ref(res);
f.emit(Inst::NewStruct { dst: o, ty });
f.emit(Inst::LoadNil { dst: o });
f.emit(Inst::GetGlobal { dst: r, global: log });
f.emit(Inst::LoadInt { dst: ten, val: 10, ty: IntTy::I64 });
f.emit(Inst::LoadInt { dst: nine, val: 9, ty: IntTy::I64 });
f.emit(Inst::IMul { dst: r, lhs: r, rhs: ten, op });
f.emit(Inst::IAdd { dst: r, lhs: r, rhs: nine, op });
f.emit(Inst::SetGlobal { global: log, src: r });
f.ret(r);
let main = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();

// PHP's order: the destructor at `$o = null`, then 9.
let mut rc = Vm::with_memory(&p, Memory::Rc);
assert_eq!(rc.set_destructor("__destruct"), Ok(1));
assert_eq!(rc.run(main, &[]), Ok(Value::Int(19)));
// Tracing: only after a collection finds the object.
let mut gc = Vm::new(&p);
gc.set_destructor("__destruct").unwrap();
assert_eq!(gc.run(main, &[]), Ok(Value::Int(9)));
gc.collect_garbage();
assert_eq!(gc.run_finalizers(), Ok(1));
assert_eq!(gc.global(log), Some(Value::Int(91)));
```

### `Vm::set_hash_seed`

```rust,ignore
pub fn set_hash_seed(&mut self, seed: u128) -> bool
```

Sets the key map keys are hashed with. With the `std` feature every VM gets
a random key; without it the key is a fixed constant, so a module that knows
it can pick colliding string keys and make building a map quadratic. A
`no_std` host that runs untrusted modules should pass its own random value
before the first run. Returns `false`, changing nothing, once the heap holds
a map.

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
assert!(vm.set_hash_seed(0x5eed));
```

### `Vm::run`

```rust,ignore
pub fn run(&mut self, func: FuncId, args: &[Value]) -> Result<Value, VmError>
```

Calls `func` with `args` under the VM's limits. A void function returns
`Value::Nil`.

**Errors.** [`VmError`](#vmerror): `NoSuchFunction`, `NeedsClosure` (the
function has captures), `ArgumentCount`, `ArgumentType`, `GlobalInit`, or the
run's own outcome (`Raised`, `Thrown`, `Trap`).

```rust
use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{Inst, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let mut f = m.function("not", &[ValType::Bool], &[ValType::Bool]);
let r = f.reg(ValType::Bool);
f.emit(Inst::BNot { dst: r, src: f.param(0) });
f.ret(r);
let not = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
assert_eq!(vm.run(not, &[Value::Bool(true)]), Ok(Value::Bool(false)));
assert_eq!(vm.run(not, &[Value::Int(1)]), Err(VmError::ArgumentType { index: 0 }));
assert_eq!(vm.run(not, &[]), Err(VmError::ArgumentCount { expected: 1, found: 0 }));
```

### `Vm::run_with`

```rust,ignore
pub fn run_with(&mut self, func: FuncId, args: &[Value], limits: Limits) -> Result<Value, VmError>
```

As [`run`](#vmrun) under `limits` for this run only. Fuel starts at the
budget each run; the memory budget applies to the whole heap (including what
earlier runs left).

```rust
use bvm_lang::{Host, Limits, Program, Value, Vm, VmError};
use bytecode_lang::{ErrorKind, Inst, ModuleBuilder};

let mut m = ModuleBuilder::new();
let mut f = m.function("f", &[], &[]);
let me = f.id();
let w = f.reg(bytecode_lang::ValType::Dyn);
f.emit(Inst::Call { dst: w, func: me, argc: 0 }); // unbounded recursion
f.ret_void();
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let err = Vm::new(&p).run_with(id, &[], Limits::new().with_depth(100)).unwrap_err();
assert_eq!(err, VmError::Raised { kind: ErrorKind::StackOverflow, payload: Value::Nil, func: id, pc: 0 });
```

### `Vm::run_export`

```rust,ignore
pub fn run_export(&mut self, name: &str, args: &[Value]) -> Result<Value, VmError>
```

Runs the function exported under `name`; `VmError::NoSuchExport` if none.
See [Quick start](#quick-start).

### `Vm::run_async`

```rust,ignore
pub fn run_async(&mut self, func: FuncId, args: &[Value]) -> Result<Value, VmError>
```

Runs `func` as the main task of the built-in scheduler (Python's
`asyncio.run`) and returns its result. `func` becomes a coroutine, so it may
`await`; tasks run one at a time, FIFO, until the main task finishes:

- `spawn` (its hook bound to the [`Host::register_scheduler`](#hostregister_scheduler)
  import) queues the new coroutine as a task and returns it as the handle;
- `await` of a task, or of any coroutine (which becomes a task), waits until
  it finishes and resumes with its return value, or raises its error at the
  `await`; a finished one resumes at once, at the back of the queue;
- `await` of any other value resumes, at the back of the queue, with that
  value; a task that `yield`s resumes with `nil`;
- the main task failing ends the run with its error.

Tasks unfinished when the main task finishes are dropped (and closed when
collected, like any dropped coroutine); tasks spawned by a plain
[`run`](#vmrun) wait for the next `run_async`. One fuel budget covers the
whole run.

**Errors.** As [`run`](#vmrun), plus `VmError::Deadlock { waiting }` when no
task can run and the main task has not finished.

```rust
use bvm_lang::{Host, Program, Vm, VmError};
use bytecode_lang::{Inst, ModuleBuilder, ValType};

let d = ValType::Dyn;
let mut m = ModuleBuilder::new();
// main() { me = current coroutine; return await me }: waits for itself.
let mut f = m.function("main", &[], &[d]);
let (me, r) = (f.reg(d), f.reg(d));
f.emit(Inst::CoroCurrent { dst: me });
f.emit(Inst::Await { dst: r, src: me });
f.ret(r);
let main = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
assert_eq!(Vm::new(&p).run_async(main, &[]), Err(VmError::Deadlock { waiting: 1 }));
```

### `Vm::run_finalizers`

```rust,ignore
pub fn run_finalizers(&mut self) -> Result<usize, VmError>
pub fn pending_finalizers(&self) -> usize
```

Runs now, under the VM's limits, the finalizations queued for later: the
dropped suspended coroutines tracing collections found (see
[Memory and collection](#memory-and-collection)), the objects whose
[destructor](#destructors) is due, and under [`Memory::Rc`](#memory) what
died outside a run (or when a run ended abnormally) and the finalizable
garbage a cycle collection found. Returns how many it started (the
finalizations they cause included); `pending_finalizers` counts the queue.
Call [`collect_garbage`](#heap-and-fuel-statistics) first to queue everything
dropped (at program exit, say). Under RC the queue is also drained at the
next run's first instruction.

**Errors.** A trap inside a closing `finally` (`OutOfFuel`, `OutOfMemory`,
`Unreachable`) stops it; the rest stay queued. Errors the `finally` blocks
raise are discarded.

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::{Inst, ModuleBuilder, ValType};

let d = ValType::Dyn;
let mut m = ModuleBuilder::new();
let count = m.global("count", d, true, None);
// gen() { try { yield nil } finally { count = 1 } }
let mut g = m.function("gen", &[], &[]);
let (s, e, one) = (g.reg(d), g.reg(d), g.reg(d));
let (start, end, h) = (g.label(), g.label(), g.label());
g.bind(start);
g.emit(Inst::Yield { dst: s, src: s });
g.bind(end);
g.ret_void();
g.bind(h);
g.emit(Inst::DLoadInt { dst: one, val: 1 });
g.emit(Inst::SetGlobal { global: count, src: one });
g.emit(Inst::Throw { src: e });
g.try_region(start, end, h, e);
let gen_id = m.add_function(g).unwrap();
// drop() { c = gen(); resume c } -- the suspended coroutine is dropped.
let mut f = m.function("drop", &[], &[]);
let (c, r) = (f.reg(d), f.reg(d));
f.emit(Inst::CoroNew { dst: c, func: gen_id, argc: 0 });
f.emit(Inst::Resume { dst: r, coro: c, src: r });
f.ret_void();
let drop_id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();

let mut vm = Vm::new(&p);
vm.run(drop_id, &[]).unwrap();
vm.collect_garbage();
assert_eq!(vm.pending_finalizers(), 1);
assert_eq!(vm.run_finalizers(), Ok(1));
assert_eq!(vm.global(count), Some(bvm_lang::Value::Int(1))); // its finally ran
```

### Inspecting values

```rust,ignore
pub fn kind(&self, v: Value) -> Kind
pub fn str_bytes(&self, v: Value) -> Option<&[u8]>
pub fn elements(&self, v: Value) -> Option<Vec<Value>>
pub fn entries(&self, v: Value) -> Option<Vec<(Value, Value)>>
pub fn field(&self, v: Value, index: usize) -> Option<Value>
pub fn error_code(&self, v: Value) -> Option<u32>
pub fn error_payload(&self, v: Value) -> Option<Value>
pub fn ref_value(&self, v: Value) -> Option<Value>
pub fn coro_state(&self, v: Value) -> Option<CoroState>
pub fn global(&self, id: GlobalId) -> Option<Value>
pub fn new_str(&mut self, bytes: &[u8]) -> Result<Value, VmError>
```

Read what runs return: a value's dynamic kind, a string's bytes, an array's
elements, a map's entries in insertion order, a struct's field (each of
those three reading a reference slot as its value), a runtime error value's
code and payload, a reference's value, a coroutine's state, a global's
current value; `new_str` allocates a string to pass to a run (the
`OutOfMemory` trap if the budget is spent).

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{Const, Inst, Kind, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let k = m.constant(Const::Bytes(b"k".to_vec()));
let v = m.constant(Const::Int(5));
let map = m.constant(Const::Map(vec![(k, v)]));
let mut f = m.function("f", &[], &[ValType::Dyn]);
let r = f.reg(ValType::Dyn);
f.emit(Inst::DLoadConst { dst: r, k: map });
f.ret(r);
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
let out = vm.run(id, &[]).unwrap();
assert_eq!(vm.kind(out), Kind::Map);
let entries = vm.entries(out).unwrap();
assert_eq!(vm.str_bytes(entries[0].0), Some(&b"k"[..]));
assert_eq!(entries[0].1, Value::Int(5));
```

### Heap and fuel statistics

```rust,ignore
pub fn fuel_used(&self) -> u64
pub fn heap_bytes(&self) -> usize
pub fn heap_objects(&self) -> usize
pub fn collections(&self) -> u64
pub fn collect_garbage(&mut self)
```

Fuel the last run (or `run_async`, or `run_finalizers`) consumed; bytes
charged to the memory budget; live objects (exact after a collection, and
always exact under [`Memory::Rc`](#memory)); collections so far (cycle
collections under RC); and an explicit collection keeping only what globals
and caches reach (suspended coroutines it finds dropped are queued to be
closed, objects with a destructor queued for it, not freed). Under RC,
`collect_garbage` lets go of the values earlier runs handed out and runs the
cycle collector over every possible root; everything else was freed at its
last reference already.

```rust
use bvm_lang::{Host, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
let s = vm.new_str(b"temporary").unwrap();
assert_eq!(vm.heap_objects(), 1);
vm.collect_garbage();
assert_eq!((vm.heap_objects(), vm.collections()), (0, 1));
assert_eq!(vm.kind(s), bytecode_lang::Kind::Nil); // the stale handle reads as nil
```

```rust,ignore
pub fn memory_stats(&self) -> MemoryStats
```

What the memory manager has done: see [`MemoryStats`](#memorystats).

## `Limits`

```rust,ignore
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits { /* private */ }
impl Limits {
    pub const fn new() -> Limits;
    pub const fn untrusted() -> Limits;
    pub const fn trusted() -> Limits;
    pub const fn with_fuel(self, fuel: u64) -> Limits;
    pub const fn with_memory(self, bytes: usize) -> Limits;
    pub const fn with_depth(self, frames: usize) -> Limits;   // at least 1
    pub const fn with_stack(self, slots: usize) -> Limits;
    pub const fn with_data_fuel(self, on: bool) -> Limits;
    pub const fn fuel(&self) -> u64;
    pub const fn memory(&self) -> usize;
    pub const fn depth(&self) -> usize;
    pub const fn stack(&self) -> usize;
    pub const fn data_fuel(&self) -> bool;
}
```

The budgets of a run (see [Fuel and the other budgets](#fuel-and-the-other-budgets)).
Three presets:

| preset | fuel | memory | depth | register slots |
|---|---|---|---|---|
| `untrusted` | 10,000,000 | 64 MiB | 1,000 | 1 Mi (8 MiB) |
| `new`, `Default` | 100,000,000 | 256 MiB | 10,000 | 4 Mi (32 MiB) |
| `trusted` | unlimited | 1 GiB | 10,000 | 4 Mi (32 MiB) |

The guarantees for untrusted modules hold under finite limits, which both
`new` and `untrusted` are. Start from `untrusted` for code you do not
control and size the fuel to the time a run may take; `trusted` has no fuel
limit, so a module that loops forever runs forever under it.
`with_data_fuel(false)` turns off the data-work charge, leaving exactly
LSB §5.14's charge points (the count every tier agrees on); with it off the
work one unit pays for is bounded only by the memory budget, so keep it on
for code you do not control. The memory
budget is only as hard as the host can honour, so keep it below the memory
actually available.

```rust
use bvm_lang::Limits;

assert_eq!(Limits::default(), Limits::new());
assert_eq!(Limits::new().fuel(), 100_000_000);
assert_eq!(Limits::trusted().fuel(), u64::MAX);
let l = Limits::untrusted().with_fuel(1_000).with_memory(64 << 20).with_depth(256).with_stack(1 << 20);
assert_eq!((l.fuel(), l.memory(), l.depth(), l.stack()), (1_000, 64 << 20, 256, 1 << 20));
```

## `Memory`

```rust,ignore
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Memory {
    #[default]
    Tracing,
    Rc,
}
```

A VM's [memory profile](#memory-profiles): `Tracing` (the default, a
mark-and-sweep collector; `[runtime] memory = "gc"`, Mercury and others) or
`Rc` (reference counting plus a cycle collector, PHP's model; `[runtime]
memory = "rc"`, Mox).

```rust
use bvm_lang::{Host, Memory, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
assert_eq!(Memory::default(), Memory::Tracing);
assert_eq!(Vm::with_memory(&p, Memory::Rc).memory(), Memory::Rc);
```

## `MemoryStats`

```rust,ignore
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoryStats {
    pub collections: u64,
    pub freed: u64,
    pub cycle_freed: u64,
    pub finalized: u64,
    pub unbound: u64,
    pub live: usize,
    pub bytes: usize,
}
```

Counters from [`Vm::memory_stats`](#heap-and-fuel-statistics): collections
(tracing collections, or cycle collections under RC), objects freed by every
means, those of them the cycle collector freed, finalizations started
(destructors run and dropped coroutines closed), references unbound by a
copy (RC), live objects, and bytes charged to the budget.

```rust
use bvm_lang::{Host, Memory, Program, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::with_memory(&p, Memory::Rc);
vm.new_str(b"held for the host").unwrap();
assert_eq!(vm.memory_stats().live, 1);
vm.collect_garbage(); // lets go of it
let stats = vm.memory_stats();
assert_eq!((stats.live, stats.freed, stats.collections), (0, 1, 1));
```

## `Value`

```rust,ignore
#[non_exhaustive]
pub enum Value { Nil, Bool(bool), Int(i64), UInt(u64), F32(f32), Float(f64), Char(char), Obj(Obj) }
```

A register's contents at the API boundary (see
[Registers and values](#registers-and-values)). `as_int` (also unsigned values
that fit), `as_float` (also `F32`), `as_bool`, and `is_nil` read it;
`Display` prints scalars and `<obj>`; `From` converts `bool`, `i64`, `u64`,
`f64`, and `char`.

```rust
use bvm_lang::Value;

assert_eq!(Value::from(3i64).as_int(), Some(3));
assert_eq!(Value::UInt(9).as_int(), Some(9));
assert_eq!(Value::F32(0.5).as_float(), Some(0.5));
assert_eq!(Value::Bool(true).to_string(), "true");
assert!(Value::default().is_nil());
```

## `Obj`

```rust,ignore
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Obj(/* private */);
```

A handle to a heap object of one [`Vm`](#vm); equality is identity. Valid
until the next run on that VM, or until collected (then it reads as `nil`).

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::ModuleBuilder;

let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
let mut vm = Vm::new(&p);
let (a, b) = (vm.new_str(b"x").unwrap(), vm.new_str(b"x").unwrap());
assert!(matches!((a, b), (Value::Obj(x), Value::Obj(y)) if x != y));
```

## `Host`

```rust,ignore
#[derive(Clone, Default)]
pub struct Host { /* private */ }
impl Host {
    pub fn new() -> Host;
    pub fn register<F>(&mut self, module: &str, name: &str, f: F) -> &mut Host
    where F: Fn(&mut HostCtx<'_>, &[Value]) -> Result<Value, HostError> + Send + Sync + 'static;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

Host functions by import module and name. A host function receives its
arguments converted from the import's declared parameter types; its result is
converted to the declared result type (`TypeError` at the call if it does not
fit; ignored for a void import). It runs to completion without re-entering the
VM, so no collection happens during it, and no host frame can ever sit
between a coroutine and its `yield`.

```rust
use bvm_lang::{Host, HostError, Program, Value, Vm};
use bytecode_lang::{ErrorKind, Inst, ModuleBuilder, ValType};

let mut host = Host::new();
host.register("math", "isqrt", |_, args| match args {
    [Value::Int(n)] if *n >= 0 => Ok(Value::Int(n.isqrt())),
    _ => Err(HostError::Raise(ErrorKind::TypeError)),
});
let mut m = ModuleBuilder::new();
let sig = m.func_type(&[ValType::I64], &[ValType::I64]);
let isqrt = m.import("math", "isqrt", sig);
let mut f = m.function("f", &[ValType::I64], &[ValType::I64]);
let w = f.regs(&[ValType::I64, ValType::I64]);
f.mov(bytecode_lang::Reg(w.0 + 1), f.param(0));
f.emit(Inst::CallImport { dst: w, import: isqrt, argc: 1 });
f.ret(w);
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &host).unwrap();
assert_eq!(Vm::new(&p).run(id, &[Value::Int(99)]), Ok(Value::Int(9)));
```

### `Host::register_scheduler`

```rust,ignore
pub fn register_scheduler(&mut self, module: &str, name: &str) -> &mut Host
```

Registers the VM's built-in scheduler as the import `module`.`name`, which
must have the signature `(dyn) -> dyn` (`LoadErrorKind::BadSignature`
otherwise). Bind it as the `spawn` hook; [`Vm::run_async`](#vmrun_async)
drives the tasks it receives.

```rust
use bvm_lang::{Host, Program, Value, Vm};
use bytecode_lang::{Callee, Hook, Inst, ModuleBuilder, Policy, Reg, ValType};

let d = ValType::Dyn;
let mut host = Host::new();
host.register_scheduler("ls.async", "spawn");
let mut m = ModuleBuilder::new();
let sig = m.func_type(&[d], &[d]);
let spawn = m.import("ls.async", "spawn", sig);
m.hook(Hook::Spawn, Callee::Import(spawn));
// async fn double(x) { x + x }
let mut t = m.function("double", &[d], &[d]);
let r = t.reg(d);
t.emit(Inst::DAdd { dst: r, lhs: t.param(0), rhs: t.param(0), pol: Policy::new() });
t.ret(r);
let double = m.add_function(t).unwrap();
// async fn main() { await spawn double(21) }
let mut f = m.function("main", &[], &[d]);
let (fv, out) = (f.reg(d), f.reg(d));
let w = f.regs(&[d, d]);
f.emit(Inst::MakeClosure { dst: fv, func: double });
f.emit(Inst::DLoadInt { dst: Reg(w.0 + 1), val: 21 });
f.emit(Inst::Spawn { dst: w, callee: fv, argc: 1 });
f.emit(Inst::Await { dst: out, src: w });
f.ret(out);
let main = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &host).unwrap();
assert_eq!(Vm::new(&p).run_async(main, &[]), Ok(Value::Int(42)));
```

## `HostCtx`

```rust,ignore
pub struct HostCtx<'a> { /* private */ }
impl HostCtx<'_> {
    pub fn str_bytes(&self, v: Value) -> Option<&[u8]>;
    pub fn new_str(&mut self, bytes: &[u8]) -> Result<Value, HostError>;
    pub fn kind(&self, v: Value) -> Kind;
    pub fn error_code(&self, v: Value) -> Option<u32>;
    pub fn ref_get(&self, v: Value) -> Option<Value>;
    pub fn ref_set(&mut self, r: Value, value: Value) -> Result<(), HostError>;
    pub fn elements(&self, v: Value) -> Option<Vec<Value>>;
    pub fn entries(&self, v: Value) -> Option<Vec<(Value, Value)>>;
    pub fn new_array(&mut self, items: &[Value]) -> Result<Value, HostError>;
    pub fn new_map(&mut self, entries: &[(Value, Value)]) -> Result<Value, HostError>;
}
```

What a host function can do with the VM while it runs. `new_str`,
`new_array`, `new_map`, and `ref_set` fail with
`HostError::Raise(ErrorKind::OutOfMemory)` (a trap) when the budget is
spent. `ref_get`/`ref_set` read and write a reference (what a by-reference
parameter of an import's parameter list receives; `ref_set` of a
non-reference is `TypeError`); `elements`/`entries` read an array or a map
(reference slots as their values; a rest parameter arrives as one);
`new_array`/`new_map` build `dyn` containers to return.

```rust
use bvm_lang::{Host, Value};

let mut host = Host::new();
host.register("str", "upper", |ctx, args| {
    let s = args.first().and_then(|v| ctx.str_bytes(*v)).unwrap_or(b"").to_ascii_uppercase();
    ctx.new_str(&s)
});
// PHP: function sum(&$total, ...$values), through an import parameter list:
// adds the rest array into the referenced total and returns the count.
host.register("php", "sum_into", |ctx, args| {
    let values = ctx.elements(args[1]).unwrap_or_default();
    let mut total = ctx.ref_get(args[0]).and_then(Value::as_int).unwrap_or(0);
    for v in &values {
        total += v.as_int().unwrap_or(0);
    }
    ctx.ref_set(args[0], Value::Int(total))?;
    Ok(Value::Int(values.len() as i64))
});
```

## `HostError`

```rust,ignore
#[non_exhaustive]
pub enum HostError { Raise(ErrorKind), Throw(Value) }
```

`Raise` raises a runtime error of that kind at the calling instruction
(catchable unless the kind is `OutOfMemory`, `OutOfFuel`, or `Unreachable`,
which trap); `Throw` throws a value, as `throw` would.

```rust
use bvm_lang::{HostError, Value};
use bytecode_lang::ErrorKind;

assert_eq!(HostError::Raise(ErrorKind::DivByZero).to_string(), "host raised E0002 DivByZero");
assert_eq!(HostError::Throw(Value::Nil).to_string(), "host threw nil");
```

## `VmError`

```rust,ignore
#[non_exhaustive]
pub enum VmError {
    Raised { kind: ErrorKind, payload: Value, func: FuncId, pc: u32 },
    Thrown { value: Value, func: FuncId, pc: u32 },
    Trap { kind: ErrorKind, func: FuncId, pc: u32 },
    Deadlock { waiting: usize },
    GlobalInit { global: GlobalId, kind: ErrorKind },
    NoSuchFunction(FuncId),
    NoSuchExport,
    NeedsClosure(FuncId),
    ArgumentCount { expected: usize, found: usize },
    ArgumentType { index: usize },
    BadDestructor(FuncId),
}
impl VmError {
    pub fn kind(&self) -> Option<ErrorKind>;
    pub fn code(&self) -> Option<u32>;
    pub fn location(&self) -> Option<(FuncId, u32)>;
}
```

How a run ended without a result. `Raised` is an uncaught runtime error (its
payload is `raise`'s operand, `nil` for an error an instruction raised by
itself; its location is where it was raised, even if a handler caught and
rethrew it);
`Thrown` an uncaught non-error value (its location is where the unwind that
ended the run began); `Trap` a trap; `Deadlock` a [`run_async`](#vmrun_async)
whose tasks all wait; `GlobalInit` a global initialiser that does not fit its
global; `BadDestructor` a method [`set_destructor`](#vmset_destructor) cannot
use; the rest are entry problems. `kind`, `code`, and `location` read the
common parts.

```rust
use bvm_lang::{Host, Program, Value, Vm, VmError};
use bytecode_lang::{Inst, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let mut f = m.function("f", &[ValType::Dyn], &[]);
f.emit(Inst::Throw { src: f.param(0) });
let id = m.add_function(f).unwrap();
let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
let err = Vm::new(&p).run(id, &[Value::Int(7)]).unwrap_err();
assert_eq!(err, VmError::Thrown { value: Value::Int(7), func: id, pc: 0 });
assert_eq!((err.kind(), err.location()), (None, Some((id, 0))));
```

## `LoadError`

```rust,ignore
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError { /* private */ }
impl LoadError {
    pub fn kind(&self) -> &LoadErrorKind;
    pub fn func(&self) -> Option<FuncId>;
    pub fn pc(&self) -> Option<u32>;
}
```

Why [`Program::load`](#programload) refused a module, and where. `Display`
prints `f<id> @<pc>: <reason>`.

## `LoadErrorKind`

```rust,ignore
#[non_exhaustive]
pub enum LoadErrorKind {
    Decode(DecodeError),
    UnresolvedImport { module: String, name: String },
    OutOfRange { what: &'static str, index: u32 },
    BadSignature, ParamMismatch, EmptyCode, FallsThrough,
    ArityMismatch { expected: u32, found: u32 },
    CalleeHasCaptures, PromoteNotDynamic,
    WrongTypeKind { expected: &'static str },
    BadModifier, InvalidHandler, CatchNotDyn, TailCallInTry,
    BadParent, InheritanceTooDeep, BadConstant,
    BadHook(Hook), BadStart,
    BadParamList, BadCallShape,
}
```

Each variant is one rule of [What loading checks](#what-loading-checks);
`OutOfRange::what` is one of `"register"`, `"constant"`, `"function"`,
`"import"`, `"global"`, `"table"`, `"name"`, `"type ref"`, `"capture"`,
`"branch target"`, `"table target"`, `"type"`, `"string"`.

```rust
use bvm_lang::{Host, LoadErrorKind, Program};
use bytecode_lang::{Callee, Hook, ModuleBuilder, ValType};

let mut m = ModuleBuilder::new();
let mut h = m.function("h", &[ValType::Dyn], &[ValType::Dyn]);
h.ret(h.param(0));
let id = m.add_function(h).unwrap();
m.hook(Hook::Eq, Callee::Func(id)); // `eq` takes two operands
let err = Program::load(m.finish().unwrap(), &Host::new()).unwrap_err();
assert_eq!(err.kind(), &LoadErrorKind::BadHook(Hook::Eq));
```

## `Location`

```rust,ignore
pub struct Location<'a> { pub file: &'a str, pub line: u32, pub column: u32 }
```

A line-table position; `Display` prints `file:line:column`. See
[`Program::location`](#programlocation).

## Constants

```rust,ignore
pub const MAX_INHERITANCE_DEPTH: usize = 256;
pub const MAX_CONST_DEPTH: u32 = 64;
```

The deepest struct inheritance chain and aggregate-constant nesting a module
may declare.

```rust
assert_eq!(bvm_lang::MAX_INHERITANCE_DEPTH, 256);
assert_eq!(bvm_lang::MAX_CONST_DEPTH, 64);
```

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `std` | yes | Float `sqrt`/`fma`/rounding from the standard library (hardware where available), and a random per-VM hashing key. Without it the crate is `no_std` + `alloc`, computes the same float results in software (property-tested equal), and uses a fixed key. |

## Stability

2.0.0-alpha.4 is a pre-release: names and signatures may change before 2.0.0,
each change recorded in the CHANGELOG. Instruction semantics follow LSB and
OPS and change only with them. See [`STABILITY.md`](./STABILITY.md).
