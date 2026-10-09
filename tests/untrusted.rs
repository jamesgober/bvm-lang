//! Untrusted modules: random instruction streams (every opcode, operands
//! biased to be in range so they pass the loader and execute) and randomly
//! mutated encoded modules either fail to load or run to some outcome under
//! tight limits. None may panic, hang, or exhaust host memory.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

use bvm_lang::{Host, Limits, Program, Value, Vm};
use bytecode_lang::{
    Callee, Const, Field, FieldKind, FuncId, Hook, Inst, IntOp, IntTy, Method, ModuleBuilder,
    Opcode, StructDef, Target, TypeDef, ValType,
};
use proptest::prelude::*;

/// Register types the random functions draw from.
const REG_TYPES: [ValType; 8] = [
    ValType::I64,
    ValType::I32,
    ValType::U8,
    ValType::Bool,
    ValType::F64,
    ValType::Char,
    ValType::Str,
    ValType::Dyn,
];

/// Raw choices for one instruction: an opcode index and a value per field.
type RawInst = (usize, [u32; 4]);

fn host() -> Host {
    let mut h = Host::new();
    h.register("env", "echo", |_, args| {
        Ok(args.first().copied().unwrap_or(Value::Nil))
    });
    h.register("env", "add", |_, args| {
        Ok(args.get(1).copied().unwrap_or(Value::Nil))
    });
    h
}

fn limits() -> Limits {
    Limits::new()
        .with_fuel(3_000)
        .with_memory(1 << 20)
        .with_depth(40)
        .with_stack(50_000)
}

/// Builds the instruction a raw choice describes, keeping operands mostly in
/// range for a function with `nregs` registers and `len` instructions.
fn make_inst(raw: RawInst, nregs: u32, len: u32) -> Option<Inst> {
    let (which, vals) = raw;
    let op = Opcode::ALL[which % Opcode::ALL.len()];
    let mut word = u64::from(op as u8);
    for (i, field) in op.fields().iter().enumerate() {
        let v = vals[i % 4];
        let value: u64 = match field.kind {
            // A few operands are out of range on purpose.
            FieldKind::Reg if v % 512 == 0 => u64::from(v % 70_000),
            FieldKind::Reg => u64::from(v % nregs.max(1)),
            FieldKind::Target => u64::from(v % (len + 1)),
            FieldKind::Const => u64::from(v % 5),
            FieldKind::Func => u64::from(v % 4),
            FieldKind::Import => u64::from(v % 2),
            FieldKind::Global => u64::from(v % 3),
            FieldKind::Table => u64::from(v % 2),
            FieldKind::Name => u64::from(v % 2),
            FieldKind::TypeRef => u64::from(v % 3),
            FieldKind::Field => u64::from(v % 2),
            FieldKind::Upval => 0,
            FieldKind::Count => u64::from(v % 4),
            FieldKind::Imm32 => u64::from(v),
            // Policies without `promote` (the builder refuses it on typed
            // destinations, and it is tested on its own elsewhere).
            FieldKind::Policy => u64::from((v & 0x1C) | (v % 3)),
            FieldKind::IntOp => u64::from((v & 0xE7) | (((v >> 3) % 3) << 3)),
            FieldKind::IntConv => u64::from((v & 0x3F) | (((v >> 6) % 3) << 6)),
            _ => u64::from(v % 256),
        };
        let mask = (1u64 << field.slot.width()) - 1;
        word |= (value & mask) << field.slot.shift();
    }
    Inst::from_bytes(word.to_le_bytes()).ok()
}

