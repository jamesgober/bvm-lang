//! A reference interpreter for whole LSB modules, written independently of
//! the crate, for the differential property tests of calls, closures, heap
//! objects, exceptions (with `finally`), and coroutines.
//!
//! It shares nothing with the VM: values are a plain enum, the heap is a
//! `Vec` of enum objects that is never collected, frames own their
//! registers, and coroutines are *segmented*: each running coroutine keeps
//! its own frame stack in the resume chain (the VM instead copies frames
//! between one flat stack and the coroutine object). Semantics are read off
//! LSB §4.3, §5.9-§5.13 and the fuel rule of §5.14.
//!
//! Subset: moves and constants (`dyn`, strings, typed ints), globals, the
//! dynamic arithmetic and comparison instructions (numbers via the scalar
//! reference, `dpow`/`dabs`/shifts included), `dconcat`, `to_dyn` from
//! integers and `bool`, typed `i64` loop arithmetic, branches and `switch`,
//! `safepoint`, calls (direct, indirect, tail, and dynamic calls with
//! parameter-list binding: `dcall`, `dcall_shape` with named arguments and
//! spreads, `dparam_ref`/`dparam_ref_named`), closures and captures,
//! `throw`/`raise`/`err_code`/`err_payload`, handlers, arrays, maps, structs
//! (typed and dynamic access: `dget_index`/`dset_index`, `get_prop`/
//! `set_prop`), strings, `dup`, separation (`dsep_*`), PHP references
//! (`new_ref`, `cell_get`/`cell_set`, `dref_*`, `dbind_*`, `dunref_*`, and
//! transparent reference slots in every slot access), and every coroutine
//! instruction, with iteration over coroutines. No hooks are bound.
//!
//! **Copy-on-write.** Containers here copy their contents eagerly, so the
//! reference needs only LSB §5.16's two bits to decide what the VM decides:
//! `cow` (set on both sides by `dup`, cleared by the first write, which
//! marks the arrays and maps in the contents `aliased`) and `aliased` (which
//! elements `dsep_*` and `dref_*` replace by a `dup`). The bits live in side
//! sets keyed by object index.

#![allow(dead_code, clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::{HashMap, HashSet};

use bytecode_lang::{
    ArgKind, CallShape, Const, ConstId, CoroState, ErrorKind, FuncId, Inst, IntTy, Module,
    ParamKind, ParamList, Prim, ValType,
};

use super::reference::{Dv, dyn_abs, dyn_bin, dyn_cmp, int_bin, wrap};

/// A value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum V {
    Nil,
    Bool(bool),
    /// A `dyn` int.
    Int(i64),
    /// A `dyn` float.
    Float(f64),
    /// A heap object.
    Obj(usize),
    /// A typed integer register.
    I(i128),
}

#[derive(Clone, Debug)]
struct RMap {
    entries: Vec<(V, V)>,
    next: Option<i128>,
}

#[derive(Clone, Debug)]
struct Co {
    state: CoroState,
    closing: bool,
    signal: V,
    func: usize,
    closure: Option<usize>,
    args: Vec<V>,
    /// Suspended frames (empty while running or before starting).
    stack: Vec<Frame>,
    resume_dst: u16,
    key: V,
    /// PHP's generator key counter (rule 10): starts at -1.
    max_key: i64,
    result: V,
}

#[derive(Clone, Debug)]
enum O {
    Str(Vec<u8>),
    Arr(Vec<V>),
    Map(RMap),
    Struct(Vec<V>),
    Func {
        func: usize,
        caps: Vec<V>,
    },
    Err {
        kind: ErrorKind,
        func: u32,
        pc: u32,
        payload: V,
    },
    /// A PHP reference (LSB §5.17).
    Ref(V),
    Iter {
        coro: usize,
        done: bool,
        key: Option<V>,
    },
    Coro(Box<Co>),
}

/// Where a returning frame's result goes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ret {
    Entry,
    Write(u16),
    Dyn(u16),
    /// The body frame of a coroutine.
    Body,
}

#[derive(Clone, Debug)]
struct Frame {
    func: usize,
    pc: usize,
    regs: Vec<V>,
    closure: Option<usize>,
    ret: Ret,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Driver {
    Resume(u16),
    Iter { has: u16, val: u16, iter: usize },
}

/// A running coroutine with its own frames.
struct Level {
    coro: usize,
    driver: Driver,
    stack: Vec<Frame>,
}

/// A value as compared between the VM and the reference.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Nil,
    Bool(bool),
    Int(i64),
    /// Float bits, NaN canonical.
    Float(u64),
    Str(Vec<u8>),
    Arr(Vec<Shape>),
    Map(Vec<(Shape, Shape)>),
    Struct(Vec<Shape>),
    Func,
    /// An error value: its code and payload.
    Err(u32, Box<Shape>),
    Iter,
    Coro(u8),
    /// A reference, by its value.
    Ref(Box<Shape>),
    /// Nesting deeper than the comparison looks.
    Deep,
}

/// Canonical float bits for comparison.
pub fn float_bits(f: f64) -> u64 {
    if f.is_nan() {
        f64::NAN.to_bits()
    } else {
        f.to_bits()
    }
}

/// How a run ends.
#[derive(Clone, Debug, PartialEq)]
pub enum End {
    Ret(Shape),
    Raised(ErrorKind, u32, u32, Shape),
    Thrown(Shape, u32, u32),
    Trapped(ErrorKind, u32, u32),
}

/// How deep [`Shape`]s look into nested objects.
pub const SHAPE_DEPTH: usize = 4;

/// What one instruction did.
enum Flow {
    /// Continue at the next instruction of the same frame.
    Next,
    /// Continue at this pc of the same frame (taken branch, already charged).
    Jump(usize),
    /// The frames changed; the (new) top frame's pc is already right.
    Stay,
    /// Raise a runtime error at the executing instruction.
    Kind(ErrorKind),
    /// Raise a value; the unwind starts at the current top frame, whose pc
    /// is its origin.
    Throw(V),
    /// End the run.
    End(End),
}

struct M<'a> {
    m: &'a Module,
    heap: Vec<O>,
    globals: Vec<V>,
    main: Vec<Frame>,
    chain: Vec<Level>,
    fuel: u64,
    depth: usize,
    strs: HashMap<u32, usize>,
    /// LSB §5.16 `cow` and `aliased`, by object index.
    cow: HashSet<usize>,
    aliased: HashSet<usize>,
}

/// A slot of a container (LSB §5.17).
#[derive(Clone, Copy, Debug)]
enum At {
    Arr(usize, usize),
    Map(usize, usize),
    Field(usize, usize),
}

impl At {
    fn obj(self) -> usize {
        match self {
            At::Arr(o, _) | At::Map(o, _) | At::Field(o, _) => o,
        }
    }
}

/// A flattened argument item of a dynamic call: its value and name.
type Item = (V, Option<Vec<u8>>);

/// What a parameter binds to (LSB §5.15).
#[derive(Clone, Debug)]
enum Slot {
    Arg(usize),
    Default,
    Rest(Vec<usize>),
}

fn default_of(t: ValType) -> V {
    match t {
        ValType::Bool => V::Bool(false),
        ValType::Dyn | ValType::Str | ValType::Ref(_) => V::Nil,
        ValType::F32 | ValType::F64 => V::Float(0.0),
        _ => V::I(0),
    }
}

