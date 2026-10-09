//! The dispatch loop.
//!
//! One `match` over [`Inst`] walks a function's code with a program counter;
//! the instruction set is bytecode-lang's decoded form, executed as is. Calls
//! do not recurse in Rust: a call pushes a [`Frame`] and the outer loop picks
//! it up, so bytecode recursion depth is bounded only by the configured
//! limits, never by the native stack.
//!
//! **Hot and cold.** The loop in [`run`] handles the instructions that
//! dominate real programs inline (moves, typed integer and float arithmetic,
//! branches, calls and returns, field access, and the inline-int fast paths of
//! the dynamic instructions); everything else goes through one out-of-line
//! call to [`cold`], which reports back with a [`Step`]. Keeping the loop small
//! lets its state (`pc`, the frame base, the code and register slices) stay in
//! machine registers (on the dispatch benchmarks it measured faster than one
//! `match` over every instruction, by roughly 10% on a noisy machine).
//!
//! **Indexing invariant.** Registers are read as `stack[base + r]`. The
//! loader proved every register operand (and every register of every call
//! window) is below its function's register count, and a frame always owns
//! `base..base + nregs` of the stack, so these indexes are in bounds by
//! construction; the same holds for globals, jump tables, type refs, and
//! names. Everything a program can make arbitrary at run time (heap
//! references, array indices, map keys, string positions) goes through
//! checked lookups.
//!
//! **Faults.** An instruction that fails breaks out of the dispatch loop with
//! a [`Fault`] before writing its destination (LSB §4.3); the frame's pc is
//! recorded and [`Machine::unwind`] finds the handler.
//!
//! **Fuel.** One unit is charged at every `safepoint`, every call-family
//! instruction (calls, tail calls, host calls, hook invocations), every taken
//! backward branch, and every handler entry. LSB's verifier will require a
//! safepoint or call on every loop (V-CF6); until it exists, charging
//! backward branches is what bounds a module that omits them, and no loop
//! can avoid every one of these points.

use alloc::vec::Vec;

use bytecode_lang::{Callee, ErrorKind, FloatTy, Hook, Inst, IntTy, ValType};

use crate::coll::{self, index_of, not_a};
use crate::conv::{self, prim_type};
use crate::dynops::{self, DOp};
use crate::dynv;
use crate::error::VmError;
use crate::fault::Fault;
use crate::fmath;
use crate::heap::{ArrayObj, Callable, CellObj, FuncObj, MapObj, Object, StructObj};
use crate::int::{self, Bin, Cmp, Un};
use crate::machine::{Cont, Machine, Next};
use crate::program::{FuncInfo, Program, TypeInfo};

const SIGN32: u64 = 0x8000_0000;
const SIGN64: u64 = 1 << 63;

/// Runs `func` with `args` (already in their parameters' representations)
/// until its frame returns. Returns the result word, or `None` for a void
/// function.
pub(crate) fn execute(
    m: &mut Machine,
    prog: &Program,
    func: u32,
    args: &[u64],
    fuel: &mut u64,
) -> Result<Option<u64>, VmError> {
    // The interpreter owns the register stack while it runs (so its buffer
    // pointer lives in a register rather than behind `m`); the allocation
    // goes back to the machine afterwards to be reused.
    let mut stack = core::mem::take(&mut m.stack);
    stack.clear();
    m.frames.clear();
    let pushed = m.push_frame(&mut stack, prog, func, dynv::NIL, Cont::Entry);
    let base = match pushed {
        Ok(b) => b,
        Err(_) => {
            m.stack = stack;
            return Err(VmError::Raised {
                kind: ErrorKind::StackOverflow,
                func: bytecode_lang::FuncId(func),
                pc: 0,
            });
        }
    };
    if let Some(dst) = stack.get_mut(base..base + args.len()) {
        dst.copy_from_slice(args);
    }
    let mut local_fuel = *fuel;
    let result = run(m, &mut stack, prog, &mut local_fuel);
    *fuel = local_fuel;
    stack.clear();
    m.stack = stack;
    m.frames.clear();
    result
}

/// The read of a float register as `f32`.
#[inline]
fn f32_of(bits: u64) -> f32 {
    f32::from_bits(bits as u32)
}

#[inline]
fn f32_bits(f: f32) -> u64 {
    u64::from(f.to_bits())
}

/// `-1`, `0`, `1` as `i8` register words.
#[inline]
fn ordering_word(o: core::cmp::Ordering) -> u64 {
    (o as i8) as i64 as u64
}

/// Truncation of a float to an integer type (OPS §5 `float_to_int`).
fn float_to_int(x: f64, op: bytecode_lang::IntOp) -> Result<u64, Fault> {
    let ty = op.ty();
    let bits = ty.bits() as i32;
    let (lo, hi) = if ty.is_signed() {
        (-pow2(bits - 1), pow2(bits - 1))
    } else {
        (0.0, pow2(bits))
    };
    let t = fmath::trunc(x);
    if !x.is_nan() && t >= lo && t < hi {
        // In range and integral: the conversion is exact.
        return int::from_value(ty, t as i128).ok_or(Fault::Raise(ErrorKind::InvalidConversion));
    }
    if op.policy().float_to_int() == bytecode_lang::FloatToInt::Saturate {
        let v: i128 = if x.is_nan() {
            0
        } else if t < lo {
            lo as i128
        } else {
            hi as i128 - 1
        };
        return Ok(int::from_value(ty, v).unwrap_or(0));
    }
    Err(Fault::Raise(ErrorKind::InvalidConversion))
}

/// `2^e` as an exact `f64` (`0 <= e <= 64`).
fn pow2(e: i32) -> f64 {
    f64::from_bits(((e + 1023) as u64) << 52)
}

/// Whether byte `i` of `s` is a UTF-8 continuation byte.
#[inline]
fn is_continuation(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(|b| b & 0xC0 == 0x80)
}

