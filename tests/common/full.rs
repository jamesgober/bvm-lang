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
//! reference), `dconcat`, `to_dyn` from integers and `bool`, typed `i64`
//! loop arithmetic, branches and `switch`, `safepoint`, calls (direct,
//! indirect, `dcall`, tail), closures and captures, `throw`/`err_code`,
//! handlers, arrays, maps, structs, strings, and every coroutine
//! instruction, with iteration over coroutines. No hooks are bound.

#![allow(dead_code, clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::HashMap;

use bytecode_lang::{
    Const, ConstId, CoroState, ErrorKind, FuncId, Inst, IntTy, Module, Prim, ValType,
};

use super::reference::{Dv, dyn_bin, dyn_cmp, int_bin, wrap};

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
    max_key: Option<i64>,
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
    },
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
    Err(u32),
    Iter,
    Coro(u8),
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
    Raised(ErrorKind, u32, u32),
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
                O::Arr(items) => {
                    Shape::Arr(items.iter().map(|&x| self.shape(x, depth - 1)).collect())
                }
                O::Map(m) => Shape::Map(
                    m.entries
                        .iter()
                        .map(|&(k, v)| (self.shape(k, depth - 1), self.shape(v, depth - 1)))
                        .collect(),
                ),
                O::Struct(f) => {
                    Shape::Struct(f.iter().map(|&x| self.shape(x, depth - 1)).collect())
                }
                O::Func { .. } => Shape::Func,
                O::Err { kind, .. } => Shape::Err(kind.code()),
                O::Iter { .. } => Shape::Iter,
                O::Coro(c) => Shape::Coro(c.state.code()),
            },
        }
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
            Some(O::Err { kind, func, pc }) => End::Raised(*kind, *func, *pc),
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
            Inst::CallIndirect { dst, callee, argc } | Inst::DCall { dst, callee, argc } => {
                let dynamic = matches!(inst, Inst::DCall { .. });
                let cv = r!(callee);
                let target = match self.obj(cv) {
                    Some(O::Func { func, .. }) => *func,
                    // `dcall` on a non-callable is the `call` hook's, and
                    // none is bound.
                    _ if dynamic => kind!(ErrorKind::TypeError),
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
                let ret = if dynamic {
                    Ret::Dyn(dst.0)
                } else {
                    Ret::Write(dst.0)
                };
                if !self.push_call(target, args, closure, ret) {
                    kind!(ErrorKind::StackOverflow);
                }
                return Ok(Flow::Stay);
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
                let v = match self.obj(a) {
                    Some(O::Arr(items)) => match usize::try_from(i).ok().and_then(|i| items.get(i))
                    {
                        Some(&v) => v,
                        None => kind!(ErrorKind::IndexOutOfBounds),
                    },
                    _ => kind!(Self::not_a(a)),
                };
                set!(dst, v);
            }
            Inst::ArraySet { arr, idx, src } => {
                let (a, i, v) = (r!(arr), int!(r!(idx)), r!(src));
                match a {
                    V::Obj(o) if matches!(self.heap[o], O::Arr(_)) => {
                        if let O::Arr(items) = &mut self.heap[o] {
                            match usize::try_from(i).ok().and_then(|i| items.get_mut(i)) {
                                Some(slot) => *slot = v,
                                None => kind!(ErrorKind::IndexOutOfBounds),
                            }
                        }
                    }
                    other => kind!(Self::not_a(other)),
                }
            }
            Inst::ArrayPush { arr, src } => {
                let (a, v) = (r!(arr), r!(src));
                match a {
                    V::Obj(o) if matches!(self.heap[o], O::Arr(_)) => {
                        if let O::Arr(items) = &mut self.heap[o] {
                            items.push(v);
                        }
                    }
                    other => kind!(Self::not_a(other)),
                }
            }
            Inst::ArrayPop { dst, arr } => {
                let a = r!(arr);
                let popped = match a {
                    V::Obj(o) if matches!(self.heap[o], O::Arr(_)) => match &mut self.heap[o] {
                        O::Arr(items) => items.pop(),
                        _ => None,
                    },
                    other => kind!(Self::not_a(other)),
                };
                match popped {
                    Some(v) => set!(dst, v),
                    None => kind!(ErrorKind::IndexOutOfBounds),
                }
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
                let Some(O::Map(mm)) = self.obj(mv) else {
                    kind!(Self::not_a(mv));
                };
                let pos = mm.entries.iter().position(|&(x, _)| self.key_eq(x, k));
                let V::Obj(o) = mv else { unreachable!() };
                if let O::Map(mm) = &mut self.heap[o] {
                    match pos {
                        Some(p) => mm.entries[p].1 = v,
                        None => {
                            mm.entries.push((k, v));
                            if let V::Int(i) = k {
                                let next = i128::from(i) + 1;
                                if mm.next.is_none_or(|n| next > n) {
                                    mm.next = Some(next);
                                }
                            }
                        }
                    }
                }
            }
            Inst::MapGet { dst, map, key } | Inst::MapFind { dst, map, key } => {
                let (mv, k) = (r!(map), r!(key));
                let Some(O::Map(mm)) = self.obj(mv) else {
                    kind!(Self::not_a(mv));
                };
                let found = mm
                    .entries
                    .iter()
                    .find(|&&(x, _)| self.key_eq(x, k))
                    .map(|e| e.1);
                let v = match (found, inst) {
                    (Some(v), _) => v,
                    (None, Inst::MapFind { .. }) => V::Nil,
                    (None, _) => kind!(ErrorKind::KeyNotFound),
                };
                set!(dst, v);
            }
            Inst::MapHas { dst, map, key } => {
                let (mv, k) = (r!(map), r!(key));
                let Some(O::Map(mm)) = self.obj(mv) else {
                    kind!(Self::not_a(mv));
                };
                let has = mm.entries.iter().any(|&(x, _)| self.key_eq(x, k));
                set!(dst, V::Bool(has));
            }
            Inst::MapDel { map, key } => {
                let (mv, k) = (r!(map), r!(key));
                let Some(O::Map(mm)) = self.obj(mv) else {
                    kind!(Self::not_a(mv));
                };
                let pos = mm.entries.iter().position(|&(x, _)| self.key_eq(x, k));
                let V::Obj(o) = mv else { unreachable!() };
                if let (Some(p), O::Map(mm)) = (pos, &mut self.heap[o]) {
                    let _ = mm.entries.remove(p);
                }
            }
            Inst::MapPush { map, src } => {
                let (mv, v) = (r!(map), r!(src));
                let Some(O::Map(mm)) = self.obj(mv) else {
                    kind!(Self::not_a(mv));
                };
                let next = mm.next.unwrap_or(0);
                let Ok(k) = i64::try_from(next) else {
                    kind!(ErrorKind::ArithOverflow);
                };
                let V::Obj(o) = mv else { unreachable!() };
                if let O::Map(mm) = &mut self.heap[o] {
                    // The next key is never present: every integer key ever
                    // inserted is below it.
                    mm.entries.push((V::Int(k), v));
                    mm.next = Some(next + 1);
                }
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
                set!(dst, v);
            }
            Inst::SetField { obj, field, src } => {
                let (o, v) = (r!(obj), r!(src));
                match o {
                    V::Obj(i) if matches!(self.heap[i], O::Struct(_)) => {
                        if let O::Struct(f) = &mut self.heap[i] {
                            match f.get_mut(field.index()) {
                                Some(slot) => *slot = v,
                                None => kind!(ErrorKind::TypeError),
                            }
                        }
                    }
                    other => kind!(Self::not_a(other)),
                }
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
                if self.nparams(target) != usize::from(argc) {
                    kind!(ErrorKind::TypeError);
                }
                let first = dst.index() + 1;
                let args = self.cur().regs[first..first + usize::from(argc)].to_vec();
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
                let (key, max) = match inst {
                    Inst::Await { .. } => (None, self.co(c).max_key),
                    Inst::YieldKv { key, .. } => {
                        let k = r!(key);
                        let max = self.co(c).max_key;
                        let max = match k {
                            V::Int(i) => Some(max.map_or(i, |m| m.max(i))),
                            _ => max,
                        };
                        (Some(k), max)
                    }
                    _ => {
                        let next = match self.co(c).max_key {
                            None => 0,
                            Some(m) => match m.checked_add(1) {
                                Some(n) => n,
                                None => kind!(ErrorKind::ArithOverflow),
                            },
                        };
                        (Some(V::Int(next)), Some(next))
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
            max_key: None,
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