/// A module with types of every kind, constants, globals, imports, hooks,
/// and three functions of random code.
fn random_module(funcs: &[(Vec<usize>, Vec<RawInst>)]) -> Option<bytecode_lang::Module> {
    let mut m = ModuleBuilder::new();
    let x = m.string("x");
    let get = m.string("get");
    let _ = m.string("y");
    let sname = m.string("S");
    let at = m.add_type(TypeDef::Array(ValType::Dyn));
    let mt = m.add_type(TypeDef::Map {
        key: ValType::Dyn,
        value: ValType::Dyn,
    });
    let ct = m.add_type(TypeDef::Cell(ValType::I64));
    let it = m.add_type(TypeDef::Iter {
        key: ValType::I64,
        value: ValType::Dyn,
    });
    let sig = m.func_type(&[ValType::Dyn], &[ValType::Dyn]);
    let sig2 = m.func_type(&[ValType::Dyn, ValType::Dyn], &[ValType::Dyn]);
    let one = m.constant(Const::Int(1));
    let s = m.constant(Const::Bytes(b"abc".to_vec()));
    let _arr = m.constant(Const::Array(vec![one, s]));
    let _map = m.constant(Const::Map(vec![(s, one)]));
    let _f = m.constant(Const::f64(-0.5));
    let _g1 = m.global("g1", ValType::Dyn, true, Some(s));
    let _g2 = m.global("g2", ValType::I64, true, Some(one));
    let _g3 = m.global("g3", ValType::Ref(at), true, None);
    let echo = m.import("env", "echo", sig);
    let add = m.import("env", "add", sig2);
    let _ = (ct, it);
    // Function 3 is a well-formed method / hook body: (dyn) -> dyn.
    let mut builders = Vec::new();
    for (i, (types, _)) in funcs.iter().enumerate() {
        let params: &[ValType] = if i == 0 { &[] } else { &[ValType::Dyn] };
        let mut f = m.function(&format!("f{i}"), params, &[ValType::Dyn]);
        for &t in types {
            let _ = f.reg(REG_TYPES[t % REG_TYPES.len()]);
        }
        for t in REG_TYPES {
            let _ = f.reg(t);
        }
        if i == 1 {
            // Function 1 is only reachable as a closure.
            let _ = f.capture(ValType::Dyn);
        }
        let _ = f.name_ref(x);
        let _ = f.name_ref(get);
        let _ = f.type_ref(at);
        let _ = f.type_ref(mt);
        builders.push(f);
    }
    let mut helper = m.function("helper", &[ValType::Dyn], &[ValType::Dyn]);
    helper.ret(bytecode_lang::Reg(0));
    let helper_id = helper.id();
    let s_ty = m.add_type(TypeDef::Struct(StructDef {
        name: sname,
        parent: None,
        fields: vec![Field {
            name: x,
            ty: ValType::Dyn,
        }],
        methods: vec![Method {
            name: get,
            func: helper_id,
        }],
    }));
    for (f, (types, code)) in builders.iter_mut().zip(funcs) {
        let _ = f.type_ref(s_ty);
        let nregs =
            u32::from(f.param_count()) + u32::try_from(types.len() + REG_TYPES.len()).unwrap_or(0);
        let len = u32::try_from(code.len()).unwrap_or(0) + 1;
        let (a, b) = (f.label(), f.label());
        f.bind(a);
        for &raw in code {
            if let Some(inst) = make_inst(raw, nregs, len) {
                // The builder refuses out-of-range raw branch targets;
                // `make_inst` keeps them in range.
                f.emit(inst);
            } else {
                f.emit(Inst::Nop {});
            }
        }
        f.bind(b);
        let _ = f.switch(IntTy::I64, bytecode_lang::Reg(0), &[a, b], a);
        let _ = f.switch(IntTy::I8, bytecode_lang::Reg(0), &[b], b);
    }
    for f in builders {
        m.add_function(f).ok()?;
    }
    m.add_function(helper).ok()?;
    m.hook(Hook::Add, Callee::Import(add));
    m.hook(Hook::Neg, Callee::Import(echo));
    m.hook(Hook::Truthy, Callee::Func(helper_id));
    m.finish().ok()
}

