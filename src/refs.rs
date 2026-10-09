//! PHP references and copy-on-write separation: `dup`, `dsep_*` (LSB
//! §5.16), and `dref_*`, `dbind_*`, `dunref_*` (§5.17).
//!
//! **Separation.** A nested write (`$a[$i][] = $v`) must not be seen through
//! another container. `dsep_*` first writes the outer container (a write to
//! a `cow` container marks every container in its contents aliased and
//! makes them its own), then replaces the element by an O(1) `dup` exactly
//! when it is aliased ([`Heap::is_aliased`](crate::heap::Heap::is_aliased)). Under the value discipline a container is
//! reachable from two places only through a contents copy or a constant, and
//! both set the bit (constant parts are frozen, which counts as aliased), so
//! a write through the result is visible through nothing else, and the loop
//! `$g[$i % 64][] = $i` copies nothing after its first pass.
//!
//! **References.** A reference slot holds a box ([`Object::Ref`]); the
//! instructions here are the only ones that store a box into a slot as a
//! box. Every other slot access goes through the transparent operations of
//! [`coll`]. A slot is addressed by [`Slot`], so one code path serves array
//! elements, map values, and struct fields.
//!
//! Each function validates everything it can before it changes anything, so
//! an instruction that raises leaves the container as it was (a copy-on-write
//! copy may already have been made; it is invisible except as identity).

use bytecode_lang::{ErrorKind, ValType};

use crate::coll::{self, not_a};
use crate::conv;
use crate::dynv;
use crate::fault::Fault;
use crate::heap::{ArrayObj, CellObj, MapObj, Object, StructObj};
use crate::machine::Machine;
use crate::program::Program;

/// A slot of a container (LSB §5.17): an array element, a map value at an
/// entry position, or a struct field.
#[derive(Clone, Copy, Debug)]
enum Slot {
    Arr(u64, usize),
    Map(u64, usize),
    Field(u64, usize),
}

/// The word a slot holds, as stored (a box stays a box).
fn read(m: &Machine, s: Slot) -> u64 {
    match s {
        Slot::Arr(o, i) => match m.heap.get(o) {
            Some(Object::Array(a)) => a.items.get(i).copied().unwrap_or(dynv::NIL),
            _ => dynv::NIL,
        },
        Slot::Map(o, p) => coll::map_word_at(&m.heap, o, p).unwrap_or(dynv::NIL),
        Slot::Field(o, f) => match m.heap.get(o) {
            Some(Object::Struct(st)) => st.fields.get(f).copied().unwrap_or(dynv::NIL),
            _ => dynv::NIL,
        },
    }
}

/// Stores `w` into a slot as it is, writing an array or map container
/// first (copy-on-write: usually the caller already did, and then this
/// copies nothing). Struct fields are never shared.
fn write(m: &mut Machine, s: Slot, w: u64) -> Result<(), Fault> {
    match s {
        Slot::Arr(o, i) => coll::array_set_raw(&mut m.heap, o, i, w),
        Slot::Map(o, p) => {
            coll::map_store_mut(&mut m.heap, o, 0)?.set_value(p, w);
            Ok(())
        }
        Slot::Field(o, f) => {
            if let Some(Object::Struct(st)) = m.heap.get_mut(o) {
                if let Some(x) = st.fields.get_mut(f) {
                    *x = w;
                }
            }
            Ok(())
        }
    }
}