/// The outcome of a dynamic index or property read before hooks.
enum Lookup {
    Found(u64),
    /// Absent or out of range: the hook decides, else this error.
    Miss(ErrorKind),
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run(
    m: &mut Machine,
    stack: &mut Vec<u64>,
    prog: &Program,
    fuel: &mut u64,
) -> Result<Option<u64>, VmError> {
    'frame: loop {
        let Some(&top) = m.frames.last() else {
            return Ok(None);
        };
        let fi = m.frames.len() - 1;
        let func = top.func;
        let base = top.base;
        let closure = top.closure;
        let Some(info) = prog.func(func) else {
            return Err(VmError::NoSuchFunction(bytecode_lang::FuncId(func)));
        };
        let code = prog.code(func);
        let module_fn = prog.module.function(bytecode_lang::FuncId(func));
        let mut pc = top.pc as usize;

        let fault: Fault = 'dispatch: loop {
            let inst = code[pc];
            macro_rules! r {
                ($x:expr) => {
                    stack[base + ($x).index()]
                };
            }
            macro_rules! t {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(f) => break 'dispatch f,
                    }
                };
            }
            macro_rules! fail {
                ($f:expr) => {
                    break 'dispatch $f
                };
            }
            macro_rules! charge {
                () => {
                    if *fuel == 0 {
                        break 'dispatch Fault::Trap(ErrorKind::OutOfFuel);
                    }
                    *fuel -= 1;
                };
            }
            macro_rules! gc {
                ($extra:expr) => {
                    if m.heap.wants_gc($extra) {
                        m.collect(stack, prog);
                    }
                };
            }
            macro_rules! jump {
                ($target:expr) => {{
                    let target = ($target).index();
                    if target <= pc {
                        charge!();
                    }
                    pc = target;
                    continue 'dispatch;
                }};
            }
            macro_rules! cold {
                () => {{
                    let cx = Cx {
                        info,
                        base,
                        closure,
                        fi,
                    };
                    match cold(m, stack, prog, &cx, pc, inst, fuel) {
                        Step::Next => {}
                        Step::Frame => continue 'frame,
                        Step::Fault(f) => break 'dispatch f,
                    }
                }};
            }
            macro_rules! fast_arith {
                ($op:expr, $dst:expr, $lhs:expr, $rhs:expr) => {{
                    match dynops::arith_inline($op, r!($lhs), r!($rhs)) {
                        Some(v) => r!($dst) = v,
                        None => cold!(),
                    }
                }};
            }
            macro_rules! fast_cmp {
                ($dst:expr, $a:expr, $b:expr, $le:expr) => {{
                    let (a, b) = ($a, $b);
                    if dynv::is_inline_int(a) && dynv::is_inline_int(b) {
                        let (x, y) = (dynv::inline_int_value(a), dynv::inline_int_value(b));
                        r!($dst) = u64::from(x < y || ($le && x == y));
                    } else {
                        cold!();
                    }
                }};
            }
            macro_rules! int_bin {
                ($op:expr, $dst:expr, $lhs:expr, $rhs:expr, $iop:expr) => {{
                    let (a, b) = (r!($lhs), r!($rhs));
                    let v = t!(int::bin($op, $iop, a, b));
                    r!($dst) = v;
                }};
            }
            // `iadd`/`isub` at `i64` without overflow: no policy needed.
            macro_rules! fast_int {
                ($op:ident, $bin:expr, $dst:expr, $lhs:expr, $rhs:expr, $iop:expr) => {{
                    let (a, b) = (r!($lhs) as i64, r!($rhs) as i64);
                    match a.$op(b) {
                        Some(v) if $iop.ty() == IntTy::I64 => r!($dst) = v as u64,
                        _ => int_bin!($bin, $dst, $lhs, $rhs, $iop),
                    }
                }};
            }
            macro_rules! fbin {
                ($ty:expr, $dst:expr, $lhs:expr, $rhs:expr, |$a:ident, $b:ident| $e:expr) => {{
                    let (x, y) = (r!($lhs), r!($rhs));
                    r!($dst) = match $ty {
                        FloatTy::F32 => {
                            let ($a, $b) = (f32_of(x), f32_of(y));
                            f32_bits($e)
                        }
                        FloatTy::F64 => {
                            let ($a, $b) = (f64::from_bits(x), f64::from_bits(y));
                            ($e).to_bits()
                        }
                    };
                }};
            }
            macro_rules! fcmp {
                ($ty:expr, $dst:expr, $lhs:expr, $rhs:expr, $cmp:expr) => {{
                    let (x, y) = (r!($lhs), r!($rhs));
                    r!($dst) = u64::from(match $ty {
                        FloatTy::F32 => $cmp.test(f32_of(x), f32_of(y)),
                        FloatTy::F64 => $cmp.test(f64::from_bits(x), f64::from_bits(y)),
                    });
                }};
            }

            match inst {
                Inst::Nop {} => {}
                Inst::Mov { dst, src } => r!(dst) = r!(src),
                Inst::LoadInt { dst, val, ty } => {
                    r!(dst) = int::normalize(ty, i64::from(val) as u64);
                }
                Inst::DLoadInt { dst, val } => {
                    r!(dst) = dynv::inline_int(i64::from(val)).unwrap_or(dynv::NIL);
                }
                Inst::LoadBool { dst, val } => r!(dst) = u64::from(val),
                Inst::LoadNil { dst } => r!(dst) = dynv::NIL,
                Inst::GetGlobal { dst, global } => r!(dst) = m.globals[global.index()],
                Inst::SetGlobal { global, src } => m.globals[global.index()] = r!(src),
                Inst::IAdd { dst, lhs, rhs, op } => {
                    fast_int!(checked_add, Bin::Add, dst, lhs, rhs, op)
                }
                Inst::ISub { dst, lhs, rhs, op } => {
                    fast_int!(checked_sub, Bin::Sub, dst, lhs, rhs, op)
                }
                Inst::IMul { dst, lhs, rhs, op } => int_bin!(Bin::Mul, dst, lhs, rhs, op),
                Inst::IDiv { dst, lhs, rhs, op } => int_bin!(Bin::Div, dst, lhs, rhs, op),
                Inst::IRem { dst, lhs, rhs, op } => int_bin!(Bin::Rem, dst, lhs, rhs, op),
                Inst::IFloorDiv { dst, lhs, rhs, op } => {
                    int_bin!(Bin::FloorDiv, dst, lhs, rhs, op);
                }
                Inst::IFloorMod { dst, lhs, rhs, op } => {
                    int_bin!(Bin::FloorMod, dst, lhs, rhs, op);
                }
                Inst::IAnd { dst, lhs, rhs, op } => int_bin!(Bin::And, dst, lhs, rhs, op),
                Inst::IOr { dst, lhs, rhs, op } => int_bin!(Bin::Or, dst, lhs, rhs, op),
                Inst::IXor { dst, lhs, rhs, op } => int_bin!(Bin::Xor, dst, lhs, rhs, op),
                Inst::IShl { dst, lhs, rhs, op } => int_bin!(Bin::Shl, dst, lhs, rhs, op),
                Inst::IShr { dst, lhs, rhs, op } => int_bin!(Bin::Shr, dst, lhs, rhs, op),
                Inst::IMin { dst, lhs, rhs, op } => int_bin!(Bin::Min, dst, lhs, rhs, op),
                Inst::IMax { dst, lhs, rhs, op } => int_bin!(Bin::Max, dst, lhs, rhs, op),
                Inst::INeg { dst, src, op } => r!(dst) = t!(int::un(Un::Neg, op, r!(src))),
                Inst::INot { dst, src, op } => r!(dst) = t!(int::un(Un::Not, op, r!(src))),
                Inst::IAbs { dst, src, op } => r!(dst) = t!(int::un(Un::Abs, op, r!(src))),
                Inst::IEq { dst, lhs, rhs, ty } => {
                    r!(dst) = u64::from(int::cmp(Cmp::Eq, ty, r!(lhs), r!(rhs)));
                }
                Inst::INe { dst, lhs, rhs, ty } => {
                    r!(dst) = u64::from(int::cmp(Cmp::Ne, ty, r!(lhs), r!(rhs)));
                }
                Inst::ILt { dst, lhs, rhs, ty } => {
                    r!(dst) = u64::from(int::cmp(Cmp::Lt, ty, r!(lhs), r!(rhs)));
                }
                Inst::ILe { dst, lhs, rhs, ty } => {
                    r!(dst) = u64::from(int::cmp(Cmp::Le, ty, r!(lhs), r!(rhs)));
                }
                Inst::IGt { dst, lhs, rhs, ty } => {
                    r!(dst) = u64::from(int::cmp(Cmp::Gt, ty, r!(lhs), r!(rhs)));
                }
                Inst::IGe { dst, lhs, rhs, ty } => {
                    r!(dst) = u64::from(int::cmp(Cmp::Ge, ty, r!(lhs), r!(rhs)));
                }
                Inst::FAdd { dst, lhs, rhs, ty } => fbin!(ty, dst, lhs, rhs, |a, b| a + b),
                Inst::FSub { dst, lhs, rhs, ty } => fbin!(ty, dst, lhs, rhs, |a, b| a - b),
                Inst::FMul { dst, lhs, rhs, ty } => fbin!(ty, dst, lhs, rhs, |a, b| a * b),
                Inst::FDiv { dst, lhs, rhs, ty } => fbin!(ty, dst, lhs, rhs, |a, b| a / b),
                Inst::FNeg { dst, src, ty } => {
                    let x = r!(src);
                    r!(dst) = match ty {
                        FloatTy::F32 => (x & 0xFFFF_FFFF) ^ SIGN32,
                        FloatTy::F64 => x ^ SIGN64,
                    };
                }
                Inst::FAbs { dst, src, ty } => {
                    let x = r!(src);
                    r!(dst) = match ty {
                        FloatTy::F32 => x & 0x7FFF_FFFF,
                        FloatTy::F64 => x & !SIGN64,
                    };
                }
                Inst::FEq { dst, lhs, rhs, ty } => fcmp!(ty, dst, lhs, rhs, Cmp::Eq),
                Inst::FNe { dst, lhs, rhs, ty } => fcmp!(ty, dst, lhs, rhs, Cmp::Ne),
                Inst::FLt { dst, lhs, rhs, ty } => fcmp!(ty, dst, lhs, rhs, Cmp::Lt),
                Inst::FLe { dst, lhs, rhs, ty } => fcmp!(ty, dst, lhs, rhs, Cmp::Le),
                Inst::FGt { dst, lhs, rhs, ty } => fcmp!(ty, dst, lhs, rhs, Cmp::Gt),
                Inst::FGe { dst, lhs, rhs, ty } => fcmp!(ty, dst, lhs, rhs, Cmp::Ge),
                Inst::BNot { dst, src } => r!(dst) = u64::from(r!(src) == 0),
                Inst::BAnd { dst, lhs, rhs } => {
                    r!(dst) = u64::from(r!(lhs) != 0 && r!(rhs) != 0);
                }
                Inst::BOr { dst, lhs, rhs } => {
                    r!(dst) = u64::from(r!(lhs) != 0 || r!(rhs) != 0);
                }
                Inst::BXor { dst, lhs, rhs } => {
                    r!(dst) = u64::from((r!(lhs) != 0) != (r!(rhs) != 0));
                }
                Inst::RefEq { dst, lhs, rhs } => r!(dst) = u64::from(r!(lhs) == r!(rhs)),
                Inst::IntCast { dst, src, conv } => r!(dst) = t!(int::int_cast(conv, r!(src))),
                Inst::Zext { dst, src, pair } => r!(dst) = int::zext(pair, r!(src)),
                Inst::Sext { dst, src, pair } => r!(dst) = int::sext(pair, r!(src)),
                Inst::Trunc { dst, src, pair } => r!(dst) = int::trunc(pair, r!(src)),
                Inst::IntToF64 { dst, src, ty } => {
                    r!(dst) = (int::value(ty, r!(src)) as f64).to_bits();
                }
                Inst::BoolToInt { dst, src, .. } => r!(dst) = u64::from(r!(src) != 0),
                Inst::Jmp { target } => jump!(target),
                Inst::JmpIf { cond, target } => {
                    if r!(cond) != 0 {
                        jump!(target);
                    }
                }
                Inst::JmpIfNot { cond, target } => {
                    if r!(cond) == 0 {
                        jump!(target);
                    }
                }
                Inst::Switch { src, table, ty } => {
                    let v = int::value(ty, r!(src));
                    let tables = module_fn.map_or(&[][..], bytecode_lang::Function::tables);
                    let jt = &tables[table.index()];
                    let target = usize::try_from(v)
                        .ok()
                        .and_then(|i| jt.targets.get(i))
                        .copied()
                        .unwrap_or(jt.default);
                    jump!(target);
                }
                Inst::Call {
                    dst,
                    func: callee,
                    argc,
                } => {
                    charge!();
                    gc!(0);
                    let cb = t!(m.push_frame(stack, prog, callee.0, dynv::NIL, Cont::Write(dst.0)));
                    let first = base + dst.index() + 1;
                    stack.copy_within(first..first + usize::from(argc), cb);
                    m.frames[fi].pc = pc as u32;
                    continue 'frame;
                }
                Inst::TailCall {
                    func: callee,
                    args,
                    argc,
                } => {
                    charge!();
                    gc!(0);
                    let nregs = prog.func(callee.0).map_or(0, |c| c.nregs);
                    if base + nregs > m.max_stack {
                        fail!(Fault::raise(ErrorKind::StackOverflow));
                    }
                    let first = base + args.index();
                    let argc = usize::from(argc);
                    stack.copy_within(first..first + argc, base);
                    stack.truncate(base + argc);
                    stack.resize(base + nregs, 0);
                    let fr = &mut m.frames[fi];
                    fr.func = callee.0;
                    fr.pc = 0;
                    fr.closure = dynv::NIL;
                    continue 'frame;
                }
                Inst::TailCallIndirect { callee, args, argc } => {
                    let cv = r!(callee);
                    let argc = usize::from(argc);
                    let first = base + args.index();
                    let target = match m.heap.get(cv) {
                        Some(Object::Func(f)) => f.target,
                        _ => fail!(not_a(&m.heap, cv)),
                    };
                    match target {
                        Callable::Func(f) => {
                            let Some(c) = prog.func(f) else {
                                fail!(Fault::type_error());
                            };
                            if c.nparams != argc {
                                fail!(Fault::type_error());
                            }
                            charge!();
                            gc!(0);
                            if base + c.nregs > m.max_stack {
                                fail!(Fault::raise(ErrorKind::StackOverflow));
                            }
                            stack.copy_within(first..first + argc, base);
                            stack.truncate(base + argc);
                            stack.resize(base + c.nregs, 0);
                            let fr = &mut m.frames[fi];
                            fr.func = f;
                            fr.pc = 0;
                            fr.closure = cv;
                            continue 'frame;
                        }
                        Callable::Import(i) => {
                            charge!();
                            let res = t!(host_window(m, stack, prog, i, first, argc));
                            let ty = prog.imports.get(i as usize).and_then(|imp| imp.result);
                            match m.finish(stack, res, ty) {
                                Ok(Next::Exit(v)) => return Ok(v),
                                Ok(Next::Resume) => continue 'frame,
                                Err(f) => {
                                    m.unwind(stack, prog, f, fuel)?;
                                    continue 'frame;
                                }
                            }
                        }
                    }
                }
                Inst::Ret { src } => {
                    let v = r!(src);
                    match m.finish(stack, Some(v), info.result) {
                        Ok(Next::Exit(v)) => return Ok(v),
                        Ok(Next::Resume) => continue 'frame,
                        Err(f) => {
                            m.unwind(stack, prog, f, fuel)?;
                            continue 'frame;
                        }
                    }
                }
                Inst::RetVoid {} => match m.finish(stack, None, None) {
                    Ok(Next::Exit(v)) => return Ok(v),
                    Ok(Next::Resume) => continue 'frame,
                    Err(f) => {
                        m.unwind(stack, prog, f, fuel)?;
                        continue 'frame;
                    }
                },
                Inst::Safepoint {} => {
                    charge!();
                    gc!(0);
                }
                Inst::CellGet { dst, cell } => {
                    let c = r!(cell);
                    match m.heap.get(c) {
                        Some(Object::Cell(x)) => r!(dst) = x.value,
                        _ => fail!(not_a(&m.heap, c)),
                    }
                }
                Inst::GetField { dst, obj, field } => {
                    let o = r!(obj);
                    match m.heap.get(o) {
                        Some(Object::Struct(s)) => match s.fields.get(field.index()) {
                            Some(&v) => r!(dst) = v,
                            None => fail!(Fault::type_error()),
                        },
                        _ => fail!(not_a(&m.heap, o)),
                    }
                }
                Inst::SetField { obj, field, src } => {
                    let (o, v) = (r!(obj), r!(src));
                    match m.heap.get_mut(o) {
                        Some(Object::Struct(s)) => match s.fields.get_mut(field.index()) {
                            Some(slot) => *slot = v,
                            None => fail!(Fault::type_error()),
                        },
                        _ => fail!(not_a(&m.heap, o)),
                    }
                }

                // Dynamic instructions: the inline-int fast path here, every
                // other case out of line.
                Inst::DAdd { dst, lhs, rhs, .. } => fast_arith!(DOp::Add, dst, lhs, rhs),
                Inst::DSub { dst, lhs, rhs, .. } => fast_arith!(DOp::Sub, dst, lhs, rhs),
                Inst::DMul { dst, lhs, rhs, .. } => fast_arith!(DOp::Mul, dst, lhs, rhs),
                Inst::DAnd { dst, lhs, rhs, .. } => fast_arith!(DOp::And, dst, lhs, rhs),
                Inst::DOr { dst, lhs, rhs, .. } => fast_arith!(DOp::Or, dst, lhs, rhs),
                Inst::DXor { dst, lhs, rhs, .. } => fast_arith!(DOp::Xor, dst, lhs, rhs),
                Inst::DLt { dst, lhs, rhs } => fast_cmp!(dst, r!(lhs), r!(rhs), false),
                Inst::DLe { dst, lhs, rhs } => fast_cmp!(dst, r!(lhs), r!(rhs), true),
                Inst::DGt { dst, lhs, rhs } => fast_cmp!(dst, r!(rhs), r!(lhs), false),
                Inst::DGe { dst, lhs, rhs } => fast_cmp!(dst, r!(rhs), r!(lhs), true),
                Inst::DEq { dst, lhs, rhs } | Inst::DNe { dst, lhs, rhs } => {
                    let (a, b) = (r!(lhs), r!(rhs));
                    if dynv::is_inline_int(a) && dynv::is_inline_int(b) {
                        // Inline ints are canonical: equal values, equal bits.
                        r!(dst) = u64::from((a == b) != matches!(inst, Inst::DNe { .. }));
                    } else {
                        cold!();
                    }
                }
                _ => cold!(),
            }
            pc += 1;
        };
        m.frames[fi].pc = pc as u32;
        m.unwind(stack, prog, fault, fuel)?;
    }
}

