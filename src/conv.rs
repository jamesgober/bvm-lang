//! Conversions between typed register words, `dyn` words, and host
//! [`Value`]s, plus the `cast` test.
//!
//! These are the rules of `to_dyn`, `from_dyn`, and `cast` (LSB §5.7), which
//! every boundary reuses: `dcall` argument and result conversion, `set_prop`
//! on struct fields, dynamic indexing of typed collections, and the host
//! interface.

use bytecode_lang::{ErrorKind, Prim, ValType};

use crate::dynv::{self, Raw};
use crate::fault::Fault;
use crate::heap::{Callable, Heap, Object};
use crate::int;
use crate::program::{Program, TypeInfo};
use crate::value::{Obj, Value};

/// A number on the dynamic fast path (LSB §5.6).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Num {
    I(i64),
    F(f64),
}

/// The value type a `to_dyn`/`from_dyn` modifier names. `Prim::Ref` keeps
/// the reference word unchanged, which `Dyn` also does.
#[inline]
pub(crate) const fn prim_type(p: Prim) -> ValType {
    match p {
        Prim::Bool => ValType::Bool,
        Prim::I8 => ValType::I8,
        Prim::I16 => ValType::I16,
        Prim::I32 => ValType::I32,
        Prim::I64 => ValType::I64,
        Prim::U8 => ValType::U8,
        Prim::U16 => ValType::U16,
        Prim::U32 => ValType::U32,
        Prim::U64 => ValType::U64,
        Prim::F32 => ValType::F32,
        Prim::F64 => ValType::F64,
        Prim::Char => ValType::Char,
        Prim::Str => ValType::Str,
        Prim::Ref => ValType::Dyn,
    }
}

/// A `dyn` int word, inline or boxed.
#[inline]
pub(crate) fn encode_int(heap: &mut Heap, i: i64) -> Result<u64, Fault> {
    match dynv::inline_int(i) {
        Some(v) => Ok(v),
        None => heap.alloc(Object::Int(i)),
    }
}

/// The int a `dyn` word holds, inline or boxed.
#[inline]
pub(crate) fn dyn_int(heap: &Heap, v: u64) -> Option<i64> {
    if dynv::is_inline_int(v) {
        return Some(dynv::inline_int_value(v));
    }
    match heap.get(v)? {
        Object::Int(i) => Some(*i),
        _ => None,
    }
}

/// The number a `dyn` word holds.
#[inline]
pub(crate) fn dyn_num(heap: &Heap, v: u64) -> Option<Num> {
    if dynv::is_inline_int(v) {
        Some(Num::I(dynv::inline_int_value(v)))
    } else if dynv::is_float(v) {
        Some(Num::F(dynv::float_value(v)))
    } else {
        match heap.get(v)? {
            Object::Int(i) => Some(Num::I(*i)),
            _ => None,
        }
    }
}

/// The bool a `dyn` word holds.
#[inline]
pub(crate) fn dyn_bool(v: u64) -> Option<bool> {
    match dynv::decode(v) {
        Raw::Bool(b) => Some(b),
        _ => None,
    }
}