/// `dup` (LSB §5.10, §5.16): a shallow copy with a new identity of an array,
/// map, struct, or cell. Arrays and maps share their contents copy-on-write
/// (O(1)); structs and cells are copied at once, and the containers in the
/// copied fields become aliased. Strings, functions, and boxed ints are
/// returned as they are; iterators, coroutines, and references raise
/// `TypeError` (a reference is not a value, §5.17).
pub(crate) fn dup(m: &mut Machine, prog: &Program, v: u64) -> Result<u64, Fault> {
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
            cow: true,
            aliased: false,
            items: alloc::sync::Arc::clone(&a.items),
        }),
        Some(Object::Map(mm)) => Object::Map(MapObj {
            key: mm.key,
            value: mm.value,
            frozen: false,
            cow: true,
            aliased: false,
            store: alloc::sync::Arc::clone(&mm.store),
        }),
        Some(Object::Struct(s)) => {
            if !m.heap.fits(s.fields.len().saturating_mul(8)) {
                return Err(Fault::Trap(ErrorKind::OutOfMemory));
            }
            let fields = s.fields.clone();
            let refs: alloc::vec::Vec<u64> = prog
                .struct_ref_fields(s.ty)
                .iter()
                .filter_map(|&f| fields.get(usize::from(f)).copied())
                .collect();
            let ty = s.ty;
            m.heap.mark_aliased(refs);
            Object::Struct(StructObj { ty, fields })
        }
        Some(Object::Cell(c)) => {
            let (elem, value) = (c.elem, c.value);
            if elem.is_reference() {
                m.heap.mark_aliased([value]);
            }
            Object::Cell(CellObj { elem, value })
        }
        // Iterators and coroutines are positions in a computation, and a
        // reference is not a value: a copy with a new identity has no
        // meaning.
        Some(Object::Iter(_) | Object::Coro(_) | Object::Ref(_)) => {
            return Err(Fault::type_error());
        }
        Some(Object::Str(_) | Object::Func(_) | Object::Int(_) | Object::Error(_)) => return Ok(v),
    };
    let shares = matches!(copy, Object::Array(_) | Object::Map(_));
    let c = m.heap.alloc(copy)?;
    if shares {
        // Both containers now share their contents (LSB §5.16 `cow`).
        match m.heap.get_mut(v) {
            Some(Object::Array(a)) => a.cow = true,
            Some(Object::Map(mm)) => mm.cow = true,
            _ => {}
        }
    }
    Ok(c)
}

/// A slot's value separated for a write in place: replaced by its `dup`
/// when it is an aliased array or map (through the box of a reference
/// slot). Returns the value to hand out.
fn separate_slot(m: &mut Machine, prog: &Program, s: Slot, ty: ValType) -> Result<u64, Fault> {
    let w = read(m, s);
    if !ty.is_reference() {
        return conv::to_dyn(&mut m.heap, ty, w);
    }
    let boxed = ty == ValType::Dyn && dynv::is_box(w);
    let e = if boxed { m.heap.deref(w) } else { w };
    if !m.heap.is_aliased(e) {
        return Ok(e);
    }
    let c = dup(m, prog, e)?;
    if boxed {
        let _ = m.heap.set_box(w, c);
    } else {
        write(m, s, c)?;
    }
    Ok(c)
}

/// `v` separated as by `dup` when it is an array or map that may be shared
/// (a reference's value given to a by-value parameter, a slot unbound by
/// `dunref_*`, a value wrapped by `dref_*`).
pub(crate) fn separated(m: &mut Machine, prog: &Program, v: u64) -> Result<u64, Fault> {
    match m.heap.get(v) {
        Some(Object::Array(_) | Object::Map(_)) => dup(m, prog, v),
        _ => Ok(v),
    }
}

/// The in-range array index a `dyn` key names, `Err(())` for a key that is
/// not an int, `Ok(None)` out of range.
fn array_index(m: &Machine, o: u64, k: u64) -> Result<Option<usize>, ()> {
    let len = match m.heap.get(o) {
        Some(Object::Array(a)) => a.items.len(),
        _ => return Ok(None),
    };
    let i = conv::dyn_int(&m.heap, k).ok_or(())?;
    Ok(usize::try_from(i).ok().filter(|&i| i < len))
}

/// The struct field named `name` (canonical string id): its slot and type.
fn field(m: &Machine, prog: &Program, o: u64, name: u32) -> Option<(usize, ValType)> {
    let Some(Object::Struct(s)) = m.heap.get(o) else {
        return None;
    };
    let info = prog.struct_info(s.ty)?;
    let i = info
        .field_names
        .binary_search_by_key(&name, |&(n, _)| n)
        .ok()?;
    let slot = usize::from(info.field_names[i].1);
    Some((slot, info.fields.get(slot).copied().unwrap_or(ValType::Dyn)))
}

