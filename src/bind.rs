//! Dynamic calls (LSB §5.15): flattening a call site's window into argument
//! items, binding the items to the callee's parameter list, and building the
//! callee's arguments from the binding.
//!
//! **The binder.** Binding follows `bytecode_lang::ParamList::bind`, the
//! family's one statement of PHP's and Python's argument rules, and property
//! tests compare both binders here with it, slot for slot and presence bit
//! for presence bit. Calls whose items are all positional (every `dcall`,
//! and every `dcall_shape` without named arguments) take
//! [`bind_positional`], which allocates nothing for a callee without a rest
//! parameter. Calls with named items take [`bind_named`], which finds each
//! named parameter through the callee's hashed name index when its list has
//! many ([`NameIndex`]), so a hostile named spread costs O(items), not
//! O(items x parameters) (ISSUES P33). The caller charges fuel per item for
//! large calls ([`item_units`]).
//!
//! **Order.** Flattening (whose errors are `TypeError` for a spread of a
//! non-container and `ArgumentError` for a spread map key that is neither
//! `int` nor `str`) and binding (every failure is `ArgumentError`) happen
//! before the fuel charge; converting the bound arguments (by-reference
//! boxes, rest collections, by-value conversions that may raise `TypeError`)
//! happens after it, as §5.15 steps 1 to 5 require. Nothing here collects
//! garbage: the caller runs the collector before flattening, so the boxed
//! ints a spread of a typed array may allocate stay alive until the
//! arguments are in the callee's frame.

use alloc::boxed::Box;
use alloc::vec::Vec;

use bytecode_lang::{ArgItem, ArgKind, Bound, CallShape, ErrorKind, ParamKind, ParamList, ValType};

use crate::coll;
use crate::conv;
use crate::dynv;
use crate::fault::Fault;
use crate::heap::{Callable, Object};
use crate::machine::Machine;
use crate::program::{NameIndex, Program, takes_names};
use crate::refs;

/// Items of a dynamic call per extra unit of fuel (ISSUES P33): a call with
/// `n` flattened items costs `n / ITEMS_PER_FUEL` units on top of its one
/// unit, so the work of binding stays proportional to the fuel spent.
/// Calls of fewer items (every call whose arguments are a window, which
/// has at most 255) cost exactly what LSB §5.14 says.
pub(crate) const ITEMS_PER_FUEL: usize = 64;

/// The extra fuel units a dynamic call of `n` flattened items costs.
#[inline]
pub(crate) fn item_units(n: usize) -> u64 {
    (n / ITEMS_PER_FUEL) as u64
}

/// The name of one flattened argument item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Name {
    /// A positional item.
    Pos,
    /// A named item whose name is a string of the module (a call shape's).
    Module(u32),
    /// A named item from a spread map's `str` key (copied, so the heap may
    /// change while the binding is built).
    Bytes(Box<[u8]>),
}

/// The flattened items of a call (§5.15 step 2): their values and names.
#[derive(Debug, Default)]
pub(crate) struct Flat {
    pub(crate) vals: Vec<u64>,
    pub(crate) names: Vec<Name>,
    /// Whether any item is named.
    pub(crate) named: bool,
}

impl Flat {
    pub(crate) fn clear(&mut self) {
        self.vals.clear();
        self.names.clear();
        self.named = false;
    }

    fn push(&mut self, v: u64, name: Name) {
        self.named |= name != Name::Pos;
        self.vals.push(v);
        self.names.push(name);
    }
}

/// `ArgumentError` (E0114).
#[cold]
pub(crate) fn argument_error() -> Fault {
    Fault::raise(ErrorKind::ArgumentError)
}

/// The parameter list of a function or import, if it has one.
pub(crate) fn params_of(prog: &Program, target: Callable) -> Option<&ParamList> {
    match target {
        Callable::Func(f) => prog
            .module
            .function(bytecode_lang::FuncId(f))
            .and_then(bytecode_lang::Function::params),
        Callable::Import(i) => prog
            .module
            .import(bytecode_lang::ImportId(i))
            .and_then(|imp| imp.params.as_ref()),
    }
}

/// The callable a word names, if it is a function value.
#[inline]
pub(crate) fn target_of(m: &Machine, v: u64) -> Option<Callable> {
    match m.heap.get(v) {
        Some(Object::Func(f)) => Some(f.target),
        _ => None,
    }
}