/// A valid scalar from char-register bits (stray bits read as U+FFFD).
#[inline]
pub(crate) fn char_of(bits: u64) -> char {
    u32::try_from(bits)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// `to_dyn` from a register of declared type `ty`.
pub(crate) fn to_dyn(heap: &mut Heap, ty: ValType, bits: u64) -> Result<u64, Fault> {
    Ok(match ty {
        ValType::Bool => dynv::from_bool(bits != 0),
        ValType::I8 | ValType::I16 | ValType::I32 | ValType::I64 => {
            let it = ty.as_int().unwrap_or(bytecode_lang::IntTy::I64);
            // Signed values of at most 64 bits fit i64.
            encode_int(heap, int::value(it, bits) as i64)?
        }
        ValType::U8 | ValType::U16 | ValType::U32 | ValType::U64 => {
            let it = ty.as_int().unwrap_or(bytecode_lang::IntTy::U64);
            match i64::try_from(int::value(it, bits)) {
                Ok(i) => encode_int(heap, i)?,
                Err(_) => return Err(Fault::raise(ErrorKind::ArithOverflow)),
            }
        }
        ValType::F32 => dynv::from_f64(f64::from(f32::from_bits(bits as u32))),
        ValType::F64 => dynv::from_f64(f64::from_bits(bits)),
        ValType::Char => dynv::from_char(char_of(bits) as u32),
        ValType::Str | ValType::Dyn | ValType::Ref(_) => bits,
    })
}

/// `from_dyn`/`cast` into a register of declared type `ty`: the kind must
/// match, ints must fit, floats narrow to `f32` by rounding, references pass
/// the `cast` test.
pub(crate) fn from_dyn(heap: &Heap, prog: &Program, ty: ValType, v: u64) -> Result<u64, Fault> {
    match ty {
        ValType::Bool => dyn_bool(v).map(u64::from).ok_or_else(Fault::type_error),
        ValType::I8
        | ValType::I16
        | ValType::I32
        | ValType::I64
        | ValType::U8
        | ValType::U16
        | ValType::U32
        | ValType::U64 => {
            let i = dyn_int(heap, v).ok_or_else(Fault::type_error)?;
            let it = ty.as_int().unwrap_or(bytecode_lang::IntTy::I64);
            int::from_value(it, i128::from(i)).ok_or(Fault::Raise(ErrorKind::ArithOverflow))
        }
        ValType::F32 => {
            if dynv::is_float(v) {
                Ok(u64::from((dynv::float_value(v) as f32).to_bits()))
            } else {
                Err(Fault::type_error())
            }
        }
        ValType::F64 => {
            if dynv::is_float(v) {
                Ok(dynv::float_value(v).to_bits())
            } else {
                Err(Fault::type_error())
            }
        }
        ValType::Char => match dynv::decode(v) {
            Raw::Char(c) => Ok(u64::from(c)),
            _ => Err(Fault::type_error()),
        },
        ValType::Str => {
            if v == dynv::NIL || heap.str(v).is_some() {
                Ok(v)
            } else {
                Err(Fault::type_error())
            }
        }
        ValType::Dyn => Ok(v),
        ValType::Ref(t) => {
            if v == dynv::NIL || cast_ok(heap, prog, v, t.0) {
                Ok(v)
            } else {
                Err(Fault::type_error())
            }
        }
    }
}

/// Whether the object `v` names is an instance of type `t` (or, for a
/// struct, of a descendant): the `cast`/`is_type` test, extended to every
/// type kind so typed and dynamic code can exchange collections.
pub(crate) fn cast_ok(heap: &Heap, prog: &Program, v: u64, t: u32) -> bool {
    let t = prog.canon_ty.get(t as usize).copied().unwrap_or(t);
    let Some(obj) = heap.get(v) else {
        return false;
    };
    match (prog.types.get(t as usize), obj) {
        (Some(TypeInfo::Struct(_)), Object::Struct(s)) => prog.descends(s.ty, t),
        (Some(TypeInfo::Array(e)), Object::Array(a)) => a.elem == *e,
        (Some(TypeInfo::Map(k, val)), Object::Map(m)) => m.key == *k && m.value == *val,
        (Some(TypeInfo::Cell(e)), Object::Cell(c)) => c.elem == *e,
        (Some(TypeInfo::Func), Object::Func(f)) => callable_sig(prog, f.target) == Some(t),
        (Some(TypeInfo::Iter(k, val)), Object::Iter(it)) => {
            iter_types(heap, it.src, it.dynamic) == Some((*k, *val))
        }
        (Some(TypeInfo::Coroutine), Object::Coro(_)) => true,
        _ => false,
    }
}

/// The canonical signature type of a callable.
pub(crate) fn callable_sig(prog: &Program, c: Callable) -> Option<u32> {
    match c {
        Callable::Func(f) => prog.func(f).map(|i| i.sig),
        Callable::Import(i) => prog.imports.get(i as usize).map(|i| i.sig),
    }
}

/// The key and value types an iterator over `src` produces.
pub(crate) fn iter_types(heap: &Heap, src: u64, dynamic: bool) -> Option<(ValType, ValType)> {
    if dynamic {
        return Some((ValType::Dyn, ValType::Dyn));
    }
    match heap.get(src)? {
        Object::Array(a) => Some((ValType::I64, a.elem)),
        Object::Map(m) => Some((m.key, m.value)),
        _ => None,
    }
}

/// A `dyn` word as a host value.
pub(crate) fn dyn_value(heap: &Heap, v: u64) -> Value {
    match dynv::decode(v) {
        Raw::Nil => Value::Nil,
        Raw::Bool(b) => Value::Bool(b),
        Raw::Int(i) => Value::Int(i),
        Raw::Float(f) => Value::Float(f),
        Raw::Char(c) => Value::Char(char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER)),
        Raw::Ref(..) => match heap.get(v) {
            None => Value::Nil,
            Some(Object::Int(i)) => Value::Int(*i),
            Some(_) => Value::Obj(Obj(v)),
        },
    }
}