/// What the out-of-line handler tells the dispatch loop.
enum Step {
    /// Continue with the next instruction.
    Next,
    /// A frame was pushed (or the top frame changed): reload.
    Frame,
    /// The instruction failed.
    Fault(Fault),
}

/// The running frame's facts the out-of-line handler needs.
struct Cx<'a> {
    info: &'a FuncInfo,
    base: usize,
    closure: u64,
    fi: usize,
}

/// Every instruction not handled inline by the dispatch loop: kept out of
/// line so the loop itself stays small enough for its state to live in
/// machine registers. The semantics are identical; only the control
/// transfer differs (a [`Step`] instead of a `continue`).
#[inline(never)]
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn cold(
    m: &mut Machine,
    stack: &mut Vec<u64>,
    prog: &Program,
    cx: &Cx<'_>,
    pc: usize,
    inst: Inst,
    fuel: &mut u64,
) -> Step {
    let Cx {
        info,
        base,
        closure,
        fi,
    } = *cx;
    macro_rules! r {
        ($x:expr) => {
            stack[base + ($x).index()]
        };
    }
    macro_rules! t {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(f) => return Step::Fault(f),
            }
        };
    }
    macro_rules! fail {
        ($f:expr) => {
            return Step::Fault($f)
        };
    }
    macro_rules! charge {
        () => {
            if *fuel == 0 {
                return Step::Fault(Fault::Trap(ErrorKind::OutOfFuel));
            }
            *fuel -= 1;
        };
    }
    macro_rules! gc {
        ($extra:expr) => {
            if m.heap.wants_gc($extra) {
                m.collect(stack, prog);
            }
        };
    }
    macro_rules! fbin {
        ($ty:expr, $dst:expr, $lhs:expr, $rhs:expr, |$a:ident, $b:ident| $e:expr) => {{
            let (x, y) = (r!($lhs), r!($rhs));
            r!($dst) = match $ty {
                FloatTy::F32 => {
                    let ($a, $b) = (f32_of(x), f32_of(y));
                    f32_bits($e)
                }
                FloatTy::F64 => {
                    let ($a, $b) = (f64::from_bits(x), f64::from_bits(y));
                    ($e).to_bits()
                }
            };
        }};
    }
    macro_rules! fun {
        ($ty:expr, $dst:expr, $src:expr, |$a:ident| $e32:expr, $e64:expr) => {{
            let x = r!($src);
            r!($dst) = match $ty {
                FloatTy::F32 => {
                    let $a = f32_of(x);
                    f32_bits($e32)
                }
                FloatTy::F64 => {
                    let $a = f64::from_bits(x);
                    ($e64).to_bits()
                }
            };
        }};
    }
    // Invokes the hook bound to `$h` with `dyn` operands; `$none` runs when
    // no hook is bound. A function hook gets a frame (the dispatch loop then
    // continues in it); an import hook runs here.
    macro_rules! hook {
        ($h:expr, $args:expr, $cont:expr, $none:expr) => {{
            let args = $args;
            let which: Hook = $h;
            match prog.hooks[usize::from(which.code())] {
                None => $none,
                Some(Callee::Func(hf)) => {
                    charge!();
                    let hb = t!(m.push_frame(stack, prog, hf.0, dynv::NIL, $cont));
                    stack[hb..hb + args.len()].copy_from_slice(&args);
                    m.frames[fi].pc = pc as u32;
                    return Step::Frame;
                }
                Some(Callee::Import(hi)) => {
                    charge!();
                    let res = t!(m.call_host_dyn(prog, hi.0, &args));
                    t!(m.apply_cont(stack, $cont, res, Some(ValType::Dyn)));
                }
            }
        }};
    }
    macro_rules! darith {
        ($op:expr, $hook:expr, $dst:expr, $lhs:expr, $rhs:expr, $pol:expr) => {{
            let (a, b) = (r!($lhs), r!($rhs));
            match t!(dynops::arith(&mut m.heap, $op, $pol, a, b)) {
                Some(v) => r!($dst) = v,
                None => {
                    gc!(0);
                    hook!(
                        $hook,
                        [a, b],
                        Cont::Value($dst.0),
                        fail!(Fault::type_error())
                    )
                }
            }
        }};
    }
    match inst {
        Inst::LoadConst { dst, k } => {
            gc!(0);
            let ty = info.regs[dst.index()];
            r!(dst) = t!(m.load_const(prog, k.0, ty));
        }
        Inst::DLoadConst { dst, k } => {
            gc!(0);
            r!(dst) = t!(m.dload_const(prog, k.0));
        }
        Inst::LoadImport { dst, import } => {
            gc!(0);
            r!(dst) = t!(m.import_ref(import.0));
        }
        Inst::FRem { dst, lhs, rhs, ty } => fbin!(ty, dst, lhs, rhs, |a, b| a % b),
        Inst::FIeeeRem { dst, lhs, rhs, ty } => {
            let (x, y) = (r!(lhs), r!(rhs));
            r!(dst) = match ty {
                FloatTy::F32 => f32_bits(fmath::rem_ieee_f32(f32_of(x), f32_of(y))),
                FloatTy::F64 => fmath::rem_ieee(f64::from_bits(x), f64::from_bits(y)).to_bits(),
            };
        }
        Inst::FMin { dst, lhs, rhs, ty } => {
            fbin!(ty, dst, lhs, rhs, |a, b| fmath::fmin(a, b));
        }
        Inst::FMax { dst, lhs, rhs, ty } => {
            fbin!(ty, dst, lhs, rhs, |a, b| fmath::fmax(a, b));
        }
        Inst::FFma { dst, lhs, rhs, ty } => {
            let (x, y, z) = (r!(lhs), r!(rhs), r!(dst));
            r!(dst) = match ty {
                FloatTy::F32 => f32_bits(fmath::fma_f32(f32_of(x), f32_of(y), f32_of(z))),
                FloatTy::F64 => {
                    fmath::fma(f64::from_bits(x), f64::from_bits(y), f64::from_bits(z)).to_bits()
                }
            };
        }
        Inst::FSqrt { dst, src, ty } => {
            fun!(ty, dst, src, |a| fmath::sqrt_f32(a), fmath::sqrt(a));
        }
        Inst::FFloor { dst, src, ty } => {
            fun!(ty, dst, src, |a| fmath::floor_f32(a), fmath::floor(a));
        }
        Inst::FCeil { dst, src, ty } => {
            fun!(ty, dst, src, |a| fmath::ceil_f32(a), fmath::ceil(a));
        }
        Inst::FTrunc { dst, src, ty } => {
            fun!(ty, dst, src, |a| fmath::trunc_f32(a), fmath::trunc(a));
        }
        Inst::FRound { dst, src, ty } => {
            fun!(ty, dst, src, |a| fmath::round_f32(a), fmath::round(a));
        }
        Inst::FRoundEven { dst, src, ty } => {
            fun!(
                ty,
                dst,
                src,
                |a| fmath::round_even_f32(a),
                fmath::round_even(a)
            );
        }
        Inst::FTotalCmp { dst, lhs, rhs, ty } => {
            let (x, y) = (r!(lhs), r!(rhs));
            r!(dst) = ordering_word(match ty {
                FloatTy::F32 => f32_of(x).total_cmp(&f32_of(y)),
                FloatTy::F64 => f64::from_bits(x).total_cmp(&f64::from_bits(y)),
            });
        }
        Inst::CEq { dst, lhs, rhs } => {
            r!(dst) = u64::from(r!(lhs) as u32 == r!(rhs) as u32);
        }
        Inst::CNe { dst, lhs, rhs } => {
            r!(dst) = u64::from(r!(lhs) as u32 != r!(rhs) as u32);
        }
        Inst::CLt { dst, lhs, rhs } => {
            r!(dst) = u64::from((r!(lhs) as u32) < (r!(rhs) as u32));
        }
        Inst::CLe { dst, lhs, rhs } => {
            r!(dst) = u64::from(r!(lhs) as u32 <= r!(rhs) as u32);
        }
        Inst::CGt { dst, lhs, rhs } => {
            r!(dst) = u64::from(r!(lhs) as u32 > r!(rhs) as u32);
        }
        Inst::CGe { dst, lhs, rhs } => {
            r!(dst) = u64::from(r!(lhs) as u32 >= r!(rhs) as u32);
        }
        Inst::IntToF32 { dst, src, ty } => {
            r!(dst) = f32_bits(int::value(ty, r!(src)) as f32);
        }
        Inst::F32ToInt { dst, src, op } => {
            r!(dst) = t!(float_to_int(f64::from(f32_of(r!(src))), op));
        }
        Inst::F64ToInt { dst, src, op } => {
            r!(dst) = t!(float_to_int(f64::from_bits(r!(src)), op));
        }
        Inst::F32ToF64 { dst, src } => r!(dst) = f64::from(f32_of(r!(src))).to_bits(),
        Inst::F64ToF32 { dst, src } => {
            r!(dst) = f32_bits(f64::from_bits(r!(src)) as f32);
        }
        Inst::FloatToBits { dst, src, ty } => {
            let x = r!(src);
            let raw = if ty.bits() == 32 { x & 0xFFFF_FFFF } else { x };
            r!(dst) = int::normalize(ty, raw);
        }
        Inst::BitsToFloat { dst, src, ty } => {
            let x = r!(src);
            r!(dst) = if ty.bits() == 32 { x & 0xFFFF_FFFF } else { x };
        }
        Inst::CharFromU32 { dst, src } => {
            let v = r!(src) as u32;
            match char::from_u32(v) {
                Some(c) => r!(dst) = u64::from(c as u32),
                None => fail!(Fault::raise(ErrorKind::InvalidChar)),
            }
        }
        Inst::CharToU32 { dst, src } => r!(dst) = r!(src) & 0xFFFF_FFFF,
        Inst::DAdd { dst, lhs, rhs, pol } => {
            darith!(DOp::Add, Hook::Add, dst, lhs, rhs, pol);
        }
        Inst::DSub { dst, lhs, rhs, pol } => {
            darith!(DOp::Sub, Hook::Sub, dst, lhs, rhs, pol);
        }
        Inst::DMul { dst, lhs, rhs, pol } => {
            darith!(DOp::Mul, Hook::Mul, dst, lhs, rhs, pol);
        }
        Inst::DDiv { dst, lhs, rhs, pol } => {
            darith!(DOp::Div, Hook::Div, dst, lhs, rhs, pol);
        }
        Inst::DRem { dst, lhs, rhs, pol } => {
            darith!(DOp::Rem, Hook::Rem, dst, lhs, rhs, pol);
        }
        Inst::DFloorDiv { dst, lhs, rhs, pol } => {
            darith!(DOp::FloorDiv, Hook::FloorDiv, dst, lhs, rhs, pol);
        }
        Inst::DFloorMod { dst, lhs, rhs, pol } => {
            darith!(DOp::FloorMod, Hook::FloorMod, dst, lhs, rhs, pol);
        }
        Inst::DAnd { dst, lhs, rhs, pol } => {
            darith!(DOp::And, Hook::BitAnd, dst, lhs, rhs, pol);
        }
        Inst::DOr { dst, lhs, rhs, pol } => {
            darith!(DOp::Or, Hook::BitOr, dst, lhs, rhs, pol);
        }
        Inst::DXor { dst, lhs, rhs, pol } => {
            darith!(DOp::Xor, Hook::BitXor, dst, lhs, rhs, pol);
        }
        Inst::DShl { dst, lhs, rhs, pol } => {
            darith!(DOp::Shl, Hook::Shl, dst, lhs, rhs, pol);
        }
        Inst::DShr { dst, lhs, rhs, pol } => {
            darith!(DOp::Shr, Hook::Shr, dst, lhs, rhs, pol);
        }
        Inst::DNeg { dst, src, pol } => {
            let a = r!(src);
            match t!(dynops::neg(&mut m.heap, pol, a)) {
                Some(v) => r!(dst) = v,
                None => {
                    gc!(0);
                    hook!(
                        Hook::Neg,
                        [a],
                        Cont::Value(dst.0),
                        fail!(Fault::type_error())
                    );
                }
            }
        }
        Inst::DNot { dst, src, .. } => {
            let a = r!(src);
            match t!(dynops::not(&mut m.heap, a)) {
                Some(v) => r!(dst) = v,
                None => {
                    gc!(0);
                    hook!(
                        Hook::BitNot,
                        [a],
                        Cont::Value(dst.0),
                        fail!(Fault::type_error())
                    );
                }
            }
        }
        Inst::DEq { dst, lhs, rhs } | Inst::DNe { dst, lhs, rhs } => {
            let negate = matches!(inst, Inst::DNe { .. });
            let (a, b) = (r!(lhs), r!(rhs));
            match dynops::eq_builtin(&m.heap, a, b) {
                Some(e) => r!(dst) = u64::from(e != negate),
                None => {
                    gc!(0);
                    hook!(
                        Hook::Eq,
                        [a, b],
                        Cont::Bool { dst: dst.0, negate },
                        r!(dst) = u64::from((a == b) != negate)
                    );
                }
            }
        }
        Inst::DLt { dst, lhs, rhs }
        | Inst::DLe { dst, lhs, rhs }
        | Inst::DGt { dst, lhs, rhs }
        | Inst::DGe { dst, lhs, rhs } => {
            let le = matches!(inst, Inst::DLe { .. } | Inst::DGe { .. });
            let swap = matches!(inst, Inst::DGt { .. } | Inst::DGe { .. });
            let (mut a, mut b) = (r!(lhs), r!(rhs));
            if swap {
                core::mem::swap(&mut a, &mut b);
            }
            if dynv::is_inline_int(a) && dynv::is_inline_int(b) {
                let (x, y) = (dynv::inline_int_value(a), dynv::inline_int_value(b));
                r!(dst) = u64::from(x < y || (le && x == y));
                return Step::Next;
            }
            match dynops::lt_builtin(&m.heap, a, b, le) {
                Some(x) => r!(dst) = u64::from(x),
                None => {
                    gc!(0);
                    let h = if le { Hook::Le } else { Hook::Lt };
                    hook!(
                        h,
                        [a, b],
                        Cont::Bool {
                            dst: dst.0,
                            negate: false
                        },
                        fail!(Fault::type_error())
                    );
                }
            }
        }
        Inst::DTruthy { dst, src } | Inst::DLNot { dst, src } => {
            let negate = matches!(inst, Inst::DLNot { .. });
            let v = r!(src);
            match dynops::truthy(&m.heap, v) {
                Ok(b) => r!(dst) = u64::from(b != negate),
                Err(builtin) => {
                    gc!(0);
                    hook!(
                        Hook::Truthy,
                        [v],
                        Cont::Bool { dst: dst.0, negate },
                        r!(dst) = u64::from(builtin != negate)
                    );
                }
            }
        }
        Inst::DConcat { dst, lhs, rhs } => {
            let (a, b) = (r!(lhs), r!(rhs));
            let lens = match (m.heap.str(a), m.heap.str(b)) {
                (Some(x), Some(y)) => Some(x.len().saturating_add(y.len())),
                _ => None,
            };
            match lens {
                Some(n) => {
                    gc!(n);
                    r!(dst) = t!(concat(m, &[a, b], n));
                }
                None => {
                    gc!(0);
                    hook!(
                        Hook::Concat,
                        [a, b],
                        Cont::Value(dst.0),
                        fail!(Fault::type_error())
                    );
                }
            }
        }
        Inst::ToDyn { dst, src, from } => {
            gc!(0);
            r!(dst) = t!(conv::to_dyn(&mut m.heap, prim_type(from), r!(src)));
        }
        Inst::FromDyn { dst, src, to } => {
            r!(dst) = t!(conv::from_dyn(&m.heap, prog, prim_type(to), r!(src)));
        }
        Inst::TypeOf { dst, src } => {
            r!(dst) = u64::from(m.heap.kind(r!(src)).code());
        }
        Inst::IsKind { dst, src, kind } => {
            r!(dst) = u64::from(m.heap.kind(r!(src)) == kind);
        }
        Inst::IsType { dst, src, ty } => {
            let t = info.type_refs[ty.index()];
            r!(dst) = u64::from(conv::cast_ok(&m.heap, prog, r!(src), t));
        }
        Inst::Cast { dst, src, ty } => {
            let t = info.type_refs[ty.index()];
            let v = r!(src);
            if v == dynv::NIL || conv::cast_ok(&m.heap, prog, v, t) {
                r!(dst) = v;
            } else {
                fail!(Fault::type_error());
            }
        }
        Inst::DGetIndex { dst, obj, key } => {
            let (o, k) = (r!(obj), r!(key));
            match t!(dget_index(m, prog, o, k)) {
                Lookup::Found(v) => r!(dst) = v,
                Lookup::Miss(kind) => {
                    gc!(0);
                    hook!(
                        Hook::GetIndex,
                        [o, k],
                        Cont::Value(dst.0),
                        fail!(Fault::raise(kind))
                    );
                }
            }
        }
        Inst::DSetIndex { obj, key, src } => {
            gc!(64);
            let (o, k, v) = (r!(obj), r!(key), r!(src));
            if let Some(kind) = t!(dset_index(m, prog, o, k, v)) {
                hook!(
                    Hook::SetIndex,
                    [o, k, v],
                    Cont::Discard,
                    fail!(Fault::raise(kind))
                );
            }
        }
        Inst::GetProp { dst, obj, name } => {
            let o = r!(obj);
            let id = info.names[name.index()];
            match t!(get_prop(m, prog, o, id)) {
                Some(v) => r!(dst) = v,
                None => {
                    gc!(0);
                    let n = t!(m.name_str(prog, id));
                    hook!(
                        Hook::GetProp,
                        [o, n],
                        Cont::Value(dst.0),
                        fail!(Fault::raise(ErrorKind::UndefinedProperty))
                    );
                }
            }
        }
        Inst::SetProp { obj, name, src } => {
            gc!(64);
            let (o, v) = (r!(obj), r!(src));
            let id = info.names[name.index()];
            if !t!(set_prop(m, prog, o, id, v)) {
                let n = t!(m.name_str(prog, id));
                hook!(
                    Hook::SetProp,
                    [o, n, v],
                    Cont::Discard,
                    fail!(Fault::raise(ErrorKind::UndefinedProperty))
                );
            }
        }
        Inst::HasProp { dst, obj, name } => {
            let o = r!(obj);
            let id = info.names[name.index()];
            match has_prop(m, prog, o, id) {
                Some(b) => r!(dst) = u64::from(b),
                None => {
                    gc!(0);
                    let n = t!(m.name_str(prog, id));
                    hook!(
                        Hook::HasProp,
                        [o, n],
                        Cont::Bool {
                            dst: dst.0,
                            negate: false
                        },
                        r!(dst) = 0
                    );
                }
            }
        }
        Inst::DCall { dst, callee, argc } => {
            let cv = r!(callee);
            let argc = usize::from(argc);
            let first = base + dst.index() + 1;
            let target = match m.heap.get(cv) {
                Some(Object::Func(f)) => Some(f.target),
                _ => None,
            };
            match target {
                Some(Callable::Func(f)) => {
                    let Some(c) = prog.func(f) else {
                        fail!(Fault::type_error());
                    };
                    if c.nparams != argc {
                        fail!(Fault::type_error());
                    }
                    charge!();
                    gc!(0);
                    let mut args = core::mem::take(&mut m.scratch);
                    args.clear();
                    let mut bad = None;
                    for (i, &ty) in c.regs[..argc].iter().enumerate() {
                        match conv::from_dyn(&m.heap, prog, ty, stack[first + i]) {
                            Ok(v) => args.push(v),
                            Err(e) => {
                                bad = Some(e);
                                break;
                            }
                        }
                    }
                    if let Some(e) = bad {
                        m.scratch = args;
                        fail!(e);
                    }
                    let pushed = m.push_frame(stack, prog, f, cv, Cont::Dyn(dst.0));
                    let cb = match pushed {
                        Ok(cb) => cb,
                        Err(e) => {
                            m.scratch = args;
                            fail!(e);
                        }
                    };
                    stack[cb..cb + argc].copy_from_slice(&args);
                    m.scratch = args;
                    m.frames[fi].pc = pc as u32;
                    return Step::Frame;
                }
                Some(Callable::Import(i)) => {
                    let Some(imp) = prog.imports.get(i as usize) else {
                        fail!(Fault::type_error());
                    };
                    if imp.params.len() != argc {
                        fail!(Fault::type_error());
                    }
                    charge!();
                    m.host_args.clear();
                    for (k, &ty) in imp.params.iter().enumerate() {
                        let w = t!(conv::from_dyn(&m.heap, prog, ty, stack[first + k]));
                        let v = conv::value_of(&m.heap, ty, w);
                        m.host_args.push(v);
                    }
                    let res = t!(m.call_host(prog, i));
                    r!(dst) = match (res, imp.result) {
                        (Some(w), Some(ty)) => t!(conv::to_dyn(&mut m.heap, ty, w)),
                        _ => dynv::NIL,
                    };
                }
                None => {
                    if prog.hooks[usize::from(Hook::Call.code())].is_none() {
                        fail!(Fault::type_error());
                    }
                    gc!(argc * 8 + 64);
                    let items: Vec<u64> = stack[first..first + argc].to_vec();
                    let arr = t!(m.heap.alloc(Object::Array(ArrayObj {
                        elem: ValType::Dyn,
                        frozen: false,
                        items: alloc::sync::Arc::new(items),
                    })));
                    hook!(
                        Hook::Call,
                        [cv, arr],
                        Cont::Value(dst.0),
                        fail!(Fault::type_error())
                    );
                }
            }
        }
        Inst::DIterNew { dst, src } => {
            gc!(64);
            let v = r!(src);
            match coll::new_iter(&mut m.heap, v, true) {
                Some(it) => r!(dst) = t!(it),
                None => hook!(
                    Hook::Iter,
                    [v],
                    Cont::Iter(dst.0),
                    fail!(Fault::type_error())
                ),
            }
        }
        Inst::DLen { dst, src } => {
            let v = r!(src);
            let len = match m.heap.get(v) {
                Some(Object::Str(s)) => Some(s.len()),
                Some(Object::Array(a)) => Some(a.items.len()),
                Some(Object::Map(mm)) => Some(mm.store.len()),
                _ => None,
            };
            match len {
                Some(n) => r!(dst) = n as u64,
                None => {
                    gc!(0);
                    hook!(Hook::Len, [v], Cont::Len(dst.0), fail!(Fault::type_error()));
                }
            }
        }
        Inst::CallIndirect { dst, callee, argc } => {
            let cv = r!(callee);
            let argc = usize::from(argc);
            let first = base + dst.index() + 1;
            let target = match m.heap.get(cv) {
                Some(Object::Func(f)) => f.target,
                _ => fail!(not_a(&m.heap, cv)),
            };
            match target {
                Callable::Func(f) => {
                    if prog.func(f).map(|c| c.nparams) != Some(argc) {
                        fail!(Fault::type_error());
                    }
                    charge!();
                    gc!(0);
                    let cb = t!(m.push_frame(stack, prog, f, cv, Cont::Write(dst.0)));
                    stack.copy_within(first..first + argc, cb);
                    m.frames[fi].pc = pc as u32;
                    return Step::Frame;
                }
                Callable::Import(i) => {
                    charge!();
                    if let Some(v) = t!(host_window(m, stack, prog, i, first, argc)) {
                        r!(dst) = v;
                    }
                }
            }
        }
        Inst::CallImport { dst, import, argc } => {
            charge!();
            let first = base + dst.index() + 1;
            if let Some(v) = t!(host_window(
                m,
                stack,
                prog,
                import.0,
                first,
                usize::from(argc)
            )) {
                r!(dst) = v;
            }
        }
        Inst::Throw { src } => fail!(Fault::Throw(r!(src))),
        Inst::ErrCode { dst, src } => {
            r!(dst) = match m.heap.get(r!(src)) {
                Some(Object::Error(e)) => u64::from(e.kind.code()),
                _ => 0,
            };
        }
        Inst::Unreachable {} => fail!(Fault::Trap(ErrorKind::Unreachable)),
        Inst::MakeClosure { dst, func: target } => {
            let n = prog.func(target.0).map_or(0, |c| c.captures.len());
            gc!(n * 8);
            let first = base + dst.index() + 1;
            let captures: alloc::boxed::Box<[u64]> = stack[first..first + n].into();
            r!(dst) = t!(m.heap.alloc(Object::Func(FuncObj {
                target: Callable::Func(target.0),
                captures,
            })));
        }
        Inst::GetUpval { dst, idx } => {
            let v = match m.heap.get(closure) {
                Some(Object::Func(f)) => f.captures.get(idx.index()).copied(),
                _ => None,
            };
            match v {
                Some(v) => r!(dst) = v,
                None => fail!(Fault::type_error()),
            }
        }
        Inst::NewCell { dst, src, ty } => {
            gc!(0);
            let elem = match prog.types.get(info.type_refs[ty.index()] as usize) {
                Some(TypeInfo::Cell(e)) => *e,
                _ => ValType::Dyn,
            };
            let v = r!(src);
            r!(dst) = t!(m.heap.alloc(Object::Cell(CellObj { elem, value: v })));
        }
        Inst::CellSet { cell, src } => {
            let (c, v) = (r!(cell), r!(src));
            match m.heap.get_mut(c) {
                Some(Object::Cell(x)) => x.value = v,
                _ => fail!(not_a(&m.heap, c)),
            }
        }
        Inst::NewStruct { dst, ty } => {
            let t = info.type_refs[ty.index()];
            let n = prog.struct_info(t).map_or(0, |s| s.fields.len());
            gc!(n * 8);
            if !m.heap.fits(n.saturating_mul(8)) {
                fail!(Fault::Trap(ErrorKind::OutOfMemory));
            }
            r!(dst) = t!(m.heap.alloc(Object::Struct(StructObj {
                ty: t,
                fields: alloc::vec![0; n].into_boxed_slice(),
            })));
        }
        Inst::NewArray { dst, len, ty } => {
            let n = index_of(r!(len));
            let elem = match prog.types.get(info.type_refs[ty.index()] as usize) {
                Some(TypeInfo::Array(e)) => *e,
                _ => ValType::Dyn,
            };
            gc!(usize::try_from(n).unwrap_or(0).saturating_mul(8));
            r!(dst) = t!(coll::new_array(&mut m.heap, elem, n));
        }
        Inst::ArrayLen { dst, arr } => {
            r!(dst) = t!(coll::array_ref(&m.heap, r!(arr))).items.len() as u64;
        }
        Inst::ArrayGet { dst, arr, idx } => {
            r!(dst) = t!(coll::array_get(&m.heap, r!(arr), index_of(r!(idx))));
        }
        Inst::ArraySet { arr, idx, src } => {
            let (a, i, v) = (r!(arr), index_of(r!(idx)), r!(src));
            t!(coll::array_set(&mut m.heap, a, i, v));
        }
        Inst::ArrayPush { arr, src } => {
            gc!(64);
            let (a, v) = (r!(arr), r!(src));
            t!(coll::array_push(&mut m.heap, a, v));
        }
        Inst::ArrayPop { dst, arr } => {
            r!(dst) = t!(coll::array_pop(&mut m.heap, r!(arr)));
        }
        Inst::NewMap { dst, ty } => {
            gc!(0);
            let (k, v) = match prog.types.get(info.type_refs[ty.index()] as usize) {
                Some(TypeInfo::Map(k, v)) => (*k, *v),
                _ => (ValType::Dyn, ValType::Dyn),
            };
            r!(dst) = t!(coll::new_map(&mut m.heap, k, v));
        }
        Inst::MapLen { dst, map } => {
            r!(dst) = t!(coll::map_ref(&m.heap, r!(map))).store.len() as u64;
        }
        Inst::MapGet { dst, map, key } => {
            match t!(coll::map_get(&m.heap, m.seed, r!(map), r!(key))) {
                Some(v) => r!(dst) = v,
                None => fail!(Fault::raise(ErrorKind::KeyNotFound)),
            }
        }
        Inst::MapFind { dst, map, key } => {
            let found = t!(coll::map_get(&m.heap, m.seed, r!(map), r!(key)));
            r!(dst) = found.unwrap_or(dynv::NIL);
        }
        Inst::MapHas { dst, map, key } => {
            let found = t!(coll::map_get(&m.heap, m.seed, r!(map), r!(key)));
            r!(dst) = u64::from(found.is_some());
        }
        Inst::MapSet { map, key, src } => {
            gc!(64);
            let (mp, k, v) = (r!(map), r!(key), r!(src));
            t!(coll::map_set(&mut m.heap, m.seed, mp, k, v));
        }
        Inst::MapDel { map, key } => {
            let (mp, k) = (r!(map), r!(key));
            t!(coll::map_del(&mut m.heap, m.seed, mp, k));
        }
        Inst::MapPush { map, src } => {
            gc!(64);
            let (mp, v) = (r!(map), r!(src));
            t!(coll::map_push(&mut m.heap, m.seed, mp, v));
        }
        Inst::IterNew { dst, src } => {
            gc!(64);
            let v = r!(src);
            match coll::new_iter(&mut m.heap, v, false) {
                Some(it) => r!(dst) = t!(it),
                None => fail!(not_a(&m.heap, v)),
            }
        }
        Inst::IterNext { has, iter, val } => match t!(coll::iter_next(&mut m.heap, r!(iter))) {
            Some(v) => {
                r!(val) = v;
                r!(has) = 1;
            }
            None => r!(has) = 0,
        },
        Inst::IterKey { dst, iter } => r!(dst) = t!(coll::iter_key(&m.heap, r!(iter))),
        Inst::Dup { dst, src } => {
            gc!(64);
            r!(dst) = t!(dup(m, r!(src)));
        }
        Inst::StrLen { dst, s } => {
            let v = r!(s);
            match m.heap.str(v) {
                Some(b) => r!(dst) = b.len() as u64,
                None => fail!(not_a(&m.heap, v)),
            }
        }
        Inst::StrConcat { dst, lhs, rhs } => {
            let (a, b) = (r!(lhs), r!(rhs));
            let n = match (m.heap.str(a), m.heap.str(b)) {
                (Some(x), Some(y)) => x.len().saturating_add(y.len()),
                (None, _) => fail!(not_a(&m.heap, a)),
                (_, None) => fail!(not_a(&m.heap, b)),
            };
            gc!(n);
            r!(dst) = t!(concat(m, &[a, b], n));
        }
        Inst::StrConcatN { dst, first, count } => {
            let start = base + first.index();
            let parts = &stack[start..start + usize::from(count)];
            let mut n = 0usize;
            let mut bad = None;
            for &p in parts {
                match m.heap.str(p) {
                    Some(b) => n = n.saturating_add(b.len()),
                    None => {
                        bad = Some(p);
                        break;
                    }
                }
            }
            if let Some(p) = bad {
                fail!(not_a(&m.heap, p));
            }
            gc!(n);
            let parts: Vec<u64> = stack[start..start + usize::from(count)].to_vec();
            r!(dst) = t!(concat(m, &parts, n));
        }
        Inst::StrEq { dst, lhs, rhs } => {
            let (a, b) = (r!(lhs), r!(rhs));
            let eq = match (m.heap.str(a), m.heap.str(b)) {
                (Some(x), Some(y)) => x == y,
                (None, _) => fail!(not_a(&m.heap, a)),
                (_, None) => fail!(not_a(&m.heap, b)),
            };
            r!(dst) = u64::from(eq);
        }
        Inst::StrCmp { dst, lhs, rhs } => {
            let (a, b) = (r!(lhs), r!(rhs));
            let o = match (m.heap.str(a), m.heap.str(b)) {
                (Some(x), Some(y)) => x.cmp(y),
                (None, _) => fail!(not_a(&m.heap, a)),
                (_, None) => fail!(not_a(&m.heap, b)),
            };
            r!(dst) = ordering_word(o);
        }
        Inst::StrSlice {
            dst,
            s,
            range,
            utf8,
        } => {
            let v = r!(s);
            let start = index_of(r!(range));
            let end = index_of(stack[base + range.index() + 1]);
            let (lo, hi) = {
                let Some(b) = m.heap.str(v) else {
                    fail!(not_a(&m.heap, v));
                };
                let ok = 0 <= start && start <= end && end <= b.len() as i64;
                if !ok {
                    fail!(coll::out_of_bounds());
                }
                let (lo, hi) = (start as usize, end as usize);
                if utf8 && (is_continuation(b, lo) || is_continuation(b, hi)) {
                    fail!(Fault::raise(ErrorKind::InvalidStrIndex));
                }
                (lo, hi)
            };
            gc!(hi - lo);
            let piece: alloc::boxed::Box<[u8]> = match m.heap.str(v) {
                Some(b) => b[lo..hi].into(),
                None => fail!(not_a(&m.heap, v)),
            };
            r!(dst) = t!(m.heap.alloc(Object::Str(piece)));
        }
        Inst::StrByte { dst, s, idx } => {
            let v = r!(s);
            let i = index_of(r!(idx));
            let Some(b) = m.heap.str(v) else {
                fail!(not_a(&m.heap, v));
            };
            match usize::try_from(i).ok().and_then(|i| b.get(i)) {
                Some(&byte) => r!(dst) = u64::from(byte),
                None => fail!(coll::out_of_bounds()),
            }
        }
        Inst::CoroNew { .. }
        | Inst::CoroNewIndirect { .. }
        | Inst::Yield { .. }
        | Inst::YieldKv { .. }
        | Inst::Await { .. }
        | Inst::Resume { .. }
        | Inst::ResumeThrow { .. }
        | Inst::CoroStatus { .. }
        | Inst::CoroCurrent { .. }
        | Inst::Spawn { .. }
        | Inst::CoroClose { .. }
        | Inst::CoroKey { .. }
        | Inst::CoroResult { .. } => fail!(Fault::Unsupported(inst.opcode())),
        // Handled inline by the dispatch loop.
        _ => {}
    }
    Step::Next
}