impl<'a> M<'a> {
    fn func(&self, f: usize) -> &'a bytecode_lang::Function {
        &self.m.functions()[f]
    }

    fn new_frame(&self, f: usize, args: &[V], closure: Option<usize>, ret: Ret) -> Frame {
        let regs: Vec<V> = self
            .func(f)
            .regs()
            .iter()
            .enumerate()
            .map(|(i, &t)| args.get(i).copied().unwrap_or(default_of(t)))
            .collect();
        Frame {
            func: f,
            pc: 0,
            regs,
            closure,
            ret,
        }
    }

    fn nparams(&self, f: usize) -> usize {
        let sig = self.func(f).sig();
        match self.m.types().get(sig.index()) {
            Some(bytecode_lang::TypeDef::Func(ft)) => ft.params.len(),
            _ => 0,
        }
    }

    fn stack(&mut self) -> &mut Vec<Frame> {
        match self.chain.last_mut() {
            Some(l) => &mut l.stack,
            None => &mut self.main,
        }
    }

    fn top(&mut self) -> &mut Frame {
        self.stack().last_mut().unwrap()
    }

    fn cur(&self) -> &Frame {
        match self.chain.last() {
            Some(l) => l.stack.last().unwrap(),
            None => self.main.last().unwrap(),
        }
    }

    fn rv(&self, r: bytecode_lang::Reg) -> V {
        self.cur().regs[r.index()]
    }

    fn total_depth(&self) -> usize {
        self.main.len() + self.chain.iter().map(|l| l.stack.len()).sum::<usize>()
    }

    fn alloc(&mut self, o: O) -> V {
        self.heap.push(o);
        V::Obj(self.heap.len() - 1)
    }

    fn obj(&self, v: V) -> Option<&O> {
        match v {
            V::Obj(i) => self.heap.get(i),
            _ => None,
        }
    }

    fn not_a(v: V) -> ErrorKind {
        if v == V::Nil {
            ErrorKind::NullReference
        } else {
            ErrorKind::TypeError
        }
    }

    fn co(&mut self, c: usize) -> &mut Co {
        match &mut self.heap[c] {
            O::Coro(co) => co,
            _ => unreachable!("not a coroutine"),
        }
    }

    fn coro_of(&self, v: V) -> Result<usize, ErrorKind> {
        match v {
            V::Obj(i) if matches!(self.heap.get(i), Some(O::Coro(_))) => Ok(i),
            other => Err(Self::not_a(other)),
        }
    }

    /// Ends the run with a trap: running coroutines lose their frames.
    fn trap(&mut self, kind: ErrorKind, func: usize, pc: usize) -> End {
        let running: Vec<usize> = self.chain.iter().map(|l| l.coro).collect();
        for c in running {
            let co = self.co(c);
            co.state = CoroState::Failed;
            co.result = V::Nil;
            co.closing = false;
        }
        End::Trapped(kind, func as u32, pc as u32)
    }

    fn charge(&mut self, func: usize, pc: usize) -> Result<(), End> {
        if self.fuel == 0 {
            return Err(self.trap(ErrorKind::OutOfFuel, func, pc));
        }
        self.fuel -= 1;
        Ok(())
    }

    /// The text of a name operand of `func`.
    fn name(&self, func: usize, n: bytecode_lang::NameRef) -> Vec<u8> {
        let id = self.func(func).names()[n.index()];
        self.m.string(id).unwrap_or("").as_bytes().to_vec()
    }

    fn str_bytes(&self, v: V) -> Option<&[u8]> {
        match self.obj(v) {
            Some(O::Str(b)) => Some(b),
            _ => None,
        }
    }

    fn str_const(&mut self, k: u32) -> V {
        if let Some(&i) = self.strs.get(&k) {
            return V::Obj(i);
        }
        let bytes = match self.m.constant(ConstId(k)) {
            Some(Const::Bytes(b)) => b.clone(),
            Some(Const::Str(s)) => self.m.string(*s).unwrap_or("").as_bytes().to_vec(),
            other => unreachable!("constant {other:?}"),
        };
        let v = self.alloc(O::Str(bytes));
        if let V::Obj(i) = v {
            let _ = self.strs.insert(k, i);
        }
        v
    }

    // ------------------------------------------------------------------
    // Values
    // ------------------------------------------------------------------

    fn dv(v: V) -> Dv {
        match v {
            V::Nil => Dv::Nil,
            V::Bool(b) => Dv::Bool(b),
            V::Int(i) => Dv::Int(i),
            V::Float(f) => Dv::Float(f),
            // Objects never reach the numeric reference.
            _ => Dv::Nil,
        }
    }

    fn from_dv(d: Dv) -> V {
        match d {
            Dv::Nil => V::Nil,
            Dv::Bool(b) => V::Bool(b),
            Dv::Int(i) => V::Int(i),
            Dv::Float(f) => V::Float(f),
        }
    }

    fn is_num(v: V) -> bool {
        matches!(v, V::Int(_) | V::Float(_))
    }

    /// LSB §2.4 key equality for `dyn` keys.
    fn key_eq(&self, a: V, b: V) -> bool {
        match (a, b) {
            (V::Int(x), V::Int(y)) => x == y,
            (V::Float(x), V::Float(y)) => {
                let norm = |f: f64| {
                    if f == 0.0 {
                        0.0f64.to_bits()
                    } else {
                        float_bits(f)
                    }
                };
                norm(x) == norm(y)
            }
            (V::Nil, V::Nil) => true,
            (V::Bool(x), V::Bool(y)) => x == y,
            (V::Obj(x), V::Obj(y)) => {
                x == y
                    || matches!(
                        (self.str_bytes(a), self.str_bytes(b)),
                        (Some(p), Some(q)) if p == q
                    )
            }
            _ => false,
        }
    }

    /// The identity `coro_close` compares the escaping error with.
    fn same(a: V, b: V) -> bool {
        match (a, b) {
            (V::Float(x), V::Float(y)) => float_bits(x) == float_bits(y),
            _ => a == b,
        }
    }

    fn deq(&self, a: V, b: V) -> bool {
        if Self::is_num(a) && Self::is_num(b) {
            return dyn_cmp(Self::dv(a), Self::dv(b)) == Some(core::cmp::Ordering::Equal);
        }
        match (a, b) {
            (V::Nil, V::Nil) => true,
            (V::Bool(x), V::Bool(y)) => x == y,
            (V::Obj(_), V::Obj(_)) => match (self.str_bytes(a), self.str_bytes(b)) {
                (Some(p), Some(q)) => p == q,
                _ => a == b,
            },
            _ => false,
        }
    }

    pub fn shape(&self, v: V, depth: usize) -> Shape {
        if depth == 0 {
            return Shape::Deep;
        }
        match v {
            V::Nil => Shape::Nil,
            V::Bool(b) => Shape::Bool(b),
            V::Int(i) => Shape::Int(i),
            V::I(i) => Shape::Int(i as i64),
            V::Float(f) => Shape::Float(float_bits(f)),
            V::Obj(i) => match &self.heap[i] {
                O::Str(b) => Shape::Str(b.clone()),
                // Slots are transparent (LSB §5.17): a reference slot shows
                // its value, as the VM's inspection API reads it.
                O::Arr(items) => Shape::Arr(
                    items
                        .iter()
                        .map(|&x| self.shape(self.deref(x), depth - 1))
                        .collect(),
                ),
                O::Map(m) => Shape::Map(
                    m.entries
                        .iter()
                        .map(|&(k, v)| {
                            (
                                self.shape(k, depth - 1),
                                self.shape(self.deref(v), depth - 1),
                            )
                        })
                        .collect(),
                ),
                O::Struct(f) => Shape::Struct(
                    f.iter()
                        .map(|&x| self.shape(self.deref(x), depth - 1))
                        .collect(),
                ),
                O::Func { .. } => Shape::Func,
                O::Err { kind, payload, .. } => {
                    Shape::Err(kind.code(), Box::new(self.shape(*payload, depth - 1)))
                }
                O::Iter { .. } => Shape::Iter,
                O::Coro(c) => Shape::Coro(c.state.code()),
                O::Ref(x) => Shape::Ref(Box::new(self.shape(*x, depth - 1))),
            },
        }
    }

    // ------------------------------------------------------------------
    // Slots, copy-on-write, references (LSB §5.16, §5.17)
    // ------------------------------------------------------------------

    /// A slot word read as a value: a reference gives its value.
    fn deref(&self, v: V) -> V {
        match self.obj(v) {
            Some(O::Ref(x)) => *x,
            _ => v,
        }
    }

    fn is_ref(&self, v: V) -> bool {
        matches!(self.obj(v), Some(O::Ref(_)))
    }

    fn is_container(&self, v: V) -> bool {
        matches!(self.obj(v), Some(O::Arr(_) | O::Map(_)))
    }

    /// A write to container `o`: a `cow` container marks the arrays and maps
    /// among its contents aliased and clears the bit.
    fn write(&mut self, o: usize) {
        if !self.cow.remove(&o) {
            return;
        }
        let inner: Vec<V> = match &self.heap[o] {
            O::Arr(items) => items.clone(),
            O::Map(mm) => mm.entries.iter().flat_map(|&(k, v)| [k, v]).collect(),
            _ => Vec::new(),
        };
        for v in inner {
            if let V::Obj(i) = v {
                if self.is_container(v) {
                    let _ = self.aliased.insert(i);
                }
            }
        }
    }

    /// `dup`.
    fn dup(&mut self, v: V) -> Result<V, ErrorKind> {
        let V::Obj(i) = v else {
            return if v == V::Nil {
                Err(ErrorKind::NullReference)
            } else {
                Ok(v)
            };
        };
        let o = self.heap[i].clone();
        let copy = match o {
            O::Arr(_) | O::Map(_) => o,
            O::Struct(f) => {
                for &x in &f {
                    if let V::Obj(j) = x {
                        if self.is_container(x) {
                            let _ = self.aliased.insert(j);
                        }
                    }
                }
                O::Struct(f)
            }
            O::Iter { .. } | O::Coro(_) | O::Ref(_) => return Err(ErrorKind::TypeError),
            O::Str(_) | O::Func { .. } | O::Err { .. } => return Ok(v),
        };
        let shares = matches!(copy, O::Arr(_) | O::Map(_));
        let c = self.alloc(copy);
        if shares {
            let _ = self.cow.insert(i);
            if let V::Obj(ci) = c {
                let _ = self.cow.insert(ci);
            }
        }
        Ok(c)
    }

    /// An array or map given by a reference: separated as by `dup`.
    fn separated(&mut self, v: V) -> V {
        if self.is_container(v) {
            self.dup(v).unwrap_or(v)
        } else {
            v
        }
    }

    fn is_aliased(&self, v: V) -> bool {
        match v {
            V::Obj(i) => self.is_container(v) && self.aliased.contains(&i),
            _ => false,
        }
    }

    fn slot_get(&self, s: At) -> V {
        match (s, s.obj()) {
            (At::Arr(_, i), o) => match &self.heap[o] {
                O::Arr(items) => items[i],
                _ => V::Nil,
            },
            (At::Map(_, p), o) => match &self.heap[o] {
                O::Map(mm) => mm.entries[p].1,
                _ => V::Nil,
            },
            (At::Field(_, f), o) => match &self.heap[o] {
                O::Struct(fs) => fs[f],
                _ => V::Nil,
            },
        }
    }

    fn slot_put(&mut self, s: At, v: V) {
        match s {
            At::Arr(o, i) => {
                if let O::Arr(items) = &mut self.heap[o] {
                    items[i] = v;
                }
            }
            At::Map(o, p) => {
                if let O::Map(mm) = &mut self.heap[o] {
                    mm.entries[p].1 = v;
                }
            }
            At::Field(o, f) => {
                if let O::Struct(fs) = &mut self.heap[o] {
                    fs[f] = v;
                }
            }
        }
    }

    /// A value write into a slot: a reference value stores its value; a
    /// reference slot is written through (the container is not written);
    /// otherwise the container is written (struct fields have no `cow`).
    fn slot_write(&mut self, s: At, v: V) {
        let v = self.deref(v);
        let old = self.slot_get(s);
        if let (Some(O::Ref(_)), V::Obj(r)) = (self.obj(old), old) {
            self.heap[r] = O::Ref(v);
            return;
        }
        if !matches!(s, At::Field(..)) {
            self.write(s.obj());
        }
        self.slot_put(s, v);
    }

    /// The position of a key in a map.
    fn map_pos(&self, o: usize, k: V) -> Option<usize> {
        match &self.heap[o] {
            O::Map(mm) => mm.entries.iter().position(|&(x, _)| self.key_eq(x, k)),
            _ => None,
        }
    }

    /// Appends an entry as it is (after the container was written).
    fn map_append(&mut self, o: usize, k: V, v: V) {
        if let O::Map(mm) = &mut self.heap[o] {
            mm.entries.push((k, v));
            if let V::Int(i) = k {
                let next = i128::from(i) + 1;
                if mm.next.is_none_or(|n| next > n) {
                    mm.next = Some(next);
                }
            }
        }
    }

    /// `map_set` (transparent).
    fn map_set(&mut self, o: usize, k: V, v: V) {
        match self.map_pos(o, k) {
            Some(p) => self.slot_write(At::Map(o, p), v),
            None => {
                let v = self.deref(v);
                self.write(o);
                self.map_append(o, k, v);
            }
        }
    }

    /// Stores `v` under `k` as it is (`dbind_*`).
    fn map_set_raw(&mut self, o: usize, k: V, v: V) {
        self.write(o);
        match self.map_pos(o, k) {
            Some(p) => self.slot_put(At::Map(o, p), v),
            None => self.map_append(o, k, v),
        }
    }

    /// A new string object (keys of names).
    fn new_str(&mut self, b: &[u8]) -> V {
        self.alloc(O::Str(b.to_vec()))
    }

    /// The field slot of `name` in the generated struct ("a", "b").
    fn field_of(&self, o: usize, name: &[u8]) -> Option<At> {
        match (&self.heap[o], name) {
            (O::Struct(_), b"a") => Some(At::Field(o, 0)),
            (O::Struct(_), b"b") => Some(At::Field(o, 1)),
            _ => None,
        }
    }

    /// The in-range index of an int key into array `o`: `Err(TypeError)`
    /// for another key kind, `Ok(None)` out of range.
    fn arr_index(&self, o: usize, k: V) -> Result<Option<usize>, ErrorKind> {
        let V::Int(i) = k else {
            return Err(ErrorKind::TypeError);
        };
        let len = match &self.heap[o] {
            O::Arr(items) => items.len(),
            _ => 0,
        };
        Ok(usize::try_from(i).ok().filter(|&i| i < len))
    }

    /// A slot's value separated for a write in place (`dsep_*`).
    fn separate_slot(&mut self, s: At) -> Result<V, ErrorKind> {
        let w = self.slot_get(s);
        let e = self.deref(w);
        if !self.is_aliased(e) {
            return Ok(e);
        }
        let c = self.dup(e)?;
        match w {
            V::Obj(r) if self.is_ref(w) => self.heap[r] = O::Ref(c),
            _ => self.slot_put(s, c),
        }
        Ok(c)
    }

    /// The reference a slot becomes (`dref_*`).
    fn make_ref(&mut self, s: At) -> Result<V, ErrorKind> {
        let w = self.slot_get(s);
        if self.is_ref(w) {
            return Ok(w);
        }
        let e = if self.is_aliased(w) { self.dup(w)? } else { w };
        let b = self.alloc(O::Ref(e));
        self.slot_put(s, b);
        Ok(b)
    }

    /// `dunref_*` of one slot.
    fn unref(&mut self, s: At) {
        let w = self.slot_get(s);
        let Some(O::Ref(v)) = self.obj(w) else {
            return;
        };
        let v = *v;
        let v = self.separated(v);
        if !matches!(s, At::Field(..)) {
            self.write(s.obj());
        }
        self.slot_put(s, v);
    }

    fn dget_index(&mut self, o: V, k: V) -> Result<V, ErrorKind> {
        match (o, self.obj(o)) {
            (V::Obj(i), Some(O::Arr(_))) => match self.arr_index(i, k)? {
                Some(x) => Ok(self.deref(self.slot_get(At::Arr(i, x)))),
                None => Err(ErrorKind::IndexOutOfBounds),
            },
            (V::Obj(i), Some(O::Map(_))) => match self.map_pos(i, k) {
                Some(p) => Ok(self.deref(self.slot_get(At::Map(i, p)))),
                None => Err(ErrorKind::KeyNotFound),
            },
            (_, Some(O::Str(b))) => {
                let V::Int(x) = k else {
                    return Err(ErrorKind::TypeError);
                };
                match usize::try_from(x).ok().and_then(|x| b.get(x)) {
                    Some(&byte) => Ok(V::Int(i64::from(byte))),
                    None => Err(ErrorKind::IndexOutOfBounds),
                }
            }
            _ => Err(ErrorKind::TypeError),
        }
    }

    fn dset_index(&mut self, o: V, k: V, v: V) -> Result<(), ErrorKind> {
        match (o, self.obj(o)) {
            (V::Obj(i), Some(O::Arr(_))) => match self.arr_index(i, k) {
                Ok(Some(x)) => {
                    self.slot_write(At::Arr(i, x), v);
                    Ok(())
                }
                _ => Err(ErrorKind::IndexOutOfBounds),
            },
            (V::Obj(i), Some(O::Map(_))) => {
                self.map_set(i, k, v);
                Ok(())
            }
            _ => Err(ErrorKind::TypeError),
        }
    }

    fn get_prop(&self, o: V, name: &[u8]) -> Result<V, ErrorKind> {
        let V::Obj(i) = o else {
            return Err(ErrorKind::UndefinedProperty);
        };
        if let Some(s) = self.field_of(i, name) {
            return Ok(self.deref(self.slot_get(s)));
        }
        if let O::Map(mm) = &self.heap[i] {
            let found = mm
                .entries
                .iter()
                .find(|&&(k, _)| self.str_bytes(k) == Some(name));
            if let Some(&(_, v)) = found {
                return Ok(self.deref(v));
            }
        }
        Err(ErrorKind::UndefinedProperty)
    }

    fn set_prop(&mut self, o: V, name: &[u8], v: V) -> Result<(), ErrorKind> {
        let V::Obj(i) = o else {
            return Err(ErrorKind::UndefinedProperty);
        };
        if let Some(s) = self.field_of(i, name) {
            self.slot_write(s, v);
            return Ok(());
        }
        if matches!(self.heap[i], O::Map(_)) {
            let k = self.new_str(name);
            self.map_set(i, k, v);
            return Ok(());
        }
        Err(ErrorKind::UndefinedProperty)
    }

    /// The map position of the string key `name`.
    fn name_pos(&self, o: usize, name: &[u8]) -> Option<usize> {
        match &self.heap[o] {
            O::Map(mm) => mm
                .entries
                .iter()
                .position(|&(k, _)| self.str_bytes(k) == Some(name)),
            _ => None,
        }
    }

    fn dsep_index(&mut self, o: V, k: V) -> Result<V, ErrorKind> {
        match (o, self.obj(o)) {
            (V::Obj(i), Some(O::Arr(_))) => {
                let x = self.arr_index(i, k)?;
                self.write(i);
                match x {
                    Some(x) => self.separate_slot(At::Arr(i, x)),
                    None => Ok(V::Nil),
                }
            }
            (V::Obj(i), Some(O::Map(_))) => {
                self.write(i);
                match self.map_pos(i, k) {
                    Some(p) => self.separate_slot(At::Map(i, p)),
                    None => Ok(V::Nil),
                }
            }
            _ => self.dget_index(o, k),
        }
    }

    fn dsep_prop(&mut self, o: V, name: &[u8]) -> Result<V, ErrorKind> {
        if let V::Obj(i) = o {
            if let Some(s) = self.field_of(i, name) {
                return self.separate_slot(s);
            }
            if matches!(self.heap[i], O::Map(_)) {
                self.write(i);
                return match self.name_pos(i, name) {
                    Some(p) => self.separate_slot(At::Map(i, p)),
                    None => Ok(V::Nil),
                };
            }
        }
        self.get_prop(o, name)
    }

    /// The reference to map entry `k`, appended holding nil when absent.
    fn map_ref_at(&mut self, o: usize, k: V) -> Result<V, ErrorKind> {
        self.write(o);
        match self.map_pos(o, k) {
            Some(p) => self.make_ref(At::Map(o, p)),
            None => {
                let b = self.alloc(O::Ref(V::Nil));
                self.map_append(o, k, b);
                Ok(b)
            }
        }
    }

    fn dref_index(&mut self, o: V, k: V) -> Result<V, ErrorKind> {
        match (o, self.obj(o)) {
            (V::Obj(i), Some(O::Arr(_))) => {
                let Some(x) = self.arr_index(i, k)? else {
                    return Err(ErrorKind::IndexOutOfBounds);
                };
                self.write(i);
                self.make_ref(At::Arr(i, x))
            }
            (V::Obj(i), Some(O::Map(_))) => self.map_ref_at(i, k),
            _ => Err(ErrorKind::TypeError),
        }
    }

    fn dref_prop(&mut self, o: V, name: &[u8]) -> Result<V, ErrorKind> {
        let V::Obj(i) = o else {
            return Err(ErrorKind::TypeError);
        };
        match &self.heap[i] {
            O::Struct(_) => match self.field_of(i, name) {
                Some(s) => self.make_ref(s),
                None => Err(ErrorKind::UndefinedProperty),
            },
            O::Map(_) => {
                let k = self.new_str(name);
                match self.name_pos(i, name) {
                    Some(p) => {
                        self.write(i);
                        self.make_ref(At::Map(i, p))
                    }
                    None => self.map_ref_at(i, k),
                }
            }
            _ => Err(ErrorKind::TypeError),
        }
    }

    fn dbind_index(&mut self, o: V, k: V, src: V) -> Result<(), ErrorKind> {
        if !self.is_ref(src) {
            return Err(ErrorKind::TypeError);
        }
        match (o, self.obj(o)) {
            (V::Obj(i), Some(O::Arr(_))) => {
                let Some(x) = self.arr_index(i, k)? else {
                    return Err(ErrorKind::IndexOutOfBounds);
                };
                self.write(i);
                self.slot_put(At::Arr(i, x), src);
                Ok(())
            }
            (V::Obj(i), Some(O::Map(_))) => {
                self.map_set_raw(i, k, src);
                Ok(())
            }
            _ => Err(ErrorKind::TypeError),
        }
    }

    fn dbind_prop(&mut self, o: V, name: &[u8], src: V) -> Result<(), ErrorKind> {
        if !self.is_ref(src) {
            return Err(ErrorKind::TypeError);
        }
        let V::Obj(i) = o else {
            return Err(ErrorKind::TypeError);
        };
        match &self.heap[i] {
            O::Struct(_) => match self.field_of(i, name) {
                Some(s) => {
                    self.slot_put(s, src);
                    Ok(())
                }
                None => Err(ErrorKind::UndefinedProperty),
            },
            O::Map(_) => {
                match self.name_pos(i, name) {
                    Some(p) => {
                        self.write(i);
                        self.slot_put(At::Map(i, p), src);
                    }
                    None => {
                        let k = self.new_str(name);
                        self.write(i);
                        self.map_append(i, k, src);
                    }
                }
                Ok(())
            }
            _ => Err(ErrorKind::TypeError),
        }
    }

    fn dunref_index(&mut self, o: V, k: V) -> Result<(), ErrorKind> {
        match (o, self.obj(o)) {
            (V::Obj(i), Some(O::Arr(_))) => {
                if let Ok(Some(x)) = self.arr_index(i, k) {
                    self.unref(At::Arr(i, x));
                }
                Ok(())
            }
            (V::Obj(i), Some(O::Map(_))) => {
                if let Some(p) = self.map_pos(i, k) {
                    self.unref(At::Map(i, p));
                }
                Ok(())
            }
            _ => Err(ErrorKind::TypeError),
        }
    }

    fn dunref_prop(&mut self, o: V, name: &[u8]) -> Result<(), ErrorKind> {
        let V::Obj(i) = o else {
            return Err(ErrorKind::TypeError);
        };
        match &self.heap[i] {
            O::Struct(_) => {
                if let Some(s) = self.field_of(i, name) {
                    self.unref(s);
                }
                Ok(())
            }
            O::Map(_) => {
                if let Some(p) = self.name_pos(i, name) {
                    self.unref(At::Map(i, p));
                }
                Ok(())
            }
            _ => Err(ErrorKind::TypeError),
        }
    }

    // ------------------------------------------------------------------
    // Dynamic calls (LSB §5.15), written from the spec's steps
    // ------------------------------------------------------------------

    fn params_of(&self, v: V) -> Option<ParamList> {
        match self.obj(v) {
            Some(O::Func { func, .. }) => self.func(*func).params().cloned(),
            _ => None,
        }
    }

    /// Step 2: the window as items.
    fn flatten(&mut self, shape: &CallShape, window: &[V]) -> Result<Vec<Item>, ErrorKind> {
        let mut items = Vec::new();
        for (arg, &w) in shape.args.iter().zip(window) {
            match *arg {
                ArgKind::Positional => items.push((w, None)),
                ArgKind::Named(s) => {
                    let name = self.m.string(s).unwrap_or("").as_bytes().to_vec();
                    items.push((w, Some(name)));
                }
                ArgKind::Spread | ArgKind::SpreadNamed => {
                    let named = matches!(arg, ArgKind::SpreadNamed);
                    match self.obj(w).cloned() {
                        Some(O::Arr(xs)) if !named => {
                            for x in xs {
                                items.push((self.deref(x), None));
                            }
                        }
                        Some(O::Map(mm)) => {
                            for (k, v) in mm.entries {
                                let v = self.deref(v);
                                match (self.str_bytes(k), k) {
                                    (Some(b), _) => items.push((v, Some(b.to_vec()))),
                                    (None, V::Int(_)) if !named => items.push((v, None)),
                                    _ => return Err(ErrorKind::ArgumentError),
                                }
                            }
                        }
                        _ => return Err(ErrorKind::TypeError),
                    }
                }
            }
        }
        Ok(items)
    }

    /// Step 3: items to slots and the presence mask; `Err` is `ArgumentError`.
    fn bind(
        &self,
        list: Option<&ParamList>,
        nparams: usize,
        items: &[Item],
    ) -> Result<(Vec<Slot>, u64), ()> {
        let Some(list) = list else {
            if items.len() != nparams || items.iter().any(|(_, n)| n.is_some()) {
                return Err(());
            }
            return Ok(((0..nparams).map(Slot::Arg).collect(), 0));
        };
        let ps = &list.params;
        let mut slots: Vec<Slot> = ps
            .iter()
            .map(|p| {
                if p.kind.is_rest() {
                    Slot::Rest(Vec::new())
                } else {
                    Slot::Default
                }
            })
            .collect();
        let positional: Vec<usize> = (0..ps.len())
            .filter(|&i| matches!(ps[i].kind, ParamKind::PositionalOnly | ParamKind::Normal))
            .collect();
        let pos_rest = ps
            .iter()
            .position(|p| matches!(p.kind, ParamKind::Rest | ParamKind::RestMap));
        let named_rest = ps
            .iter()
            .position(|p| p.kind == ParamKind::RestNamed)
            .or_else(|| ps.iter().position(|p| p.kind == ParamKind::RestMap));
        let (mut npos, mut named_seen) = (0usize, false);
        let mut rest_names: Vec<Vec<u8>> = Vec::new();
        for (k, (_, name)) in items.iter().enumerate() {
            match name {
                None => {
                    if named_seen {
                        return Err(());
                    }
                    if let Some(&p) = positional.get(npos) {
                        slots[p] = Slot::Arg(k);
                    } else if let Some(r) = pos_rest {
                        if let Slot::Rest(l) = &mut slots[r] {
                            l.push(k);
                        }
                    } else if !list.ignore_extra {
                        return Err(());
                    }
                    npos += 1;
                }
                Some(n) => {
                    named_seen = true;
                    let target = ps.iter().position(|p| {
                        matches!(p.kind, ParamKind::Normal | ParamKind::NamedOnly)
                            && p.name.and_then(|s| self.m.string(s)).map(str::as_bytes)
                                == Some(&n[..])
                    });
                    match target {
                        Some(p) => match slots[p] {
                            Slot::Default => slots[p] = Slot::Arg(k),
                            _ => return Err(()),
                        },
                        None => {
                            let Some(r) = named_rest else { return Err(()) };
                            if rest_names.contains(n) {
                                return Err(());
                            }
                            rest_names.push(n.clone());
                            if let Slot::Rest(l) = &mut slots[r] {
                                l.push(k);
                            }
                        }
                    }
                }
            }
        }
        let mut mask = 0u64;
        for (i, (s, p)) in slots.iter().zip(ps).enumerate() {
            let present = match s {
                Slot::Arg(_) => true,
                Slot::Rest(l) => !l.is_empty(),
                Slot::Default if p.default => false,
                Slot::Default => return Err(()),
            };
            if present && i < 64 {
                mask |= 1 << i;
            }
        }
        Ok((slots, mask))
    }

    /// Step 5: the callee's arguments.
    fn build(
        &mut self,
        list: Option<&ParamList>,
        items: &[Item],
        slots: &[Slot],
        mask: u64,
    ) -> Vec<V> {
        let by_value = |m: &mut Self, w: V| {
            if m.is_ref(w) {
                let x = m.deref(w);
                m.separated(x)
            } else {
                w
            }
        };
        let by_ref = |m: &mut Self, w: V| if m.is_ref(w) { w } else { m.alloc(O::Ref(w)) };
        let Some(list) = list else {
            return items.iter().map(|&(w, _)| by_value(self, w)).collect();
        };
        let mut args = Vec::new();
        for (p, s) in list.params.iter().zip(slots) {
            let v = match s {
                Slot::Arg(k) => {
                    let w = items[*k].0;
                    if p.by_ref {
                        by_ref(self, w)
                    } else {
                        by_value(self, w)
                    }
                }
                Slot::Default => V::Nil,
                Slot::Rest(ks) => {
                    let mut vals = Vec::new();
                    for &k in ks {
                        let w = items[k].0;
                        vals.push(if p.by_ref {
                            by_ref(self, w)
                        } else {
                            by_value(self, w)
                        });
                    }
                    if p.kind == ParamKind::Rest {
                        self.alloc(O::Arr(vals))
                    } else {
                        let mut entries = Vec::new();
                        let mut next: i64 = 0;
                        for (&k, v) in ks.iter().zip(vals) {
                            match &items[k].1 {
                                None => {
                                    entries.push((V::Int(next), v));
                                    next += 1;
                                }
                                Some(n) => {
                                    let key = self.new_str(n);
                                    entries.push((key, v));
                                }
                            }
                        }
                        let next = (next > 0).then_some(i128::from(next));
                        self.alloc(O::Map(RMap { entries, next }))
                    }
                }
            };
            args.push(v);
        }
        if list.params.iter().any(|p| p.default) {
            args.push(V::I(i128::from(mask)));
        }
        args
    }

    /// A dynamic call of `cv` with `items` (LSB §5.15).
    fn dyn_call(
        &mut self,
        func: usize,
        pc: usize,
        dst: u16,
        cv: V,
        items: Vec<Item>,
    ) -> Result<Flow, End> {
        let target = match self.obj(cv) {
            Some(O::Func { func, .. }) => *func,
            // No `call`/`call_shape` hook is bound.
            _ => return Ok(Flow::Kind(ErrorKind::TypeError)),
        };
        let list = self.func(target).params().cloned();
        let Ok((slots, mask)) = self.bind(list.as_ref(), self.nparams(target), &items) else {
            return Ok(Flow::Kind(ErrorKind::ArgumentError));
        };
        self.charge(func, pc)?;
        let args = self.build(list.as_ref(), &items, &slots, mask);
        let closure = match cv {
            V::Obj(i) => Some(i),
            _ => None,
        };
        if !self.push_call(target, args, closure, Ret::Dyn(dst)) {
            return Ok(Flow::Kind(ErrorKind::StackOverflow));
        }
        Ok(Flow::Stay)
    }

    /// `dparam_ref`.
    fn param_ref(&self, cv: V, pos: i128) -> bool {
        let Some(list) = self.params_of(cv) else {
            return false;
        };
        if pos < 0 {
            return false;
        }
        let positional: Vec<_> = list
            .params
            .iter()
            .filter(|p| matches!(p.kind, ParamKind::PositionalOnly | ParamKind::Normal))
            .collect();
        match usize::try_from(pos).ok().and_then(|i| positional.get(i)) {
            Some(p) => p.by_ref,
            None => list
                .params
                .iter()
                .find(|p| matches!(p.kind, ParamKind::Rest | ParamKind::RestMap))
                .is_some_and(|p| p.by_ref),
        }
    }

    /// `dparam_ref_named`.
    fn param_ref_named(&self, cv: V, name: &[u8]) -> bool {
        let Some(list) = self.params_of(cv) else {
            return false;
        };
        let by_name = list.params.iter().find(|p| {
            matches!(p.kind, ParamKind::Normal | ParamKind::NamedOnly)
                && p.name.and_then(|s| self.m.string(s)).map(str::as_bytes) == Some(name)
        });
        if let Some(p) = by_name {
            return p.by_ref;
        }
        let rest = list
            .params
            .iter()
            .find(|p| p.kind == ParamKind::RestNamed)
            .or_else(|| list.params.iter().find(|p| p.kind == ParamKind::RestMap));
        rest.is_some_and(|p| p.by_ref)
    }

    // ------------------------------------------------------------------
    // Coroutines
    // ------------------------------------------------------------------

    /// Makes `c` the running coroutine (rule 1). `Ok(Some(dst))` when it
    /// was suspended (the sent value goes there), `Ok(None)` when it starts.
    fn enter(&mut self, c: usize, driver: Driver) -> Result<Option<u16>, ErrorKind> {
        let depth = self.total_depth();
        let limit = self.depth;
        let co = self.co(c);
        if !co.state.is_resumable() {
            return Err(ErrorKind::InvalidCoroState);
        }
        let created = co.state == CoroState::Created;
        let n = if created { 1 } else { co.stack.len() };
        if depth + n > limit {
            return Err(ErrorKind::StackOverflow);
        }
        let (stack, dst) = if created {
            let (func, closure, args) = (co.func, co.closure, co.args.clone());
            (vec![self.new_frame(func, &args, closure, Ret::Body)], None)
        } else {
            let co = self.co(c);
            (core::mem::take(&mut co.stack), Some(co.resume_dst))
        };
        self.co(c).state = CoroState::Running;
        self.chain.push(Level {
            coro: c,
            driver,
            stack,
        });
        Ok(dst)
    }

    /// Delivers a yield/await to the driver; `Err` raises at its frame.
    fn deliver_suspend(
        &mut self,
        driver: Driver,
        payload: V,
        key: Option<V>,
        refused: bool,
    ) -> Result<(), ErrorKind> {
        if refused {
            return Err(ErrorKind::CloseIgnored);
        }
        match driver {
            Driver::Resume(dst) => {
                let top = self.top();
                top.regs[usize::from(dst)] = payload;
                top.pc += 1;
                Ok(())
            }
            Driver::Iter { has, val, iter } => match key {
                // An await: an async coroutine cannot be iterated.
                None => Err(ErrorKind::TypeError),
                Some(k) => {
                    if let O::Iter { key, .. } = &mut self.heap[iter] {
                        *key = Some(k);
                    }
                    let top = self.top();
                    top.regs[usize::from(val)] = payload;
                    top.regs[usize::from(has)] = V::Bool(true);
                    top.pc += 1;
                    Ok(())
                }
            },
        }
    }

    fn deliver_return(&mut self, driver: Driver, value: V) {
        match driver {
            Driver::Resume(dst) => {
                let top = self.top();
                top.regs[usize::from(dst)] = value;
                top.pc += 1;
            }
            Driver::Iter { has, iter, .. } => {
                if let O::Iter { done, .. } = &mut self.heap[iter] {
                    *done = true;
                }
                let top = self.top();
                top.regs[usize::from(has)] = V::Bool(false);
                top.pc += 1;
            }
        }
    }

    // ------------------------------------------------------------------
    // Unwinding and returning
    // ------------------------------------------------------------------

    fn uncaught(&self, value: V, origin: (usize, usize)) -> End {
        match self.obj(value) {
            Some(O::Err {
                kind,
                func,
                pc,
                payload,
            }) => End::Raised(*kind, *func, *pc, self.shape(*payload, SHAPE_DEPTH)),
            _ => End::Thrown(
                self.shape(value, SHAPE_DEPTH),
                origin.0 as u32,
                origin.1 as u32,
            ),
        }
    }

    /// Raises `value` at the current top frame (LSB §4.3).
    fn unwind(&mut self, value: V) -> Result<(), End> {
        let origin = {
            let t = self.top();
            (t.func, t.pc)
        };
        loop {
            let Some(fr) = self.stack().last() else {
                return Err(self.uncaught(value, origin));
            };
            let (func, pc) = (fr.func, fr.pc);
            let found = self
                .func(func)
                .handlers()
                .iter()
                .find(|h| h.start as usize <= pc && pc < h.end as usize)
                .copied();
            if let Some(h) = found {
                self.charge(func, pc)?;
                let top = self.top();
                top.regs[h.catch.index()] = value;
                top.pc = h.target.index();
                return Ok(());
            }
            let fr = self.stack().pop().unwrap();
            match fr.ret {
                Ret::Entry => return Err(self.uncaught(value, origin)),
                Ret::Body => {
                    let level = self.chain.pop().unwrap();
                    let co = self.co(level.coro);
                    let closed = co.closing && Self::same(value, co.signal);
                    co.closing = false;
                    co.signal = V::Nil;
                    if closed {
                        co.state = CoroState::Returned;
                        co.result = V::Nil;
                        self.deliver_return(level.driver, V::Nil);
                        return Ok(());
                    }
                    co.state = CoroState::Failed;
                    co.result = value;
                }
                _ => {}
            }
        }
    }

    /// Raises a runtime error at the current top frame's pc.
    fn raise(&mut self, kind: ErrorKind) -> Result<(), End> {
        let (func, pc) = {
            let t = self.top();
            (t.func, t.pc)
        };
        let e = self.alloc(O::Err {
            kind,
            func: func as u32,
            pc: pc as u32,
            payload: V::Nil,
        });
        self.unwind(e)
    }

    /// The top frame returns `value` (`None` for `ret_void`).
    fn ret(&mut self, value: Option<V>) -> Option<End> {
        let fr = self.stack().pop().unwrap();
        match fr.ret {
            Ret::Entry => Some(End::Ret(self.shape(value.unwrap_or(V::Nil), SHAPE_DEPTH))),
            Ret::Write(dst) => {
                let top = self.top();
                if let Some(v) = value {
                    top.regs[usize::from(dst)] = v;
                }
                top.pc += 1;
                None
            }
            Ret::Dyn(dst) => {
                let top = self.top();
                top.regs[usize::from(dst)] = value.unwrap_or(V::Nil);
                top.pc += 1;
                None
            }
            Ret::Body => {
                let level = self.chain.pop().unwrap();
                let v = value.unwrap_or(V::Nil);
                let co = self.co(level.coro);
                co.state = CoroState::Returned;
                co.result = v;
                co.closing = false;
                co.signal = V::Nil;
                self.deliver_return(level.driver, v);
                None
            }
        }
    }

    fn push_call(&mut self, f: usize, args: Vec<V>, closure: Option<usize>, ret: Ret) -> bool {
        if self.total_depth() >= self.depth {
            return false;
        }
        let fr = self.new_frame(f, &args, closure, ret);
        self.stack().push(fr);
        true
    }

    // ------------------------------------------------------------------
    // The loop
    // ------------------------------------------------------------------

    fn run(&mut self) -> End {
        loop {
            let (func, pc) = {
                let t = self.top();
                (t.func, t.pc)
            };
            let inst = self.func(func).code()[pc];
            let flow = match self.step(func, pc, inst) {
                Ok(f) => f,
                Err(end) => return end,
            };
            let r = match flow {
                Flow::Next => {
                    self.top().pc += 1;
                    Ok(())
                }
                Flow::Jump(t) => {
                    self.top().pc = t;
                    Ok(())
                }
                Flow::Stay => Ok(()),
                Flow::Kind(k) => self.raise(k),
                Flow::Throw(v) => self.unwind(v),
                Flow::End(e) => return e,
            };
            if let Err(e) = r {
                return e;
            }
        }
    }

    #[allow(clippy::cognitive_complexity)]
    fn step(&mut self, func: usize, pc: usize, inst: Inst) -> Result<Flow, End> {
        // Reads copy a register out; writes go through `set!`.
        macro_rules! r {
            ($x:expr) => {
                self.rv($x)
            };
        }
        macro_rules! set {
            ($x:expr, $v:expr) => {{
                let v = $v;
                self.top().regs[($x).index()] = v;
            }};
        }
        macro_rules! charge {
            () => {
                self.charge(func, pc)?
            };
        }
        macro_rules! int {
            ($v:expr) => {
                match $v {
                    V::I(i) => i,
                    _ => 0,
                }
            };
        }
        macro_rules! kind {
            ($k:expr) => {
                return Ok(Flow::Kind($k))
            };
        }
        macro_rules! jump {
            ($t:expr) => {{
                let t = ($t) as usize;
                if t <= pc {
                    charge!();
                }
                return Ok(Flow::Jump(t));
            }};
        }
        macro_rules! dynop {
            ($name:expr, $dst:expr, $l:expr, $r:expr, $pol:expr) => {{
                let (a, b) = (r!($l), r!($r));
                if !(Self::is_num(a) && Self::is_num(b)) {
                    kind!(ErrorKind::TypeError);
                }
                match dyn_bin($name, $pol, Self::dv(a), Self::dv(b)) {
                    Some(Ok(v)) => set!($dst, Self::from_dv(v)),
                    Some(Err((k, true))) => return Err(self.trap(k, func, pc)),
                    Some(Err((k, false))) => kind!(k),
                    None => kind!(ErrorKind::TypeError),
                }
            }};
        }
        match inst {
            Inst::Nop {} => {}
            Inst::Mov { dst, src } => set!(dst, r!(src)),
            Inst::LoadInt { dst, val, ty } => set!(dst, V::I(wrap(ty, i128::from(val)))),
            Inst::DLoadInt { dst, val } => set!(dst, V::Int(i64::from(val))),
            Inst::LoadNil { dst } => set!(dst, V::Nil),
            Inst::LoadBool { dst, val } => set!(dst, V::Bool(val)),
            Inst::DLoadConst { dst, k } => {
                let v = self.str_const(k.0);
                set!(dst, v);
            }
            Inst::GetGlobal { dst, global } => {
                let v = self.globals[global.index()];
                set!(dst, v);
            }
            Inst::SetGlobal { global, src } => self.globals[global.index()] = r!(src),
            Inst::IAdd { dst, lhs, rhs, op } => {
                match int_bin("add", op.ty(), op.policy(), int!(r!(lhs)), int!(r!(rhs))) {
                    Ok(v) => set!(dst, V::I(v)),
                    Err((k, true)) => return Err(self.trap(k, func, pc)),
                    Err((k, false)) => kind!(k),
                }
            }
            Inst::ILt { dst, lhs, rhs, .. } => {
                let lt = int!(r!(lhs)) < int!(r!(rhs));
                set!(dst, V::Bool(lt));
            }
            Inst::ToDyn { dst, src, from } => {
                let v = match (from, r!(src)) {
                    (Prim::Bool, V::Bool(b)) => V::Bool(b),
                    (_, V::I(i)) => V::Int(i as i64),
                    (_, other) => other,
                };
                set!(dst, v);
            }
            Inst::DAdd { dst, lhs, rhs, pol } => dynop!("add", dst, lhs, rhs, pol),
            Inst::DSub { dst, lhs, rhs, pol } => dynop!("sub", dst, lhs, rhs, pol),
            Inst::DMul { dst, lhs, rhs, pol } => dynop!("mul", dst, lhs, rhs, pol),
            Inst::DDiv { dst, lhs, rhs, pol } => dynop!("div", dst, lhs, rhs, pol),
            Inst::DPow { dst, lhs, rhs, pol } => dynop!("pow", dst, lhs, rhs, pol),
            Inst::DShl { dst, lhs, rhs, pol } => dynop!("shl", dst, lhs, rhs, pol),
            Inst::DShr { dst, lhs, rhs, pol } => dynop!("shr", dst, lhs, rhs, pol),
            Inst::DAbs { dst, src, pol } => match dyn_abs(pol, Self::dv(r!(src))) {
                Some(Ok(v)) if Self::is_num(r!(src)) => set!(dst, Self::from_dv(v)),
                Some(Err((k, true))) => return Err(self.trap(k, func, pc)),
                Some(Err((k, false))) => kind!(k),
                _ => kind!(ErrorKind::TypeError),
            },
            Inst::DLt { dst, lhs, rhs } => {
                let (a, b) = (r!(lhs), r!(rhs));
                let lt = if Self::is_num(a) && Self::is_num(b) {
                    dyn_cmp(Self::dv(a), Self::dv(b)) == Some(core::cmp::Ordering::Less)
                } else {
                    match (self.str_bytes(a), self.str_bytes(b)) {
                        (Some(x), Some(y)) => x < y,
                        _ => kind!(ErrorKind::TypeError),
                    }
                };
                set!(dst, V::Bool(lt));
            }
            Inst::DEq { dst, lhs, rhs } => {
                let e = self.deq(r!(lhs), r!(rhs));
                set!(dst, V::Bool(e));
            }
            Inst::DConcat { dst, lhs, rhs } => {
                let (a, b) = (r!(lhs), r!(rhs));
                let s = match (self.str_bytes(a), self.str_bytes(b)) {
                    (Some(x), Some(y)) => [x, y].concat(),
                    _ => kind!(ErrorKind::TypeError),
                };
                let v = self.alloc(O::Str(s));
                set!(dst, v);
            }
            Inst::Jmp { target } => jump!(target.0),
            Inst::JmpIf { cond, target } => {
                if r!(cond) == V::Bool(true) {
                    jump!(target.0);
                }
            }
            Inst::JmpIfNot { cond, target } => {
                if r!(cond) != V::Bool(true) {
                    jump!(target.0);
                }
            }
            Inst::Switch { src, table, ty } => {
                let v = wrap(ty, int!(r!(src)));
                let t = &self.func(func).tables()[table.index()];
                let target = usize::try_from(v)
                    .ok()
                    .and_then(|i| t.targets.get(i))
                    .copied()
                    .unwrap_or(t.default);
                jump!(target.0);
            }
            Inst::Safepoint {} => charge!(),
            Inst::Ret { src } => {
                let v = r!(src);
                return Ok(match self.ret(Some(v)) {
                    Some(e) => Flow::End(e),
                    None => Flow::Stay,
                });
            }
            Inst::RetVoid {} => {
                return Ok(match self.ret(None) {
                    Some(e) => Flow::End(e),
                    None => Flow::Stay,
                });
            }
            Inst::Throw { src } => return Ok(Flow::Throw(r!(src))),
            Inst::ErrCode { dst, src } => {
                let c = match self.obj(r!(src)) {
                    Some(O::Err { kind, .. }) => kind.code(),
                    _ => 0,
                };
                set!(dst, V::I(i128::from(c)));
            }
            Inst::Call {
                dst,
                func: callee,
                argc,
            } => {
                charge!();
                let first = dst.index() + 1;
                let args = self.cur().regs[first..first + usize::from(argc)].to_vec();
                if !self.push_call(callee.index(), args, None, Ret::Write(dst.0)) {
                    kind!(ErrorKind::StackOverflow);
                }
                return Ok(Flow::Stay);
            }
            Inst::TailCall {
                func: callee,
                args,
                argc,
            } => {
                charge!();
                let first = args.index();
                let a = self.cur().regs[first..first + usize::from(argc)].to_vec();
                let ret = self.cur().ret;
                let fr = self.new_frame(callee.index(), &a, None, ret);
                *self.top() = fr;
                return Ok(Flow::Stay);
            }
            Inst::CallIndirect { dst, callee, argc } => {
                let cv = r!(callee);
                let target = match self.obj(cv) {
                    Some(O::Func { func, .. }) => *func,
                    _ => kind!(Self::not_a(cv)),
                };
                if self.nparams(target) != usize::from(argc) {
                    kind!(ErrorKind::TypeError);
                }
                charge!();
                let first = dst.index() + 1;
                let args = self.cur().regs[first..first + usize::from(argc)].to_vec();
                let closure = match cv {
                    V::Obj(i) => Some(i),
                    _ => None,
                };
                if !self.push_call(target, args, closure, Ret::Write(dst.0)) {
                    kind!(ErrorKind::StackOverflow);
                }
                return Ok(Flow::Stay);
            }
            Inst::DCall { dst, callee, argc } => {
                let first = dst.index() + 1;
                let items: Vec<Item> = self.cur().regs[first..first + usize::from(argc)]
                    .iter()
                    .map(|&w| (w, None))
                    .collect();
                return self.dyn_call(func, pc, dst.0, r!(callee), items);
            }
            Inst::MakeClosure { dst, func: f } => {
                let n = self.func(f.index()).captures().len();
                let first = dst.index() + 1;
                let caps = self.cur().regs[first..first + n].to_vec();
                let v = self.alloc(O::Func {
                    func: f.index(),
                    caps,
                });
                set!(dst, v);
            }
            Inst::GetUpval { dst, idx } => {
                let c = self.cur().closure;
                let v = match c.and_then(|i| self.heap.get(i)) {
                    Some(O::Func { caps, .. }) => caps.get(idx.index()).copied(),
                    _ => None,
                };
                match v {
                    Some(v) => set!(dst, v),
                    None => kind!(ErrorKind::TypeError),
                }
            }
            Inst::NewArray { dst, len, .. } => {
                let n = int!(r!(len));
                if n < 0 {
                    kind!(ErrorKind::IndexOutOfBounds);
                }
                let v = self.alloc(O::Arr(vec![V::Nil; n as usize]));
                set!(dst, v);
            }
            Inst::ArrayLen { dst, arr } => {
                let a = r!(arr);
                let n = match self.obj(a) {
                    Some(O::Arr(items)) => items.len(),
                    _ => kind!(Self::not_a(a)),
                };
                set!(dst, V::I(n as i128));
            }
            Inst::ArrayGet { dst, arr, idx } => {
                let (a, i) = (r!(arr), int!(r!(idx)));
                let V::Obj(o) = a else { kind!(Self::not_a(a)) };
                let O::Arr(items) = &self.heap[o] else {
                    kind!(Self::not_a(a))
                };
                let Some(&w) = usize::try_from(i).ok().and_then(|i| items.get(i)) else {
                    kind!(ErrorKind::IndexOutOfBounds)
                };
                let v = self.deref(w);
                set!(dst, v);
            }
            Inst::ArraySet { arr, idx, src } => {
                let (a, i, v) = (r!(arr), int!(r!(idx)), r!(src));
                let V::Obj(o) = a else { kind!(Self::not_a(a)) };
                let O::Arr(items) = &self.heap[o] else {
                    kind!(Self::not_a(a))
                };
                let Some(x) = usize::try_from(i).ok().filter(|&x| x < items.len()) else {
                    kind!(ErrorKind::IndexOutOfBounds)
                };
                self.slot_write(At::Arr(o, x), v);
            }
            Inst::ArrayPush { arr, src } => {
                let (a, v) = (r!(arr), r!(src));
                let V::Obj(o) = a else { kind!(Self::not_a(a)) };
                if !matches!(self.heap[o], O::Arr(_)) {
                    kind!(Self::not_a(a));
                }
                let v = self.deref(v);
                self.write(o);
                if let O::Arr(items) = &mut self.heap[o] {
                    items.push(v);
                }
            }
            Inst::ArrayPop { dst, arr } => {
                let a = r!(arr);
                let V::Obj(o) = a else { kind!(Self::not_a(a)) };
                let O::Arr(items) = &self.heap[o] else {
                    kind!(Self::not_a(a))
                };
                if items.is_empty() {
                    kind!(ErrorKind::IndexOutOfBounds);
                }
                self.write(o);
                let popped = match &mut self.heap[o] {
                    O::Arr(items) => items.pop().unwrap_or(V::Nil),
                    _ => V::Nil,
                };
                let v = self.deref(popped);
                set!(dst, v);
            }
            Inst::NewMap { dst, .. } => {
                let v = self.alloc(O::Map(RMap {
                    entries: Vec::new(),
                    next: None,
                }));
                set!(dst, v);
            }
            Inst::MapSet { map, key, src } => {
                let (mv, k, v) = (r!(map), r!(key), r!(src));
                let V::Obj(o) = mv else {
                    kind!(Self::not_a(mv))
                };
                if !matches!(self.heap[o], O::Map(_)) {
                    kind!(Self::not_a(mv));
                }
                self.map_set(o, k, v);
            }
            Inst::MapGet { dst, map, key } | Inst::MapFind { dst, map, key } => {
                let (mv, k) = (r!(map), r!(key));
                let V::Obj(o) = mv else {
                    kind!(Self::not_a(mv))
                };
                if !matches!(self.heap[o], O::Map(_)) {
                    kind!(Self::not_a(mv));
                }
                let v = match (self.map_pos(o, k), inst) {
                    (Some(p), _) => self.deref(self.slot_get(At::Map(o, p))),
                    (None, Inst::MapFind { .. }) => V::Nil,
                    (None, _) => kind!(ErrorKind::KeyNotFound),
                };
                set!(dst, v);
            }
            Inst::MapHas { dst, map, key } => {
                let (mv, k) = (r!(map), r!(key));
                let V::Obj(o) = mv else {
                    kind!(Self::not_a(mv))
                };
                if !matches!(self.heap[o], O::Map(_)) {
                    kind!(Self::not_a(mv));
                }
                let has = self.map_pos(o, k).is_some();
                set!(dst, V::Bool(has));
            }
            Inst::MapDel { map, key } => {
                let (mv, k) = (r!(map), r!(key));
                let V::Obj(o) = mv else {
                    kind!(Self::not_a(mv))
                };
                if !matches!(self.heap[o], O::Map(_)) {
                    kind!(Self::not_a(mv));
                }
                if let Some(p) = self.map_pos(o, k) {
                    self.write(o);
                    if let O::Map(mm) = &mut self.heap[o] {
                        let _ = mm.entries.remove(p);
                    }
                }
            }
            Inst::MapPush { map, src } => {
                let (mv, v) = (r!(map), r!(src));
                let V::Obj(o) = mv else {
                    kind!(Self::not_a(mv))
                };
                let O::Map(mm) = &self.heap[o] else {
                    kind!(Self::not_a(mv))
                };
                let next = mm.next.unwrap_or(0);
                let Ok(k) = i64::try_from(next) else {
                    kind!(ErrorKind::ArithOverflow);
                };
                let v = self.deref(v);
                self.write(o);
                // The next key is never present: every integer key ever
                // inserted is below it.
                self.map_append(o, V::Int(k), v);
            }
            Inst::MapLen { dst, map } => {
                let mv = r!(map);
                let n = match self.obj(mv) {
                    Some(O::Map(mm)) => mm.entries.len(),
                    _ => kind!(Self::not_a(mv)),
                };
                set!(dst, V::I(n as i128));
            }
            Inst::NewStruct { dst, ty } => {
                let t = self.func(func).type_refs()[ty.index()];
                let n = match self.m.types().get(t.index()) {
                    Some(bytecode_lang::TypeDef::Struct(s)) => s.fields.len(),
                    _ => 0,
                };
                let v = self.alloc(O::Struct(vec![V::Nil; n]));
                set!(dst, v);
            }
            Inst::GetField { dst, obj, field } => {
                let o = r!(obj);
                let v = match self.obj(o) {
                    Some(O::Struct(f)) => match f.get(field.index()) {
                        Some(&v) => v,
                        None => kind!(ErrorKind::TypeError),
                    },
                    _ => kind!(Self::not_a(o)),
                };
                let v = self.deref(v);
                set!(dst, v);
            }
            Inst::SetField { obj, field, src } => {
                let (o, v) = (r!(obj), r!(src));
                match (o, self.obj(o)) {
                    (V::Obj(i), Some(O::Struct(f))) => {
                        if field.index() >= f.len() {
                            kind!(ErrorKind::TypeError);
                        }
                        self.slot_write(At::Field(i, field.index()), v);
                    }
                    _ => kind!(Self::not_a(o)),
                }
            }
            Inst::DGetIndex { dst, obj, key } => match self.dget_index(r!(obj), r!(key)) {
                Ok(v) => set!(dst, v),
                Err(k) => kind!(k),
            },
            Inst::DSetIndex { obj, key, src } => {
                if let Err(k) = self.dset_index(r!(obj), r!(key), r!(src)) {
                    kind!(k);
                }
            }
            Inst::DSepIndex { dst, obj, key } => match self.dsep_index(r!(obj), r!(key)) {
                Ok(v) => set!(dst, v),
                Err(k) => kind!(k),
            },
            Inst::GetProp { dst, obj, name }
            | Inst::DSepProp { dst, obj, name }
            | Inst::DRefProp { dst, obj, name } => {
                let n = self.name(func, name);
                let o = r!(obj);
                let r = match inst {
                    Inst::GetProp { .. } => self.get_prop(o, &n),
                    Inst::DSepProp { .. } => self.dsep_prop(o, &n),
                    _ => self.dref_prop(o, &n),
                };
                match r {
                    Ok(v) => set!(dst, v),
                    Err(k) => kind!(k),
                }
            }
            Inst::SetProp { obj, name, src } => {
                let n = self.name(func, name);
                if let Err(k) = self.set_prop(r!(obj), &n, r!(src)) {
                    kind!(k);
                }
            }
            Inst::Dup { dst, src } => match self.dup(r!(src)) {
                Ok(v) => set!(dst, v),
                Err(k) => kind!(k),
            },
            Inst::NewRef { dst, src } => {
                let v = self.deref(r!(src));
                let b = self.alloc(O::Ref(v));
                set!(dst, b);
            }
            Inst::CellGet { dst, cell } => {
                let c = r!(cell);
                match self.obj(c) {
                    Some(O::Ref(v)) => {
                        let v = *v;
                        set!(dst, v);
                    }
                    _ => kind!(Self::not_a(c)),
                }
            }
            Inst::CellSet { cell, src } => {
                let (c, v) = (r!(cell), r!(src));
                let v = self.deref(v);
                match c {
                    V::Obj(i) if self.is_ref(c) => self.heap[i] = O::Ref(v),
                    _ => kind!(Self::not_a(c)),
                }
            }
            Inst::DRefIndex { dst, obj, key } => match self.dref_index(r!(obj), r!(key)) {
                Ok(v) => set!(dst, v),
                Err(k) => kind!(k),
            },
            Inst::DBindIndex { obj, key, src } => {
                if let Err(k) = self.dbind_index(r!(obj), r!(key), r!(src)) {
                    kind!(k);
                }
            }
            Inst::DBindProp { obj, name, src } => {
                let n = self.name(func, name);
                if let Err(k) = self.dbind_prop(r!(obj), &n, r!(src)) {
                    kind!(k);
                }
            }
            Inst::DUnrefIndex { obj, key } => {
                if let Err(k) = self.dunref_index(r!(obj), r!(key)) {
                    kind!(k);
                }
            }
            Inst::DUnrefProp { obj, name } => {
                let n = self.name(func, name);
                if let Err(k) = self.dunref_prop(r!(obj), &n) {
                    kind!(k);
                }
            }
            Inst::Raise { src, kind } => {
                let e = self.alloc(O::Err {
                    kind,
                    func: func as u32,
                    pc: pc as u32,
                    payload: r!(src),
                });
                return Ok(Flow::Throw(e));
            }
            Inst::ErrPayload { dst, src } => {
                let v = match self.obj(r!(src)) {
                    Some(O::Err { payload, .. }) => *payload,
                    _ => V::Nil,
                };
                set!(dst, v);
            }
            Inst::DParamRef { dst, callee, pos } => {
                let b = self.param_ref(r!(callee), int!(r!(pos)));
                set!(dst, V::Bool(b));
            }
            Inst::DParamRefNamed { dst, callee, name } => {
                let n = self.name(func, name);
                let b = self.param_ref_named(r!(callee), &n);
                set!(dst, V::Bool(b));
            }
            Inst::DCallShape { dst, callee, shape } => {
                let sh = self.func(func).shapes()[shape.index()].clone();
                let first = dst.index() + 1;
                let window = self.cur().regs[first..first + sh.args.len()].to_vec();
                let items = match self.flatten(&sh, &window) {
                    Ok(items) => items,
                    Err(k) => kind!(k),
                };
                return self.dyn_call(func, pc, dst.0, r!(callee), items);
            }
            Inst::IAnd { dst, lhs, rhs, op } => {
                match int_bin("and", op.ty(), op.policy(), int!(r!(lhs)), int!(r!(rhs))) {
                    Ok(v) => set!(dst, V::I(v)),
                    Err((k, _)) => kind!(k),
                }
            }
            Inst::IEq { dst, lhs, rhs, .. } => {
                let e = int!(r!(lhs)) == int!(r!(rhs));
                set!(dst, V::Bool(e));
            }
            Inst::StrConcat { dst, lhs, rhs } => {
                let (a, b) = (r!(lhs), r!(rhs));
                let s = match (self.str_bytes(a), self.str_bytes(b)) {
                    (Some(x), Some(y)) => [x, y].concat(),
                    (None, _) => kind!(Self::not_a(a)),
                    (_, None) => kind!(Self::not_a(b)),
                };
                let v = self.alloc(O::Str(s));
                set!(dst, v);
            }
            Inst::StrLen { dst, s } => {
                let sv = r!(s);
                let n = match self.str_bytes(sv) {
                    Some(b) => b.len(),
                    None => kind!(Self::not_a(sv)),
                };
                set!(dst, V::I(n as i128));
            }

            // Coroutines: every instruction charges first (LSB §5.14).
            Inst::CoroNew { dst, func: f, argc } => {
                charge!();
                let first = dst.index() + 1;
                let args = self.cur().regs[first..first + usize::from(argc)].to_vec();
                let v = self.new_coro(f.index(), None, args);
                set!(dst, v);
            }
            Inst::CoroNewIndirect { dst, callee, argc } | Inst::Spawn { dst, callee, argc } => {
                charge!();
                if matches!(inst, Inst::Spawn { .. }) {
                    // No `spawn` hook is ever bound here.
                    kind!(ErrorKind::NoScheduler);
                }
                let cv = r!(callee);
                let target = match self.obj(cv) {
                    Some(O::Func { func, .. }) => *func,
                    _ => kind!(Self::not_a(cv)),
                };
                // A `dyn` callee binds its window as `dcall` does (§5.15).
                let first = dst.index() + 1;
                let items: Vec<Item> = self.cur().regs[first..first + usize::from(argc)]
                    .iter()
                    .map(|&w| (w, None))
                    .collect();
                let list = self.func(target).params().cloned();
                let Ok((slots, mask)) = self.bind(list.as_ref(), self.nparams(target), &items)
                else {
                    kind!(ErrorKind::ArgumentError)
                };
                let args = self.build(list.as_ref(), &items, &slots, mask);
                let closure = match cv {
                    V::Obj(i) => Some(i),
                    _ => None,
                };
                let v = self.new_coro(target, closure, args);
                set!(dst, v);
            }
            Inst::Yield { dst, src }
            | Inst::YieldKv { dst, src, .. }
            | Inst::Await { dst, src } => {
                charge!();
                let Some(level) = self.chain.last() else {
                    kind!(ErrorKind::CannotSuspend);
                };
                let c = level.coro;
                let payload = r!(src);
                let awaiting = matches!(inst, Inst::Await { .. });
                // Rule 10 (PHP's generators): the counter starts at -1, an
                // explicit integer key raises it, `yield` takes one more.
                let (key, max) = match inst {
                    Inst::Await { .. } => (None, self.co(c).max_key),
                    Inst::YieldKv { key, .. } => {
                        let k = r!(key);
                        let max = self.co(c).max_key;
                        let max = match k {
                            V::Int(i) => max.max(i),
                            _ => max,
                        };
                        (Some(k), max)
                    }
                    _ => {
                        let next = match self.co(c).max_key.checked_add(1) {
                            Some(n) => n,
                            None => kind!(ErrorKind::ArithOverflow),
                        };
                        (Some(V::Int(next)), next)
                    }
                };
                let level = self.chain.pop().unwrap();
                let co = self.co(c);
                co.stack = level.stack;
                co.resume_dst = dst.0;
                co.state = if awaiting {
                    CoroState::Awaiting
                } else {
                    CoroState::Yielded
                };
                co.max_key = max;
                if let Some(k) = key {
                    co.key = k;
                }
                let refused = !awaiting && co.closing;
                if refused {
                    co.closing = false;
                    co.signal = V::Nil;
                }
                let deliver_key = if awaiting { None } else { key };
                return Ok(
                    match self.deliver_suspend(level.driver, payload, deliver_key, refused) {
                        Ok(()) => Flow::Stay,
                        Err(k) => Flow::Kind(k),
                    },
                );
            }
            Inst::Resume { dst, coro, src } => {
                charge!();
                let (cv, sent) = (r!(coro), r!(src));
                let c = match self.coro_of(cv) {
                    Ok(c) => c,
                    Err(k) => kind!(k),
                };
                match self.enter(c, Driver::Resume(dst.0)) {
                    Ok(Some(d)) => {
                        let top = self.top();
                        top.regs[usize::from(d)] = sent;
                        top.pc += 1;
                    }
                    Ok(None) => {}
                    Err(k) => kind!(k),
                }
                return Ok(Flow::Stay);
            }
            Inst::ResumeThrow { dst, coro, src } => {
                charge!();
                let (cv, err) = (r!(coro), r!(src));
                let c = match self.coro_of(cv) {
                    Ok(c) => c,
                    Err(k) => kind!(k),
                };
                if self.co(c).state == CoroState::Created {
                    let co = self.co(c);
                    co.state = CoroState::Failed;
                    co.result = err;
                    return Ok(Flow::Throw(err));
                }
                if let Err(k) = self.enter(c, Driver::Resume(dst.0)) {
                    kind!(k);
                }
                return Ok(Flow::Throw(err));
            }
            Inst::CoroClose { dst, coro, src } => {
                charge!();
                let (cv, signal) = (r!(coro), r!(src));
                let c = match self.coro_of(cv) {
                    Ok(c) => c,
                    Err(k) => kind!(k),
                };
                match self.co(c).state {
                    CoroState::Created => {
                        let co = self.co(c);
                        co.state = CoroState::Returned;
                        co.result = V::Nil;
                        set!(dst, V::Nil);
                    }
                    CoroState::Returned | CoroState::Failed => set!(dst, V::Nil),
                    CoroState::Running => kind!(ErrorKind::InvalidCoroState),
                    _ => {
                        if let Err(k) = self.enter(c, Driver::Resume(dst.0)) {
                            kind!(k);
                        }
                        let co = self.co(c);
                        co.closing = true;
                        co.signal = signal;
                        return Ok(Flow::Throw(signal));
                    }
                }
            }
            Inst::CoroStatus { dst, coro } => {
                charge!();
                let c = match self.coro_of(r!(coro)) {
                    Ok(c) => c,
                    Err(k) => kind!(k),
                };
                let s = self.co(c).state.code();
                set!(dst, V::I(i128::from(s)));
            }
            Inst::CoroKey { dst, coro } => {
                charge!();
                let c = match self.coro_of(r!(coro)) {
                    Ok(c) => c,
                    Err(k) => kind!(k),
                };
                let k = self.co(c).key;
                set!(dst, k);
            }
            Inst::CoroResult { dst, coro } => {
                charge!();
                let c = match self.coro_of(r!(coro)) {
                    Ok(c) => c,
                    Err(k) => kind!(k),
                };
                let co = self.co(c);
                if co.state != CoroState::Returned {
                    kind!(ErrorKind::InvalidCoroState);
                }
                let v = co.result;
                set!(dst, v);
            }
            Inst::CoroCurrent { dst } => {
                charge!();
                let v = self.chain.last().map_or(V::Nil, |l| V::Obj(l.coro));
                set!(dst, v);
            }
            Inst::DIterNew { dst, src } | Inst::IterNew { dst, src } => {
                let Ok(c) = self.coro_of(r!(src)) else {
                    unreachable!("the generator iterates only coroutines")
                };
                let v = self.alloc(O::Iter {
                    coro: c,
                    done: false,
                    key: None,
                });
                set!(dst, v);
            }
            Inst::IterNext { has, iter, val } => {
                let V::Obj(it) = r!(iter) else {
                    unreachable!("an iterator")
                };
                let (c, done) = match &self.heap[it] {
                    O::Iter { coro, done, .. } => (*coro, *done),
                    _ => unreachable!("an iterator"),
                };
                charge!();
                let state = self.co(c).state;
                if done || state == CoroState::Returned {
                    if let O::Iter { done, .. } = &mut self.heap[it] {
                        *done = true;
                    }
                    set!(has, V::Bool(false));
                    return Ok(Flow::Next);
                }
                if state == CoroState::Awaiting {
                    kind!(ErrorKind::TypeError);
                }
                match self.enter(
                    c,
                    Driver::Iter {
                        has: has.0,
                        val: val.0,
                        iter: it,
                    },
                ) {
                    Ok(Some(d)) => {
                        let top = self.top();
                        top.regs[usize::from(d)] = V::Nil;
                        top.pc += 1;
                    }
                    Ok(None) => {}
                    Err(k) => kind!(k),
                }
                return Ok(Flow::Stay);
            }
            Inst::IterKey { dst, iter } => {
                let key = match self.obj(r!(iter)) {
                    Some(O::Iter { key, .. }) => *key,
                    _ => unreachable!("an iterator"),
                };
                match key {
                    Some(k) => set!(dst, k),
                    None => kind!(ErrorKind::IndexOutOfBounds),
                }
            }
            other => unreachable!("the reference does not model {other}"),
        }
        Ok(Flow::Next)
    }

    fn new_coro(&mut self, func: usize, closure: Option<usize>, args: Vec<V>) -> V {
        self.alloc(O::Coro(Box::new(Co {
            state: CoroState::Created,
            closing: false,
            signal: V::Nil,
            func,
            closure,
            args,
            stack: Vec::new(),
            resume_dst: 0,
            key: V::Nil,
            max_key: -1,
            result: V::Nil,
        })))
    }
}

/// Runs `entry` with `args` under `fuel` and a call-depth limit. Returns the
/// outcome, the fuel used, and every global's shape.
pub fn run(
    module: &Module,
    entry: FuncId,
    args: &[V],
    fuel: u64,
    depth: usize,
) -> (End, u64, Vec<Shape>) {
    let mut m = M {
        m: module,
        heap: Vec::new(),
        globals: vec![V::Nil; module.globals().len()],
        main: Vec::new(),
        chain: Vec::new(),
        fuel,
        depth,
        strs: HashMap::new(),
        cow: HashSet::new(),
        aliased: HashSet::new(),
    };
    let fr = m.new_frame(entry.index(), args, None, Ret::Entry);
    m.main.push(fr);
    let end = m.run();
    let globals = m
        .globals
        .clone()
        .into_iter()
        .map(|g| m.shape(g, SHAPE_DEPTH))
        .collect();
    (end, fuel - m.fuel, globals)
}

/// The integer type of `i64` registers (re-exported for generators).
pub const I64: IntTy = IntTy::I64;
