//! Fuel pays for data work as well as instructions: an instruction that
//! copies, hashes, or compares a long string costs fuel in proportion to the
//! bytes, so a fixed budget bounds the bytes a module can move, not just the
//! number of instructions it runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bvm_lang::{Host, Limits, Memory, Program, Value, Vm, VmError};
use bytecode_lang::{
    Const, ConstId, ErrorKind, FuncId, FunctionBuilder, GlobalId, Inst, IntOp, IntTy, Module,
    ModuleBuilder, Reg, TypeDef, TypeId, TypeRef, ValType,
};

/// Bytes of data work one extra unit of fuel pays for.
const WORK_BYTES_PER_FUEL: u64 = 256;

/// Doublings of a one-byte string: the loop body works on 64 KiB strings.
const DOUBLINGS: u32 = 16;
const LEN: u64 = 1 << DOUBLINGS;

/// `s = "x"` doubled [`DOUBLINGS`] times into `dst`.
fn build_string(f: &mut FunctionBuilder, k: ConstId, dst: Reg) {
    f.emit(Inst::LoadConst { dst, k });
    for _ in 0..DOUBLINGS {
        f.emit(Inst::StrConcat {
            dst,
            lhs: dst,
            rhs: dst,
        });
    }
}

/// A loop that runs `body` forever and counts its iterations in a global.
/// `body` gets the two strings and a `map<str, str>` type.
fn counting_loop(
    body: impl Fn(&mut FunctionBuilder, Reg, Reg, (TypeId, TypeRef)),
) -> (Module, FuncId, GlobalId) {
    let mut m = ModuleBuilder::new();
    let sid = m.string("x");
    let k = m.constant(Const::Str(sid));
    let count = m.global("count", ValType::I64, true, None);
    let mt = m.add_type(TypeDef::Map {
        key: ValType::Str,
        value: ValType::Str,
    });
    let mut f = m.function("work", &[], &[]);
    let map_ty = f.type_ref(mt);
    let (s, u) = (f.reg(ValType::Str), f.reg(ValType::Str));
    let (n, one) = (f.reg(ValType::I64), f.reg(ValType::I64));
    build_string(&mut f, k, s);
    build_string(&mut f, k, u);
    f.emit(Inst::LoadInt {
        dst: one,
        val: 1,
        ty: IntTy::I64,
    });
    let top = f.label();
    f.bind(top);
    body(&mut f, s, u, (mt, map_ty));
    f.emit(Inst::GetGlobal {
        dst: n,
        global: count,
    });
    f.emit(Inst::IAdd {
        dst: n,
        lhs: n,
        rhs: one,
        op: IntOp::new(IntTy::I64),
    });
    f.emit(Inst::SetGlobal {
        global: count,
        src: n,
    });
    f.jmp(top);
    let id = m.add_function(f).unwrap();
    (m.finish().unwrap(), id, count)
}

/// Runs the loop under `fuel` in both memory profiles and returns the
/// iterations each completed before the fuel ran out.
fn iterations(module: Module, id: FuncId, count: GlobalId, limits: Limits) -> [u64; 2] {
    let p = Program::load(module, &Host::new()).unwrap();
    let limits = limits.with_memory(64 << 20);
    [Memory::Tracing, Memory::Rc].map(|profile| {
        let mut vm = Vm::with_memory(&p, profile);
        vm.set_limits(limits);
        let err = vm.run(id, &[]).unwrap_err();
        assert!(
            matches!(
                err,
                VmError::Trap {
                    kind: ErrorKind::OutOfFuel,
                    ..
                }
            ),
            "{err:?}"
        );
        match vm.global(count) {
            Some(Value::Int(n)) => u64::try_from(n).unwrap(),
            other => panic!("count: {other:?}"),
        }
    })
}

#[test]
fn copying_a_long_string_costs_fuel_per_byte() {
    // Each iteration copies 128 KiB into a new string.
    let (module, id, count) = counting_loop(|f, s, _, _| {
        let t = f.reg(ValType::Str);
        f.emit(Inst::StrConcat {
            dst: t,
            lhs: s,
            rhs: s,
        });
    });
    let fuel = 20_000;
    for done in iterations(module, id, count, Limits::untrusted().with_fuel(fuel)) {
        let copied = done * 2 * LEN;
        // The bytes copied are bounded by the fuel, give or take the one
        // instruction that ran it out.
        assert!(
            copied <= fuel * WORK_BYTES_PER_FUEL + 2 * LEN,
            "{done} iterations copied {copied} bytes on {fuel} fuel"
        );
    }
}