/// Calls import `imp` with the arguments in `first..first + argc` read at the
/// import's parameter types.
fn host_window(
    m: &mut Machine,
    stack: &[u64],
    prog: &Program,
    imp: u32,
    first: usize,
    argc: usize,
) -> Result<Option<u64>, Fault> {
    let Some(info) = prog.imports.get(imp as usize) else {
        return Err(Fault::type_error());
    };
    if info.params.len() != argc {
        return Err(Fault::type_error());
    }
    m.host_args.clear();
    for (i, &ty) in info.params.iter().enumerate() {
        let w = stack.get(first + i).copied().unwrap_or(0);
        let v = conv::value_of(&m.heap, ty, w);
        m.host_args.push(v);
    }
    m.call_host(prog, imp)
}

/// Concatenates string objects into a new one of `n` bytes.
fn concat(m: &mut Machine, parts: &[u64], n: usize) -> Result<u64, Fault> {
    if !m.heap.fits(n.saturating_add(crate::heap::SLOT_BYTES)) {
        return Err(Fault::Trap(ErrorKind::OutOfMemory));
    }
    let mut out = Vec::with_capacity(n);
    for &p in parts {
        if let Some(b) = m.heap.str(p) {
            out.extend_from_slice(b);
        }
    }
    m.heap.alloc(Object::Str(out.into_boxed_slice()))
}

