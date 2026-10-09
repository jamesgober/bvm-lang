//! The loader: every structural problem the interpreter relies on being
//! absent is refused with a precise [`LoadErrorKind`], function, and pc.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::type_complexity)]

use bvm_lang::{Host, LoadErrorKind, MAX_INHERITANCE_DEPTH, Program};
use bytecode_lang::{
    Callee, Const, ConstId, Field, FuncId, Hook, Inst, IntOp, IntTy, Module, ModuleBuilder,
    Overflow, Policy, Prim, Reg, StructDef, Target, TypeDef, ValType,
};

const MARK: Inst = Inst::LoadInt {
    dst: Reg(0),
    val: 0x5EED_1234,
    ty: IntTy::I64,
};

/// Builds a module whose `main` (one i64 register) contains `MARK` then
/// `ret_void`, encodes it, replaces the marker's bytes by `inst`, and
/// decodes: the route to instructions the builder would refuse.
fn patched(inst: Inst) -> Module {
    let mut m = ModuleBuilder::new();
    let _k = m.constant(Const::Int(1));
    let mut f = m.function("main", &[], &[]);
    let _ = f.reg(ValType::I64);
    f.emit(MARK);
    f.ret_void();
    m.add_function(f).unwrap();
    let mut bytes = bytecode_lang::encode(&m.finish().unwrap());
    let mark = MARK.to_bytes();
    let at = bytes
        .windows(8)
        .position(|w| w == mark)
        .expect("marker present");
    bytes[at..at + 8].copy_from_slice(&inst.to_bytes());
    bytecode_lang::decode(&bytes).expect("still well-formed")
}

fn kind_of(module: Module) -> LoadErrorKind {
    Program::load(module, &Host::new())
        .unwrap_err()
        .kind()
        .clone()
}

#[test]
fn index_operands_out_of_range() {
    let cases = [
        (
            Inst::Mov {
                dst: Reg(1),
                src: Reg(0),
            },
            "register",
            1,
        ),
        (
            Inst::LoadConst {
                dst: Reg(0),
                k: ConstId(5),
            },
            "constant",
            5,
        ),
        (
            Inst::Call {
                dst: Reg(0),
                func: FuncId(3),
                argc: 0,
            },
            "function",
            3,
        ),
        (
            Inst::GetGlobal {
                dst: Reg(0),
                global: bytecode_lang::GlobalId(0),
            },
            "global",
            0,
        ),
        (
            Inst::CallImport {
                dst: Reg(0),
                import: bytecode_lang::ImportId(0),
                argc: 0,
            },
            "import",
            0,
        ),
        (Inst::Jmp { target: Target(2) }, "branch target", 2),
        (
            Inst::GetUpval {
                dst: Reg(0),
                idx: bytecode_lang::UpvalIdx(0),
            },
            "capture",
            0,
        ),
        (
            Inst::GetProp {
                dst: Reg(0),
                obj: Reg(0),
                name: bytecode_lang::NameRef(0),
            },
            "name",
            0,
        ),
        (
            Inst::NewStruct {
                dst: Reg(0),
                ty: bytecode_lang::TypeRef(0),
            },
            "type ref",
            0,
        ),
        (
            Inst::Switch {
                src: Reg(0),
                table: bytecode_lang::TableId(0),
                ty: IntTy::I64,
            },
            "table",
            0,
        ),
    ];
    for (inst, what, index) in cases {
        let err = Program::load(patched(inst), &Host::new()).unwrap_err();
        assert_eq!(
            err.kind(),
            &LoadErrorKind::OutOfRange { what, index },
            "{inst}"
        );
        assert_eq!((err.func(), err.pc()), (Some(FuncId(0)), Some(0)), "{inst}");
    }
}

#[test]
fn call_windows_must_fit_the_frame() {
    // dcall dst r0 with 3 arguments needs r1..r3, which do not exist.
    let kind = kind_of(patched(Inst::DCall {
        dst: Reg(0),
        callee: Reg(0),
        argc: 3,
    }));
    assert_eq!(
        kind,
        LoadErrorKind::OutOfRange {
            what: "register",
            index: 3
        }
    );
    let kind = kind_of(patched(Inst::StrConcatN {
        dst: Reg(0),
        first: Reg(0),
        count: 2,
    }));
    assert_eq!(
        kind,
        LoadErrorKind::OutOfRange {
            what: "register",
            index: 1
        }
    );
    let kind = kind_of(patched(Inst::StrSlice {
        dst: Reg(0),
        s: Reg(0),
        range: Reg(0),
        utf8: false,
    }));
    assert_eq!(
        kind,
        LoadErrorKind::OutOfRange {
            what: "register",
            index: 1
        }
    );
}