/// The hashed name index of a callable's parameter list, if it has one.
pub(crate) fn index_of(prog: &Program, target: Callable) -> Option<&NameIndex> {
    match target {
        Callable::Func(f) => prog.func(f).and_then(|i| i.param_index.as_ref()),
        Callable::Import(i) => prog
            .imports
            .get(i as usize)
            .and_then(|imp| imp.param_index.as_ref()),
    }
}

/// The signature parameter types of a callable.
pub(crate) fn sig_params(prog: &Program, target: Callable) -> &[ValType] {
    match target {
        Callable::Func(f) => prog.func(f).map_or(&[], |c| &c.regs[..c.nparams]),
        Callable::Import(i) => prog.imports.get(i as usize).map_or(&[], |imp| &imp.params),
    }
}

/// Flattens the window of a `dcall_shape` into items (§5.15 step 2): spread
/// arrays give their elements, spread maps their entries (positional for an
/// `int` key, named for a `str` key), named spreads their `str`-keyed
/// entries. Spread elements are slot reads, so a reference slot gives its
/// value.
pub(crate) fn flatten(
    m: &mut Machine,
    shape: &CallShape,
    window: &[u64],
    out: &mut Flat,
) -> Result<(), Fault> {
    out.clear();
    for (arg, &w) in shape.args.iter().zip(window) {
        match *arg {
            ArgKind::Positional => out.push(w, Name::Pos),
            ArgKind::Named(s) => out.push(w, Name::Module(s.0)),
            ArgKind::Spread => spread(m, w, false, out)?,
            ArgKind::SpreadNamed => spread(m, w, true, out)?,
        }
    }
    Ok(())
}

/// One spread (`named`: a named spread, which takes only `str` keys).
fn spread(m: &mut Machine, w: u64, named: bool, out: &mut Flat) -> Result<(), Fault> {
    match m.heap.get(w) {
        Some(Object::Array(a)) if !named => {
            let elem = a.elem;
            let items: Vec<u64> = a.items.iter().copied().collect();
            for v in items {
                let v = if elem == ValType::Dyn {
                    m.heap.deref(v)
                } else {
                    v
                };
                let d = conv::to_dyn(&mut m.heap, elem, v)?;
                out.push(d, Name::Pos);
            }
            Ok(())
        }
        Some(Object::Map(mm)) => {
            let (kty, vty) = (mm.key, mm.value);
            let entries: Vec<(u64, u64)> = mm
                .store
                .entries()
                .iter()
                .filter(|e| e.live)
                .map(|e| (e.key, e.value))
                .collect();
            for (k, v) in entries {
                let name = if let Some(bytes) = key_str(m, kty, k) {
                    Name::Bytes(bytes)
                } else if !named && key_is_int(m, kty, k) {
                    Name::Pos
                } else {
                    return Err(argument_error());
                };
                let v = if vty == ValType::Dyn {
                    m.heap.deref(v)
                } else {
                    v
                };
                let d = conv::to_dyn(&mut m.heap, vty, v)?;
                out.push(d, name);
            }
            Ok(())
        }
        _ => Err(Fault::type_error()),
    }
}

/// A map key's bytes when it is a string.
fn key_str(m: &Machine, kty: ValType, k: u64) -> Option<Box<[u8]>> {
    match kty {
        ValType::Str | ValType::Dyn => m.heap.str(k).map(|b| {
            m.heap.note_work(b.len());
            Box::from(b)
        }),
        _ => None,
    }
}

/// Whether a map key is an integer.
fn key_is_int(m: &Machine, kty: ValType, k: u64) -> bool {
    match kty {
        ValType::Dyn => conv::dyn_int(&m.heap, k).is_some(),
        other => other.as_int().is_some(),
    }
}

/// Binds `n` positional items to `list` (the fast path of §5.15 step 3):
/// the same slots and presence mask as `ParamList::bind` with `n`
/// `ArgItem::Positional`s, or `Err(())` exactly when it fails.
pub(crate) fn bind_positional(list: &ParamList, n: usize, out: &mut Vec<Bound>) -> Result<u64, ()> {
    out.clear();
    let mut item = 0usize;
    let mut rest = None;
    for (i, p) in list.params.iter().enumerate() {
        out.push(match p.kind {
            ParamKind::PositionalOnly | ParamKind::Normal if item < n => {
                item += 1;
                Bound::Arg(item - 1)
            }
            ParamKind::Rest | ParamKind::RestMap => {
                rest = Some(i);
                Bound::Collected(Vec::new())
            }
            ParamKind::RestNamed => Bound::Collected(Vec::new()),
            _ => Bound::Default,
        });
    }
    if item < n {
        match rest.and_then(|r| out.get_mut(r)) {
            Some(slot) => *slot = Bound::Collected((item..n).collect()),
            None if list.ignore_extra => {}
            None => return Err(()),
        }
    }
    presence(list, out)
}