/// `dup` (LSB §5.10): a shallow copy with a new identity.
fn dup(m: &mut Machine, v: u64) -> Result<u64, Fault> {
    let copy = match m.heap.get(v) {
        None => {
            return if dynv::is_ref(v) || v == dynv::NIL {
                Err(coll::null())
            } else {
                // A scalar `dyn` value is its own copy.
                Ok(v)
            };
        }
        Some(Object::Array(a)) => Object::Array(ArrayObj {
            elem: a.elem,
            frozen: false,
            items: alloc::sync::Arc::clone(&a.items),
        }),
        Some(Object::Map(mm)) => Object::Map(MapObj {
            key: mm.key,
            value: mm.value,
            frozen: false,
            store: alloc::sync::Arc::clone(&mm.store),
        }),
        Some(Object::Struct(s)) => {
            if !m.heap.fits(s.fields.len().saturating_mul(8)) {
                return Err(Fault::Trap(ErrorKind::OutOfMemory));
            }
            Object::Struct(StructObj {
                ty: s.ty,
                fields: s.fields.clone(),
            })
        }
        Some(Object::Cell(c)) => Object::Cell(CellObj {
            elem: c.elem,
            value: c.value,
        }),
        Some(Object::Iter(_)) => return Err(Fault::type_error()),
        Some(Object::Str(_) | Object::Func(_) | Object::Int(_) | Object::Error(_)) => return Ok(v),
    };
    m.heap.alloc(copy)
}