/// What a container is, for the instructions that treat arrays, maps, and
/// structs differently.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Array(ValType),
    Map(ValType, ValType),
    Struct,
    Other,
}

fn shape(m: &Machine, o: u64) -> Shape {
    match m.heap.get(o) {
        Some(Object::Array(a)) => Shape::Array(a.elem),
        Some(Object::Map(mm)) => Shape::Map(mm.key, mm.value),
        Some(Object::Struct(_)) => Shape::Struct,
        _ => Shape::Other,
    }
}

/// The map entry position of the string key `name` (a map keyed by `str` or
/// `dyn`), after making the map's contents unique.
fn map_name_pos(
    m: &mut Machine,
    prog: &Program,
    o: u64,
    name: u32,
) -> Result<Option<usize>, Fault> {
    coll::unique_map(&mut m.heap, o)?;
    let key = m.name_str(prog, name)?;
    Ok(coll::map_find(&m.heap, m.seed, o, key)?.pos)
}

// ---------------------------------------------------------------------------
// Separation (LSB §5.16)
// ---------------------------------------------------------------------------

/// `dsep_index`: `Ok(None)` when `obj` is not an array or map (the caller
/// then executes `dget_index`, hook included).
pub(crate) fn dsep_index(
    m: &mut Machine,
    prog: &Program,
    o: u64,
    k: u64,
) -> Result<Option<u64>, Fault> {
    match shape(m, o) {
        Shape::Array(elem) => {
            let i = array_index(m, o, k).map_err(|()| Fault::type_error())?;
            // The container is written: its own contents become unique.
            coll::unique_array(&mut m.heap, o)?;
            match i {
                Some(i) => separate_slot(m, prog, Slot::Arr(o, i), elem).map(Some),
                None => Ok(Some(dynv::NIL)),
            }
        }
        Shape::Map(kty, vty) => {
            coll::unique_map(&mut m.heap, o)?;
            let Ok(key) = conv::from_dyn(&m.heap, prog, kty, k) else {
                return Ok(Some(dynv::NIL));
            };
            match coll::map_find(&m.heap, m.seed, o, key)?.pos {
                Some(p) => separate_slot(m, prog, Slot::Map(o, p), vty).map(Some),
                None => Ok(Some(dynv::NIL)),
            }
        }
        _ => Ok(None),
    }
}