/// The presence mask of a binding (§5.15 step 3d), or `Err(())` for a
/// parameter left empty without a default.
fn presence(list: &ParamList, slots: &[Bound]) -> Result<u64, ()> {
    let mut mask = 0u64;
    for (i, (slot, p)) in slots.iter().zip(&list.params).enumerate() {
        let present = match slot {
            Bound::Arg(_) => true,
            Bound::Collected(items) => !items.is_empty(),
            Bound::Default if p.default => false,
            Bound::Default => return Err(()),
        };
        if present && i < 64 {
            mask |= 1 << i;
        }
    }
    Ok(mask)
}

/// Binds items to a callee (§5.15 step 3): `list` is its parameter list
/// (`None`: exact arity over `nparams`, positional only) and `index` its
/// hashed name index. Returns the presence mask; `out` receives one slot
/// per parameter of the list (per signature parameter without a list).
pub(crate) fn bind(
    prog: &Program,
    (list, index): (Option<&ParamList>, Option<&NameIndex>),
    nparams: usize,
    flat: Option<&Flat>,
    n: usize,
    out: &mut Vec<Bound>,
) -> Result<u64, Fault> {
    let named = flat.is_some_and(|f| f.named);
    match (list, named) {
        (None, false) => {
            if n != nparams {
                return Err(argument_error());
            }
            out.clear();
            out.extend((0..n).map(Bound::Arg));
            Ok(0)
        }
        (None, true) => Err(argument_error()),
        (Some(list), false) => bind_positional(list, n, out).map_err(|()| argument_error()),
        (Some(list), true) => {
            let names = flat.map_or(&[][..], |f| &f.names[..]);
            let items = names.iter().map(|name| match name {
                Name::Pos => ArgItem::Positional,
                Name::Module(s) => ArgItem::Named(prog.string(*s).as_bytes()),
                Name::Bytes(b) => ArgItem::Named(b),
            });
            bind_named(&prog.module, list, index, items, out).map_err(|()| argument_error())
        }
    }
}

/// Binds items, named ones included, to `list` (§5.15 step 3, the rule of
/// `ParamList::bind`): the same slots and presence mask, or `Err(())`
/// exactly when it fails. Parameter names are strings of `module`; with an
/// `index` each named item costs one hash and a binary search, without one
/// (a list of at most [`crate::program::NAME_SCAN`] named parameters) a
/// scan of the list. Names repeated within a rest collection are found
/// with an ordered set, O(log n) each.
pub(crate) fn bind_named<'a>(
    module: &bytecode_lang::Module,
    list: &ParamList,
    index: Option<&NameIndex>,
    items: impl Iterator<Item = ArgItem<'a>>,
    out: &mut Vec<Bound>,
) -> Result<u64, ()> {
    out.clear();
    out.extend(list.params.iter().map(|p| {
        if p.kind.is_rest() {
            Bound::Collected(Vec::new())
        } else {
            Bound::Default
        }
    }));
    let rest = list
        .params
        .iter()
        .position(|p| matches!(p.kind, ParamKind::Rest | ParamKind::RestMap));
    let named_rest = list
        .params
        .iter()
        .position(|p| p.kind == ParamKind::RestNamed)
        .or_else(|| {
            list.params
                .iter()
                .position(|p| p.kind == ParamKind::RestMap)
        });
    let find = |name: &[u8]| match index {
        Some(ix) => ix.find(module, list, name),
        None => list.params.iter().position(|p| {
            takes_names(p.kind)
                && p.name
                    .and_then(|s| module.string(s))
                    .is_some_and(|s| s.as_bytes() == name)
        }),
    };
    // The next parameter that may take a positional item: it only moves
    // forward, so finding all of them is O(parameters) in total.
    let mut cursor = 0usize;
    let mut seen_named = false;
    let mut rest_names: alloc::collections::BTreeSet<&[u8]> = alloc::collections::BTreeSet::new();
    for (item, arg) in items.enumerate() {
        match arg {
            ArgItem::Positional => {
                if seen_named {
                    return Err(());
                }
                while list.params.get(cursor).is_some_and(|p| {
                    !matches!(p.kind, ParamKind::PositionalOnly | ParamKind::Normal)
                }) {
                    cursor += 1;
                }
                if let Some(slot) = out.get_mut(cursor) {
                    cursor += 1;
                    *slot = Bound::Arg(item);
                } else if let Some(Bound::Collected(items)) = rest.and_then(|r| out.get_mut(r)) {
                    items.push(item);
                } else if !list.ignore_extra {
                    return Err(());
                }
            }
            ArgItem::Named(name) => {
                seen_named = true;
                if let Some(param) = find(name) {
                    match out.get_mut(param) {
                        Some(slot @ Bound::Default) => *slot = Bound::Arg(item),
                        _ => return Err(()),
                    }
                } else if let Some(Bound::Collected(items)) =
                    named_rest.and_then(|r| out.get_mut(r))
                {
                    if !rest_names.insert(name) {
                        return Err(());
                    }
                    items.push(item);
                } else {
                    return Err(());
                }
            }
        }
    }
    presence(list, out)
}