/// `dget_index`'s built-in cases.
fn dget_index(m: &mut Machine, prog: &Program, o: u64, k: u64) -> Result<Lookup, Fault> {
    let key_int = conv::dyn_int(&m.heap, k);
    let (elem, word) = match m.heap.get(o) {
        Some(Object::Array(a)) => {
            let Some(i) = key_int else {
                return Ok(Lookup::Miss(ErrorKind::TypeError));
            };
            match usize::try_from(i).ok().and_then(|i| a.items.get(i)) {
                Some(&w) => (a.elem, w),
                None => return Ok(Lookup::Miss(ErrorKind::IndexOutOfBounds)),
            }
        }
        Some(Object::Map(mm)) => {
            let (kty, vty) = (mm.key, mm.value);
            let Ok(key) = conv::from_dyn(&m.heap, prog, kty, k) else {
                return Ok(Lookup::Miss(ErrorKind::KeyNotFound));
            };
            match coll::map_get(&m.heap, m.seed, o, key)? {
                Some(w) => (vty, w),
                None => return Ok(Lookup::Miss(ErrorKind::KeyNotFound)),
            }
        }
        Some(Object::Str(s)) => {
            let Some(i) = key_int else {
                return Ok(Lookup::Miss(ErrorKind::TypeError));
            };
            match usize::try_from(i).ok().and_then(|i| s.get(i)) {
                Some(&b) => (ValType::U8, u64::from(b)),
                None => return Ok(Lookup::Miss(ErrorKind::IndexOutOfBounds)),
            }
        }
        _ => return Ok(Lookup::Miss(ErrorKind::TypeError)),
    };
    Ok(Lookup::Found(conv::to_dyn(&mut m.heap, elem, word)?))
}