/// A register word of declared type `ty` as a host value.
pub(crate) fn value_of(heap: &Heap, ty: ValType, bits: u64) -> Value {
    match ty {
        ValType::Bool => Value::Bool(bits != 0),
        ValType::I8 | ValType::I16 | ValType::I32 | ValType::I64 => {
            let it = ty.as_int().unwrap_or(bytecode_lang::IntTy::I64);
            Value::Int(int::value(it, bits) as i64)
        }
        ValType::U8 | ValType::U16 | ValType::U32 | ValType::U64 => {
            let it = ty.as_int().unwrap_or(bytecode_lang::IntTy::U64);
            Value::UInt(int::value(it, bits) as u64)
        }
        ValType::F32 => Value::F32(f32::from_bits(bits as u32)),
        ValType::F64 => Value::Float(f64::from_bits(bits)),
        ValType::Char => Value::Char(char_of(bits)),
        ValType::Str | ValType::Ref(_) | ValType::Dyn => dyn_value(heap, bits),
    }
}

/// A host value as a `dyn` word.
pub(crate) fn dyn_of(heap: &mut Heap, v: Value) -> Result<u64, Fault> {
    Ok(match v {
        Value::Nil => dynv::NIL,
        Value::Bool(b) => dynv::from_bool(b),
        Value::Int(i) => encode_int(heap, i)?,
        Value::UInt(u) => match i64::try_from(u) {
            Ok(i) => encode_int(heap, i)?,
            Err(_) => return Err(Fault::raise(ErrorKind::ArithOverflow)),
        },
        Value::F32(f) => dynv::from_f64(f64::from(f)),
        Value::Float(f) => dynv::from_f64(f),
        Value::Char(c) => dynv::from_char(c as u32),
        Value::Obj(Obj(bits)) => {
            if heap.get(bits).is_some() {
                bits
            } else {
                dynv::NIL
            }
        }
    })
}

/// A host value as a register word of declared type `ty` (`TypeError` on a
/// kind mismatch, `ArithOverflow` on an int that does not fit).
pub(crate) fn slot_of(
    heap: &mut Heap,
    prog: &Program,
    ty: ValType,
    v: Value,
) -> Result<u64, Fault> {
    match (ty, v) {
        (ValType::Bool, Value::Bool(b)) => Ok(u64::from(b)),
        (ValType::F32, Value::F32(f)) => Ok(u64::from(f.to_bits())),
        (ValType::F64, Value::Float(f)) => Ok(f.to_bits()),
        (ValType::F64, Value::F32(f)) => Ok(f64::from(f).to_bits()),
        (ValType::Char, Value::Char(c)) => Ok(u64::from(c as u32)),
        (ValType::Dyn, v) => dyn_of(heap, v),
        (ValType::Str | ValType::Ref(_), Value::Nil) => Ok(dynv::NIL),
        (ValType::Str, Value::Obj(Obj(bits))) if heap.str(bits).is_some() => Ok(bits),
        (ValType::Ref(t), Value::Obj(Obj(bits))) if cast_ok(heap, prog, bits, t.0) => Ok(bits),
        (_, Value::Int(i)) if ty.as_int().is_some() => {
            let it = ty.as_int().unwrap_or(bytecode_lang::IntTy::I64);
            int::from_value(it, i128::from(i)).ok_or(Fault::Raise(ErrorKind::ArithOverflow))
        }
        (_, Value::UInt(u)) if ty.as_int().is_some() => {
            let it = ty.as_int().unwrap_or(bytecode_lang::IntTy::U64);
            int::from_value(it, i128::from(u)).ok_or(Fault::Raise(ErrorKind::ArithOverflow))
        }
        _ => Err(Fault::type_error()),
    }
}