/// A by-value argument (§5.15 step 5): a reference gives its value,
/// separated as by `dup` when an array or map, then the value converts to
/// the parameter's type by `from_dyn`/`cast` rules.
fn by_value(m: &mut Machine, prog: &Program, ty: ValType, w: u64) -> Result<u64, Fault> {
    let v = if dynv::is_box(w) {
        let inner = m.heap.deref(w);
        refs::separated(m, prog, inner)?
    } else {
        w
    };
    conv::from_dyn(&m.heap, prog, ty, v)
}

/// A by-reference argument: a reference as it is, anything else in a new
/// reference (a temporary).
fn reference_arg(m: &mut Machine, w: u64) -> Result<u64, Fault> {
    if dynv::is_box(w) && m.heap.box_value(w).is_some() {
        Ok(w)
    } else {
        let v = m.heap.deref(w);
        m.heap.alloc_box(v)
    }
}

/// One item of a rest collection, by the rest parameter's `by_ref`.
fn rest_item(m: &mut Machine, prog: &Program, by_ref: bool, w: u64) -> Result<u64, Fault> {
    if by_ref {
        reference_arg(m, w)
    } else {
        by_value(m, prog, ValType::Dyn, w)
    }
}

/// Builds the callee's arguments from a binding (§5.15 step 5), in
/// signature order, the presence mask last when the list has defaults.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_args(
    m: &mut Machine,
    prog: &Program,
    list: Option<&ParamList>,
    sig: &[ValType],
    vals: &[u64],
    flat: Option<&Flat>,
    bound: &[Bound],
    mask: u64,
    args: &mut Vec<u64>,
) -> Result<(), Fault> {
    args.clear();
    let Some(list) = list else {
        for (i, &ty) in sig.iter().enumerate() {
            let w = vals.get(i).copied().unwrap_or(dynv::NIL);
            let v = by_value(m, prog, ty, w)?;
            args.push(v);
        }
        return Ok(());
    };
    for (i, (p, slot)) in list.params.iter().zip(bound).enumerate() {
        let ty = sig.get(i).copied().unwrap_or(ValType::Dyn);
        let v = match slot {
            Bound::Arg(k) => {
                let w = vals.get(*k).copied().unwrap_or(dynv::NIL);
                if p.by_ref {
                    reference_arg(m, w)?
                } else {
                    by_value(m, prog, ty, w)?
                }
            }
            Bound::Default => 0,
            Bound::Collected(items) => collect(m, prog, p.kind, p.by_ref, vals, flat, items)?,
        };
        args.push(v);
    }
    if list.has_defaults() {
        args.push(mask);
    }
    Ok(())
}