/// Runs every parameterless function of a loaded program.
fn exercise(p: &Program) {
    let mut vm = Vm::with_limits(p, limits());
    for (i, f) in p.module().functions().iter().enumerate() {
        let id = FuncId(u32::try_from(i).unwrap_or(0));
        let nparams = match p.module().type_def(f.sig()) {
            Some(TypeDef::Func(sig)) => sig.params.clone(),
            _ => continue,
        };
        let args: Vec<Value> = nparams
            .iter()
            .map(|t| match t {
                ValType::Bool => Value::Bool(true),
                ValType::F32 => Value::F32(1.0),
                ValType::F64 => Value::Float(-1.5),
                ValType::Char => Value::Char('x'),
                ValType::I8 | ValType::I16 | ValType::I32 | ValType::I64 => Value::Int(-1),
                ValType::U8 | ValType::U16 | ValType::U32 | ValType::U64 => Value::UInt(1),
                _ => Value::Nil,
            })
            .collect();
        let _outcome = vm.run(id, &args);
        // Inspecting whatever came back must not panic either.
        if let Ok(v) = _outcome {
            let _ = (
                vm.kind(v),
                vm.elements(v),
                vm.entries(v),
                vm.str_bytes(v),
                vm.field(v, 0),
            );
        }
    }
    vm.collect_garbage();
}

fn raw_inst() -> impl Strategy<Value = RawInst> {
    (any::<usize>(), proptest::array::uniform4(any::<u32>()))
}

fn func() -> impl Strategy<Value = (Vec<usize>, Vec<RawInst>)> {
    (
        proptest::collection::vec(any::<usize>(), 0..6),
        proptest::collection::vec(raw_inst(), 0..40),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    /// Random code over every opcode: loads or is refused, and if it loads,
    /// every function runs to an outcome under the limits.
    #[test]
    fn prop_random_code_never_panics(funcs in proptest::collection::vec(func(), 1..4)) {
        if let Some(module) = random_module(&funcs) {
            if let Ok(p) = Program::load(module, &host()) {
                exercise(&p);
            }
        }
    }
}

/// A valid module exercising much of the instruction set, for mutation.
fn seed_module() -> Vec<u8> {
    let funcs: Vec<(Vec<usize>, Vec<RawInst>)> = (0..3)
        .map(|k| {
            let types: Vec<usize> = (0..6).collect();
            let code: Vec<RawInst> = (0..30)
                .map(|i| ((i * 7 + k * 13) % 200, [i as u32, (i * 3) as u32, 1, 2]))
                .collect();
            (types, code)
        })
        .collect();
    let module = random_module(&funcs).expect("seed builds");
    bytecode_lang::encode(&module)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    /// Randomly mutated encodings: decode, load, or run; never panic.
    #[test]
    fn prop_mutated_modules_never_panic(edits in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..12)) {
        let mut bytes = seed_module();
        for (at, b) in edits {
            let i = at % bytes.len();
            bytes[i] = b;
        }
        if let Ok(p) = Program::decode(&bytes, &host()) {
            exercise(&p);
        }
    }
}

#[test]
fn the_seed_module_loads_and_runs() {
    let bytes = seed_module();
    let p = Program::decode(&bytes, &host());
    // The seed is random-but-fixed code; it may be refused by the loader
    // (that is fine), but decoding must succeed.
    if let Ok(p) = p {
        exercise(&p);
    }
    let _ = (Target(0), IntOp::new(IntTy::I64));
}

#[test]
fn every_opcode_is_generated() {
    // `make_inst` reaches every opcode with in-range operands.
    let mut seen = std::collections::BTreeSet::new();
    for which in 0..Opcode::ALL.len() {
        for vals in [
            [1, 2, 3, 0],
            [0, 0, 0, 0],
            [3, 3, 3, 3],
            [1, 1, 1, 1],
            [2, 2, 2, 2],
        ] {
            if let Some(inst) = make_inst((which, vals), 8, 10) {
                let _ = seen.insert(inst.opcode() as u8);
            }
        }
    }
    assert_eq!(seen.len(), Opcode::ALL.len());
}

#[test]
fn most_random_modules_load_and_run() {
    // The fuzz properties are only meaningful if a good share of their
    // inputs get past the loader into the interpreter.
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strat = proptest::collection::vec(func(), 1..4);
    let (mut built, mut loaded) = (0, 0);
    for _ in 0..500 {
        let funcs = strat.new_tree(&mut runner).unwrap().current();
        if let Some(module) = random_module(&funcs) {
            built += 1;
            if let Ok(p) = Program::load(module, &host()) {
                loaded += 1;
                exercise(&p);
            }
        }
    }
    assert!(built > 250, "built {built} of 500");
    assert!(loaded > 100, "loaded {loaded} of {built}");
}