#[test]
fn modifiers_the_vm_cannot_execute() {
    let promote =
        IntOp::new(IntTy::I64).with_policy(Policy::new().with_overflow(Overflow::Promote));
    assert_eq!(
        kind_of(patched(Inst::IAdd {
            dst: Reg(0),
            lhs: Reg(0),
            rhs: Reg(0),
            op: promote
        })),
        LoadErrorKind::PromoteNotDynamic
    );
    assert_eq!(
        kind_of(patched(Inst::DAdd {
            dst: Reg(0),
            lhs: Reg(0),
            rhs: Reg(0),
            pol: promote.policy()
        })),
        LoadErrorKind::PromoteNotDynamic,
        "promote into an i64 register"
    );
    assert_eq!(
        kind_of(patched(Inst::FromDyn {
            dst: Reg(0),
            src: Reg(0),
            to: Prim::Ref
        })),
        LoadErrorKind::BadModifier
    );
    assert_eq!(
        kind_of(patched(Inst::FloatToBits {
            dst: Reg(0),
            src: Reg(0),
            ty: IntTy::I16
        })),
        LoadErrorKind::BadModifier
    );
}

#[test]
fn shape_problems() {
    // Falls through.
    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[], &[]);
    f.emit(Inst::Nop {});
    m.add_function(f).unwrap();
    assert_eq!(kind_of(m.finish().unwrap()), LoadErrorKind::FallsThrough);
    // Empty.
    let mut m = ModuleBuilder::new();
    let f = m.function("f", &[], &[]);
    m.add_function(f).unwrap();
    assert_eq!(kind_of(m.finish().unwrap()), LoadErrorKind::EmptyCode);
    // Two results.
    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[], &[ValType::I64, ValType::I64]);
    f.ret_void();
    m.add_function(f).unwrap();
    assert_eq!(kind_of(m.finish().unwrap()), LoadErrorKind::BadSignature);
}

#[test]
fn call_rules() {
    // Arity.
    let mut m = ModuleBuilder::new();
    let mut callee = m.function("callee", &[ValType::I64], &[]);
    callee.ret_void();
    let cid = callee.id();
    m.add_function(callee).unwrap();
    let mut main = m.function("main", &[], &[]);
    let w = main.regs(&[ValType::I64, ValType::I64]);
    main.emit(Inst::Call {
        dst: w,
        func: cid,
        argc: 0,
    });
    main.ret_void();
    m.add_function(main).unwrap();
    assert_eq!(
        kind_of(m.finish().unwrap()),
        LoadErrorKind::ArityMismatch {
            expected: 1,
            found: 0
        }
    );
    // A function with captures cannot be called directly.
    let mut m = ModuleBuilder::new();
    let mut clo = m.function("clo", &[], &[]);
    let _ = clo.capture(ValType::I64);
    clo.ret_void();
    let cid = clo.id();
    m.add_function(clo).unwrap();
    let mut main = m.function("main", &[], &[]);
    let w = main.reg(ValType::Dyn);
    main.emit(Inst::Call {
        dst: w,
        func: cid,
        argc: 0,
    });
    main.ret_void();
    m.add_function(main).unwrap();
    assert_eq!(
        kind_of(m.finish().unwrap()),
        LoadErrorKind::CalleeHasCaptures
    );
}

#[test]
fn handler_rules() {
    // Catch register must be dyn.
    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[], &[]);
    let e = f.reg(ValType::I64);
    let (s, end) = (f.label(), f.label());
    f.bind(s);
    f.emit(Inst::Nop {});
    f.bind(end);
    f.ret_void();
    f.try_region(s, end, s, e);
    m.add_function(f).unwrap();
    assert_eq!(kind_of(m.finish().unwrap()), LoadErrorKind::CatchNotDyn);
    // No tail call inside a try region.
    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[], &[]);
    let me = f.id();
    let e = f.reg(ValType::Dyn);
    let (s, end, h) = (f.label(), f.label(), f.label());
    f.bind(s);
    f.emit(Inst::TailCall {
        func: me,
        args: Reg(0),
        argc: 0,
    });
    f.bind(end);
    f.bind(h);
    f.ret_void();
    f.try_region(s, end, h, e);
    m.add_function(f).unwrap();
    assert_eq!(kind_of(m.finish().unwrap()), LoadErrorKind::TailCallInTry);
}

#[test]
fn type_operand_kinds() {
    let mut m = ModuleBuilder::new();
    let at = m.add_type(TypeDef::Array(ValType::I64));
    let mut f = m.function("f", &[], &[]);
    let tr = f.type_ref(at);
    let d = f.reg(ValType::Dyn);
    f.emit(Inst::NewStruct { dst: d, ty: tr });
    f.ret_void();
    m.add_function(f).unwrap();
    assert_eq!(
        kind_of(m.finish().unwrap()),
        LoadErrorKind::WrongTypeKind {
            expected: "a struct"
        }
    );
}