/// A rest parameter's collection: a `dyn` array for `rest`, a `dyn` map for
/// `rest_map` (positional items under the next integer key, named ones
/// under their names) and `rest_named`.
fn collect(
    m: &mut Machine,
    prog: &Program,
    kind: ParamKind,
    by_ref: bool,
    vals: &[u64],
    flat: Option<&Flat>,
    items: &[usize],
) -> Result<u64, Fault> {
    if items.is_empty() {
        // An empty rest collection shares the machine's empty storage and
        // allocates storage of its own only when first written (P33).
        return m.empty_rest(kind == ParamKind::Rest);
    }
    if kind == ParamKind::Rest {
        let mut words = Vec::with_capacity(items.len());
        for &k in items {
            let w = vals.get(k).copied().unwrap_or(dynv::NIL);
            words.push(rest_item(m, prog, by_ref, w)?);
        }
        return coll::new_dyn_array(&mut m.heap, words);
    }
    let map = coll::new_map(&mut m.heap, ValType::Dyn, ValType::Dyn)?;
    for &k in items {
        let w = vals.get(k).copied().unwrap_or(dynv::NIL);
        let v = rest_item(m, prog, by_ref, w)?;
        let name = flat.and_then(|f| f.names.get(k));
        match name {
            None | Some(Name::Pos) => coll::map_push_raw(&mut m.heap, m.seed, map, v)?,
            Some(Name::Module(s)) => {
                let key = m.heap.alloc_str(prog.string(*s).as_bytes())?;
                coll::map_set_raw(&mut m.heap, m.seed, map, key, v)?;
            }
            Some(Name::Bytes(b)) => {
                let key = m.heap.alloc_str(b)?;
                coll::map_set_raw(&mut m.heap, m.seed, map, key, v)?;
            }
        }
    }
    Ok(map)
}

/// The named items of a flattened call as a `dyn` map in item order, and
/// the positional ones as a `dyn` array (the `call_shape` hook's operands,
/// §5.15 "Non-callables"); a repeated name is `ArgumentError`.
pub(crate) fn hook_operands(
    m: &mut Machine,
    prog: &Program,
    flat: &Flat,
) -> Result<(u64, u64), Fault> {
    let positional: Vec<u64> = flat
        .vals
        .iter()
        .zip(&flat.names)
        .filter(|(_, n)| **n == Name::Pos)
        .map(|(&v, _)| m.heap.deref(v))
        .collect();
    let arr = coll::new_dyn_array(&mut m.heap, positional)?;
    let map = coll::new_map(&mut m.heap, ValType::Dyn, ValType::Dyn)?;
    for (&v, name) in flat.vals.iter().zip(&flat.names) {
        let key = match name {
            Name::Pos => continue,
            Name::Module(s) => m.heap.alloc_str(prog.string(*s).as_bytes())?,
            Name::Bytes(b) => m.heap.alloc_str(b)?,
        };
        let pos = coll::map_find(&m.heap, m.seed, map, key)?.pos;
        if pos.is_some() {
            return Err(argument_error());
        }
        let v = m.heap.deref(v);
        coll::map_set_raw(&mut m.heap, m.seed, map, key, v)?;
    }
    Ok((arr, map))
}

/// `dparam_ref`: whether a positional argument at `pos` would bind by
/// reference.
pub(crate) fn param_ref(prog: &Program, target: Option<Callable>, pos: i64) -> bool {
    let Ok(pos) = u64::try_from(pos) else {
        return false;
    };
    target
        .and_then(|t| params_of(prog, t))
        .is_some_and(|list| list.positional_by_ref(pos))
}