#[test]
fn comparing_long_strings_costs_fuel_per_byte() {
    // Each iteration compares two equal 64 KiB strings byte by byte.
    let (module, id, count) = counting_loop(|f, s, u, _| {
        let eq = f.reg(ValType::Bool);
        f.emit(Inst::StrEq {
            dst: eq,
            lhs: s,
            rhs: u,
        });
    });
    let fuel = 20_000;
    for done in iterations(module, id, count, Limits::untrusted().with_fuel(fuel)) {
        let compared = done * LEN;
        assert!(
            compared <= fuel * WORK_BYTES_PER_FUEL + LEN,
            "{done} iterations compared {compared} bytes on {fuel} fuel"
        );
    }
}

#[test]
fn hashing_a_long_map_key_costs_fuel_per_byte() {
    // Each iteration makes an empty map and stores one entry under a
    // 64 KiB key, which is hashed in full.
    let (module, id, count) = counting_loop(|f, s, u, (mt, ty)| {
        let map = f.reg(ValType::Ref(mt));
        f.emit(Inst::NewMap { dst: map, ty });
        f.emit(Inst::MapSet {
            map,
            key: s,
            src: u,
        });
    });
    let fuel = 20_000;
    for done in iterations(module, id, count, Limits::untrusted().with_fuel(fuel)) {
        let hashed = done * LEN;
        assert!(
            hashed <= fuel * WORK_BYTES_PER_FUEL + LEN,
            "{done} iterations hashed {hashed} bytes on {fuel} fuel"
        );
    }
}

#[test]
fn data_fuel_can_be_turned_off_for_lsb_exact_counting() {
    // Off, only LSB's charge points count: one unit per iteration (the back
    // edge), however much each iteration copies.
    let (module, id, count) = counting_loop(|f, s, _, _| {
        let t = f.reg(ValType::Str);
        f.emit(Inst::StrConcat {
            dst: t,
            lhs: s,
            rhs: s,
        });
    });
    let limits = Limits::untrusted().with_fuel(200).with_data_fuel(false);
    for done in iterations(module, id, count, limits) {
        assert_eq!(done, 201);
    }
}

#[test]
fn a_loop_without_data_work_pays_one_unit_per_iteration() {
    // The charge is for data, not instructions: an iteration that only
    // counts still costs one unit (its back edge), so this loop completes
    // close to `fuel` iterations once the strings are built.
    let (module, id, count) = counting_loop(|_, _, _, _| {});
    let fuel = 20_000;
    let build = 2 * 2 * LEN / WORK_BYTES_PER_FUEL;
    for done in iterations(module, id, count, Limits::untrusted().with_fuel(fuel)) {
        assert!(done + build + 64 >= fuel, "{done}");
        assert!(done <= fuel + 1, "{done}");
    }
}

#[test]
fn the_default_limits_are_finite() {
    for limits in [Limits::new(), Limits::default(), Limits::untrusted()] {
        assert!(limits.fuel() < u64::MAX);
        assert!(limits.memory() <= 256 << 20);
    }
    assert!(Limits::untrusted().fuel() <= Limits::new().fuel());
    assert_eq!(Limits::trusted().fuel(), u64::MAX);
    let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    assert_eq!(Vm::new(&p).limits(), Limits::new());
}

#[test]
fn a_host_supplied_hash_seed_is_taken_before_any_map_exists() {
    // put(k, v) into a fresh map and read it back, keyed by `str`.
    let mut m = ModuleBuilder::new();
    let (ks, vs) = (m.string("key"), m.string("value"));
    let (kk, kv) = (m.constant(Const::Str(ks)), m.constant(Const::Str(vs)));
    let mt = m.add_type(TypeDef::Map {
        key: ValType::Dyn,
        value: ValType::Dyn,
    });
    let mut f = m.function("roundtrip", &[], &[ValType::Dyn]);
    let map = f.reg(ValType::Dyn);
    let tr = f.type_ref(mt);
    let (k, v, out) = (
        f.reg(ValType::Dyn),
        f.reg(ValType::Dyn),
        f.reg(ValType::Dyn),
    );
    f.emit(Inst::NewMap { dst: map, ty: tr });
    f.emit(Inst::DLoadConst { dst: k, k: kk });
    f.emit(Inst::DLoadConst { dst: v, k: kv });
    f.emit(Inst::DSetIndex {
        obj: map,
        key: k,
        src: v,
    });
    f.emit(Inst::DGetIndex {
        dst: out,
        obj: map,
        key: k,
    });
    f.ret(out);
    let id = m.add_function(f).unwrap();
    let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();

    let mut vm = Vm::new(&p);
    assert!(vm.set_hash_seed(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210));
    let got = vm.run(id, &[]).unwrap();
    assert_eq!(vm.str_bytes(got), Some(&b"value"[..]));
}