#[test]
fn struct_inheritance_rules() {
    // Parent fields must prefix the child's.
    let mut m = ModuleBuilder::new();
    let (x, y, n) = (m.string("x"), m.string("y"), m.string("S"));
    let parent = m.add_type(TypeDef::Struct(StructDef {
        name: n,
        parent: None,
        fields: vec![Field {
            name: x,
            ty: ValType::I64,
        }],
        methods: Vec::new(),
    }));
    let _child = m.add_type(TypeDef::Struct(StructDef {
        name: n,
        parent: Some(parent),
        fields: vec![Field {
            name: y,
            ty: ValType::I64,
        }],
        methods: Vec::new(),
    }));
    assert_eq!(kind_of(m.finish().unwrap()), LoadErrorKind::BadParent);
    // A cycle.
    let mut m = ModuleBuilder::new();
    let n = m.string("S");
    let t = m.reserve_type();
    m.define_type(
        t,
        TypeDef::Struct(StructDef {
            name: n,
            parent: Some(t),
            fields: Vec::new(),
            methods: Vec::new(),
        }),
    );
    assert_eq!(
        kind_of(m.finish().unwrap()),
        LoadErrorKind::InheritanceTooDeep
    );
    // Deeper than the cap.
    let mut m = ModuleBuilder::new();
    let n = m.string("S");
    let mut parent = None;
    for _ in 0..=MAX_INHERITANCE_DEPTH {
        parent = Some(m.add_type(TypeDef::Struct(StructDef {
            name: n,
            parent,
            fields: Vec::new(),
            methods: Vec::new(),
        })));
    }
    assert_eq!(
        kind_of(m.finish().unwrap()),
        LoadErrorKind::InheritanceTooDeep
    );
}

#[test]
fn constants_nested_too_deep() {
    let mut m = ModuleBuilder::new();
    let mut k = m.constant(Const::Int(0));
    for _ in 0..=bvm_lang::MAX_CONST_DEPTH {
        k = m.constant(Const::Array(vec![k]));
    }
    let _ = k;
    assert_eq!(kind_of(m.finish().unwrap()), LoadErrorKind::BadConstant);
}

#[test]
fn hooks_start_and_imports() {
    // A two-parameter function bound to the unary `neg` hook.
    let mut m = ModuleBuilder::new();
    let mut h = m.function("h", &[ValType::Dyn, ValType::Dyn], &[ValType::Dyn]);
    h.ret(Reg(0));
    let hid = h.id();
    m.add_function(h).unwrap();
    m.hook(Hook::Neg, Callee::Func(hid));
    assert_eq!(
        kind_of(m.finish().unwrap()),
        LoadErrorKind::BadHook(Hook::Neg)
    );
    // Start must be () -> ().
    let mut m = ModuleBuilder::new();
    let mut s = m.function("s", &[ValType::I64], &[]);
    s.ret_void();
    let sid = m.add_function(s).unwrap();
    m.set_start(sid);
    assert_eq!(kind_of(m.finish().unwrap()), LoadErrorKind::BadStart);
    // Imports must be registered.
    let mut m = ModuleBuilder::new();
    let sig = m.func_type(&[], &[]);
    let _ = m.import("env", "absent", sig);
    assert_eq!(
        kind_of(m.finish().unwrap()),
        LoadErrorKind::UnresolvedImport {
            module: "env".into(),
            name: "absent".into()
        }
    );
}

#[test]
fn malformed_bytes_are_a_decode_error() {
    let err = Program::decode(&[0x4C, 0x53, 0x42, 0x00, 9, 0, 0, 0], &Host::new()).unwrap_err();
    assert!(matches!(err.kind(), LoadErrorKind::Decode(_)));
    assert!(err.to_string().starts_with("decode error"));
}

#[test]
fn a_valid_module_round_trips_through_bytes() {
    let mut m = ModuleBuilder::new();
    let mut f = m.function("f", &[], &[ValType::I64]);
    let r = f.reg(ValType::I64);
    f.emit(Inst::LoadInt {
        dst: r,
        val: 3,
        ty: IntTy::I64,
    });
    f.ret(r);
    m.add_function(f).unwrap();
    let bytes = bytecode_lang::encode(&m.finish().unwrap());
    let p = Program::decode(&bytes, &Host::new()).unwrap();
    assert_eq!(
        bvm_lang::Vm::new(&p).run(FuncId(0), &[]),
        Ok(bvm_lang::Value::Int(3))
    );
}