/// `dparam_ref_named`: whether a named argument would bind by reference.
pub(crate) fn param_ref_named(prog: &Program, target: Option<Callable>, name: &[u8]) -> bool {
    target
        .and_then(|t| params_of(prog, t))
        .is_some_and(|list| list.named_by_ref(&prog.module, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytecode_lang::{ModuleBuilder, Param};
    use proptest::prelude::*;

    /// A valid list from counts: positional-only, normal, an optional
    /// positional rest (`rest` or `rest_map`), named-only, an optional
    /// `rest_named`; flags per entry.
    fn list_of(
        counts: (usize, usize, u8, usize, bool),
        flags: &[(bool, bool)],
        names: &[bytecode_lang::StrId],
    ) -> ParamList {
        let (po, normal, rest, named, rest_named) = counts;
        let mut kinds = Vec::new();
        kinds.extend(core::iter::repeat_n(ParamKind::PositionalOnly, po));
        kinds.extend(core::iter::repeat_n(ParamKind::Normal, normal));
        match rest {
            1 => kinds.push(ParamKind::Rest),
            2 => kinds.push(ParamKind::RestMap),
            _ => {}
        }
        kinds.extend(core::iter::repeat_n(ParamKind::NamedOnly, named));
        if rest_named {
            kinds.push(ParamKind::RestNamed);
        }
        let params = kinds
            .into_iter()
            .enumerate()
            .map(|(i, kind)| {
                let (by_ref, def) = flags.get(i).copied().unwrap_or((false, false));
                let mut p = Param::new(kind, Some(names[i]));
                p.by_ref = by_ref;
                p.default = def && !kind.is_rest();
                p
            })
            .collect();
        ParamList::new(params)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4000))]
        /// The named binder (P33: hashed lookup, ordered-set duplicate
        /// check) binds exactly as `ParamList::bind` does, slot for slot and
        /// presence bit for presence bit, failing exactly when it fails,
        /// with the hashed index and with a scan, on lists of up to 26
        /// parameters. Calls are built from the list (a positional prefix,
        /// then some named parameters in a rotated order) with noise: extra
        /// positional items, unknown names, a repeated name, a positional
        /// item after a named one.
        #[test]
        fn named_binder_matches_param_list_bind(
            counts in (0usize..3, 0usize..12, 0u8..3, 0usize..12, any::<bool>()),
            flags in proptest::collection::vec((any::<bool>(), any::<bool>()), 32),
            ignore in any::<bool>(),
            plan in proptest::collection::vec(0u8..4, 32),
            (k, rot, extra_pos, extra_named) in (0usize..16, 0usize..32, 0usize..3, 0usize..3),
            noise in 0u8..8,
        ) {
            let mut mb = ModuleBuilder::new();
            let names: Vec<_> = (0..32).map(|i| mb.string(&alloc::format!("p{i}"))).collect();
            let module = mb.finish().unwrap_or_default();
            let mut list = list_of(counts, &flags, &names);
            list.ignore_extra = ignore;
            prop_assert!(list.validate().is_ok());
            let texts: Vec<alloc::string::String> =
                (0..32).map(|i| alloc::format!("p{i}")).collect();
            let unknown: Vec<alloc::string::String> =
                (0..3).map(|i| alloc::format!("q{i}")).collect();
            let positional = list
                .params
                .iter()
                .take_while(|p| matches!(p.kind, ParamKind::PositionalOnly | ParamKind::Normal))
                .count();
            // Positional-only parameters can only be given positionally.
            let only = list
                .params
                .iter()
                .take_while(|p| p.kind == ParamKind::PositionalOnly)
                .count();
            let k = only + k % (positional - only + 1);
            let mut items: Vec<ArgItem<'_>> = alloc::vec![ArgItem::Positional; k + extra_pos];
            let mut named: Vec<ArgItem<'_>> = list
                .params
                .iter()
                .enumerate()
                .skip(k)
                .filter(|(i, p)| takes_names(p.kind) && (plan[*i] != 0 || !p.default))
                .map(|(i, _)| ArgItem::Named(texts[i].as_bytes()))
                .collect();
            named.extend(unknown.iter().take(extra_named).map(|u| ArgItem::Named(u.as_bytes())));
            if !named.is_empty() {
                let r = rot % named.len();
                named.rotate_left(r);
            }
            match noise {
                0 => {
                    if let Some(&first) = named.first() {
                        named.push(first);
                    }
                }
                1 => named.push(ArgItem::Positional),
                _ => {}
            }
            items.extend(named);
            let want = list.bind(&module, &items);
            let index = crate::program::NameIndex::build(&module, &list);
            for ix in [None, Some(&index)] {
                let mut out = Vec::new();
                let got = bind_named(&module, &list, ix, items.iter().copied(), &mut out);
                match &want {
                    Ok(b) => {
                        prop_assert_eq!(got, Ok(b.presence()));
                        prop_assert_eq!(&out[..], b.slots());
                    }
                    Err(_) => prop_assert!(got.is_err()),
                }
            }
        }

        /// The positional fast path binds exactly as `ParamList::bind` does.
        #[test]
        fn positional_fast_path_matches_param_list_bind(
            counts in (0usize..3, 0usize..4, 0u8..3, 0usize..3, any::<bool>()),
            flags in proptest::collection::vec((any::<bool>(), any::<bool>()), 12),
            ignore in any::<bool>(),
            n in 0usize..12,
        ) {
            let mut mb = ModuleBuilder::new();
            let names: Vec<_> = (0..12).map(|i| mb.string(&alloc::format!("p{i}"))).collect();
            let module = mb.finish().unwrap_or_default();
            let mut list = list_of(counts, &flags, &names);
            list.ignore_extra = ignore;
            prop_assert!(list.validate().is_ok());
            let items = alloc::vec![ArgItem::Positional; n];
            let want = list.bind(&module, &items);
            let mut out = Vec::new();
            let got = bind_positional(&list, n, &mut out);
            match want {
                Ok(b) => {
                    prop_assert_eq!(got, Ok(b.presence()));
                    prop_assert_eq!(&out[..], b.slots());
                }
                Err(_) => prop_assert!(got.is_err()),
            }
        }
    }
}