/// `dsep_prop`: `Ok(None)` when it is exactly `get_prop` (not a `dyn` or
/// `ref` struct field, not a map keyed by strings).
pub(crate) fn dsep_prop(
    m: &mut Machine,
    prog: &Program,
    o: u64,
    name: u32,
) -> Result<Option<u64>, Fault> {
    match shape(m, o) {
        Shape::Struct => match field(m, prog, o, name) {
            Some((slot, ty @ (ValType::Dyn | ValType::Ref(_)))) => {
                separate_slot(m, prog, Slot::Field(o, slot), ty).map(Some)
            }
            _ => Ok(None),
        },
        Shape::Map(ValType::Str | ValType::Dyn, vty) => match map_name_pos(m, prog, o, name)? {
            Some(p) => separate_slot(m, prog, Slot::Map(o, p), vty).map(Some),
            None => Ok(Some(dynv::NIL)),
        },
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// References (LSB §5.17)
// ---------------------------------------------------------------------------

/// The reference a slot becomes: its box if it holds one, else a new box
/// holding its value, separated first (so the wrapped container is not
/// shared with another copy of the container's contents).
fn make_ref(m: &mut Machine, prog: &Program, s: Slot) -> Result<u64, Fault> {
    let w = read(m, s);
    if dynv::is_box(w) && m.heap.box_value(w).is_some() {
        return Ok(w);
    }
    // A stale box (never left in a slot by this VM) reads as nil.
    let w = m.heap.deref(w);
    let e = if m.heap.is_aliased(w) {
        dup(m, prog, w)?
    } else {
        w
    };
    let b = m.heap.alloc_box(e)?;
    write(m, s, b)?;
    Ok(b)
}

/// The `dyn` key of a map (`k` converted to the key type), or `TypeError`.
fn map_key(m: &Machine, prog: &Program, kty: ValType, k: u64) -> Result<u64, Fault> {
    conv::from_dyn(&m.heap, prog, kty, k).map_err(|_| Fault::type_error())
}

/// `dref_index`: a reference to `obj[key]`.
pub(crate) fn dref_index(m: &mut Machine, prog: &Program, o: u64, k: u64) -> Result<u64, Fault> {
    match shape(m, o) {
        Shape::Array(ValType::Dyn) => {
            let i = array_index(m, o, k)
                .map_err(|()| Fault::type_error())?
                .ok_or_else(coll::out_of_bounds)?;
            coll::unique_array(&mut m.heap, o)?;
            make_ref(m, prog, Slot::Arr(o, i))
        }
        Shape::Map(kty, ValType::Dyn) => {
            let key = map_key(m, prog, kty, k)?;
            map_ref_at(m, prog, o, key)
        }
        _ => Err(Fault::type_error()),
    }
}

/// The reference to the map entry `key` (already in the key type),
/// appending it holding nil when absent, as PHP creates it.
fn map_ref_at(m: &mut Machine, prog: &Program, o: u64, key: u64) -> Result<u64, Fault> {
    coll::unique_map(&mut m.heap, o)?;
    match coll::map_find(&m.heap, m.seed, o, key)?.pos {
        Some(p) => make_ref(m, prog, Slot::Map(o, p)),
        None => {
            let b = m.heap.alloc_box(dynv::NIL)?;
            coll::map_set_raw(&mut m.heap, m.seed, o, key, b)?;
            Ok(b)
        }
    }
}

/// `dref_prop`: a reference to a `dyn` struct field or a map entry keyed by
/// the name.
pub(crate) fn dref_prop(m: &mut Machine, prog: &Program, o: u64, name: u32) -> Result<u64, Fault> {
    match shape(m, o) {
        Shape::Struct => match field(m, prog, o, name) {
            Some((slot, ValType::Dyn)) => make_ref(m, prog, Slot::Field(o, slot)),
            Some(_) => Err(Fault::type_error()),
            None => Err(Fault::raise(ErrorKind::UndefinedProperty)),
        },
        Shape::Map(ValType::Str | ValType::Dyn, ValType::Dyn) => {
            let key = m.name_str(prog, name)?;
            map_ref_at(m, prog, o, key)
        }
        _ => Err(Fault::type_error()),
    }
}

/// The reference `src` names, or `TypeError`.
fn reference(m: &Machine, src: u64) -> Result<u64, Fault> {
    if dynv::is_box(src) && m.heap.box_value(src).is_some() {
        Ok(src)
    } else {
        Err(Fault::type_error())
    }
}

/// `dbind_index`: the slot `obj[key]` becomes the reference `src`.
pub(crate) fn dbind_index(
    m: &mut Machine,
    prog: &Program,
    o: u64,
    k: u64,
    src: u64,
) -> Result<(), Fault> {
    let r = reference(m, src)?;
    match shape(m, o) {
        Shape::Array(ValType::Dyn) => {
            let i = array_index(m, o, k)
                .map_err(|()| Fault::type_error())?
                .ok_or_else(coll::out_of_bounds)?;
            coll::array_set_raw(&mut m.heap, o, i, r)
        }
        Shape::Map(kty, ValType::Dyn) => {
            let key = map_key(m, prog, kty, k)?;
            coll::map_set_raw(&mut m.heap, m.seed, o, key, r)
        }
        _ => Err(Fault::type_error()),
    }
}

/// `dbind_prop`: a `dyn` struct field or a map entry becomes the reference
/// `src`.
pub(crate) fn dbind_prop(
    m: &mut Machine,
    prog: &Program,
    o: u64,
    name: u32,
    src: u64,
) -> Result<(), Fault> {
    let r = reference(m, src)?;
    match shape(m, o) {
        Shape::Struct => match field(m, prog, o, name) {
            Some((slot, ValType::Dyn)) => write(m, Slot::Field(o, slot), r),
            Some(_) => Err(Fault::type_error()),
            None => Err(Fault::raise(ErrorKind::UndefinedProperty)),
        },
        Shape::Map(ValType::Str | ValType::Dyn, ValType::Dyn) => {
            let key = m.name_str(prog, name)?;
            coll::map_set_raw(&mut m.heap, m.seed, o, key, r)
        }
        _ => Err(Fault::type_error()),
    }
}

/// Turns a reference slot back into a value slot (its value separated as by
/// `dup`); nothing for any other slot.
fn unref(m: &mut Machine, prog: &Program, s: Slot) -> Result<(), Fault> {
    let w = read(m, s);
    let Some(v) = dynv::is_box(w).then(|| m.heap.box_value(w)).flatten() else {
        return Ok(());
    };
    let v = separated(m, prog, v)?;
    // `write` writes the container (copy-on-write) before it stores.
    write(m, s, v)
}

/// `dunref_index`.
pub(crate) fn dunref_index(m: &mut Machine, prog: &Program, o: u64, k: u64) -> Result<(), Fault> {
    match shape(m, o) {
        Shape::Array(ValType::Dyn) => match array_index(m, o, k) {
            Ok(Some(i)) => unref(m, prog, Slot::Arr(o, i)),
            _ => Ok(()),
        },
        Shape::Array(_) => Ok(()),
        Shape::Map(kty, vty) => {
            let Ok(key) = conv::from_dyn(&m.heap, prog, kty, k) else {
                return Ok(());
            };
            match coll::map_find(&m.heap, m.seed, o, key)?.pos {
                Some(p) if vty == ValType::Dyn => unref(m, prog, Slot::Map(o, p)),
                _ => Ok(()),
            }
        }
        _ => Err(not_a_container()),
    }
}

/// `dunref_prop`.
pub(crate) fn dunref_prop(m: &mut Machine, prog: &Program, o: u64, name: u32) -> Result<(), Fault> {
    match shape(m, o) {
        Shape::Struct => match field(m, prog, o, name) {
            Some((slot, ValType::Dyn)) => unref(m, prog, Slot::Field(o, slot)),
            _ => Ok(()),
        },
        Shape::Map(kty, vty) => {
            if !matches!(kty, ValType::Str | ValType::Dyn) || vty != ValType::Dyn {
                return Ok(());
            }
            let key = m.name_str(prog, name)?;
            match coll::map_find(&m.heap, m.seed, o, key)?.pos {
                Some(p) => unref(m, prog, Slot::Map(o, p)),
                None => Ok(()),
            }
        }
        _ => Err(not_a_container()),
    }
}

#[cold]
fn not_a_container() -> Fault {
    Fault::type_error()
}

/// `new_ref`: a new reference holding `src` (a reference given as `src`
/// gives its value: a reference never holds a reference).
pub(crate) fn new_ref(m: &mut Machine, src: u64) -> Result<u64, Fault> {
    let v = m.heap.deref(src);
    m.heap.alloc_box(v)
}

/// `cell_set`: stores into a cell, or into a reference (a reference given
/// as the value stores its value).
#[inline]
pub(crate) fn cell_set(m: &mut Machine, c: u64, v: u64) -> Result<(), Fault> {
    let v_deref = m.heap.deref(v);
    match m.heap.get_mut(c) {
        Some(Object::Cell(x)) => {
            x.value = v;
            Ok(())
        }
        Some(Object::Ref(x)) => {
            *x = v_deref;
            Ok(())
        }
        _ => Err(not_a(&m.heap, c)),
    }
}