/// `dset_index`'s built-in cases; `Some(kind)` when the hook (or that
/// error) takes over.
fn dset_index(
    m: &mut Machine,
    prog: &Program,
    o: u64,
    k: u64,
    v: u64,
) -> Result<Option<ErrorKind>, Fault> {
    match m.heap.get(o) {
        Some(Object::Array(a)) => {
            let elem = a.elem;
            let len = a.items.len();
            let in_range = conv::dyn_int(&m.heap, k)
                .and_then(|i| usize::try_from(i).ok())
                .filter(|&i| i < len);
            let Some(i) = in_range else {
                return Ok(Some(ErrorKind::IndexOutOfBounds));
            };
            let w = conv::from_dyn(&m.heap, prog, elem, v)?;
            coll::array_set(&mut m.heap, o, i as i64, w)?;
            Ok(None)
        }
        Some(Object::Map(mm)) => {
            let (kty, vty) = (mm.key, mm.value);
            let key = conv::from_dyn(&m.heap, prog, kty, k)?;
            let w = conv::from_dyn(&m.heap, prog, vty, v)?;
            coll::map_set(&mut m.heap, m.seed, o, key, w)?;
            Ok(None)
        }
        _ => Ok(Some(ErrorKind::TypeError)),
    }
}

/// `get_prop`'s built-in cases (`None`: hook or `UndefinedProperty`).
fn get_prop(m: &mut Machine, prog: &Program, o: u64, name: u32) -> Result<Option<u64>, Fault> {
    match m.heap.get(o) {
        Some(Object::Struct(s)) => {
            let ty = s.ty;
            if let Some(info) = prog.struct_info(ty) {
                if let Ok(i) = info.field_names.binary_search_by_key(&name, |&(n, _)| n) {
                    let slot = usize::from(info.field_names[i].1);
                    let fty = info.fields.get(slot).copied().unwrap_or(ValType::Dyn);
                    let w = s.fields.get(slot).copied().unwrap_or(0);
                    return conv::to_dyn(&mut m.heap, fty, w).map(Some);
                }
            }
            match find_method(prog, ty, name) {
                Some(f) => m.fn_ref(f).map(Some),
                None => Ok(None),
            }
        }
        Some(Object::Map(mm)) => {
            let vty = mm.value;
            let text = prog.string(name).as_bytes();
            match coll::map_get_bytes(&m.heap, m.seed, o, text) {
                Some(w) => conv::to_dyn(&mut m.heap, vty, w).map(Some),
                None => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// A method by name on a struct or its ancestors (at most
/// `MAX_INHERITANCE_DEPTH` steps, checked at load).
fn find_method(prog: &Program, ty: u32, name: u32) -> Option<u32> {
    let mut cur = Some(ty);
    while let Some(t) = cur {
        let info = prog.struct_info(t)?;
        if let Ok(i) = info.methods.binary_search_by_key(&name, |&(n, _)| n) {
            return info.methods.get(i).map(|&(_, f)| f);
        }
        cur = info.parent;
    }
    None
}

/// `set_prop`'s built-in cases; `false` when the hook (or
/// `UndefinedProperty`) takes over.
fn set_prop(m: &mut Machine, prog: &Program, o: u64, name: u32, v: u64) -> Result<bool, Fault> {
    match m.heap.get(o) {
        Some(Object::Struct(s)) => {
            let Some(info) = prog.struct_info(s.ty) else {
                return Ok(false);
            };
            let Ok(i) = info.field_names.binary_search_by_key(&name, |&(n, _)| n) else {
                return Ok(false);
            };
            let slot = usize::from(info.field_names[i].1);
            let fty = info.fields.get(slot).copied().unwrap_or(ValType::Dyn);
            let w = conv::from_dyn(&m.heap, prog, fty, v)?;
            if let Some(Object::Struct(s)) = m.heap.get_mut(o) {
                if let Some(f) = s.fields.get_mut(slot) {
                    *f = w;
                }
            }
            Ok(true)
        }
        Some(Object::Map(mm)) => {
            let (kty, vty) = (mm.key, mm.value);
            if !matches!(kty, ValType::Str | ValType::Dyn) {
                return Ok(false);
            }
            let w = conv::from_dyn(&m.heap, prog, vty, v)?;
            let key = m.name_str(prog, name)?;
            coll::map_set(&mut m.heap, m.seed, o, key, w)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `has_prop`'s built-in cases (`None`: hook, else false).
fn has_prop(m: &Machine, prog: &Program, o: u64, name: u32) -> Option<bool> {
    match m.heap.get(o) {
        Some(Object::Struct(s)) => {
            let found = prog.struct_info(s.ty).is_some_and(|info| {
                info.field_names
                    .binary_search_by_key(&name, |&(n, _)| n)
                    .is_ok()
            }) || find_method(prog, s.ty, name).is_some();
            Some(found)
        }
        Some(Object::Map(_)) => {
            let text = prog.string(name).as_bytes();
            Some(coll::map_get_bytes(&m.heap, m.seed, o, text).is_some())
        }
        _ => None,
    }
}
