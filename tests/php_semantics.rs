//! PHP-semantics conformance: small PHP programs, lowered by hand the way a
//! PHP code generator (bcgen-lang) lowers them to LSB format 2, with the
//! result PHP 8.3 gives. Each test names the PHP source it mirrors.
//!
//! Covered: value semantics of arrays with copy-on-write and separation
//! (LSB §5.16), references in variables, array slots, properties, and
//! `foreach` (§5.17), dynamic calls with parameter lists, named arguments,
//! variadics, by-reference parameters decided at run time, and host
//! functions as values (§5.15), the `pow`/`abs`/shift arithmetic of OPS v2,
//! generator keys (§5.13 rule 10), and `match` without an arm (`NoMatch`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use bvm_lang::{Host, HostError, Program, Value, Vm, VmError};
use bytecode_lang::{
    ArgKind, Const, ErrorKind, FuncId, FunctionBuilder, ImportId, Inst, ModuleBuilder, Overflow,
    Param, ParamKind, ParamList, Policy, Reg, Shift, StrId, TypeDef, TypeRef, ValType,
};

const D: ValType = ValType::Dyn;

/// A function under construction with PHP-shaped helpers: every helper
/// returns a fresh `dyn` register.
struct P<'a> {
    f: FunctionBuilder,
    m: &'a mut ModuleBuilder,
    arr: TypeRef,
    map: TypeRef,
}

impl<'a> P<'a> {
    fn new(m: &'a mut ModuleBuilder, name: &str, params: &[ValType]) -> P<'a> {
        let at = m.add_type(TypeDef::Array(D));
        let mt = m.add_type(TypeDef::Map { key: D, value: D });
        let mut f = m.function(name, params, &[D]);
        let (arr, map) = (f.type_ref(at), f.type_ref(mt));
        P { f, m, arr, map }
    }
    fn reg(&mut self) -> Reg {
        self.f.reg(D)
    }
    fn int(&mut self, v: i32) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::DLoadInt { dst: r, val: v });
        r
    }
    fn big(&mut self, v: i64) -> Reg {
        let k = self.m.constant(Const::Int(v));
        let r = self.reg();
        self.f.emit(Inst::DLoadConst { dst: r, k });
        r
    }
    fn float(&mut self, v: f64) -> Reg {
        let k = self.m.constant(Const::f64(v));
        let r = self.reg();
        self.f.emit(Inst::DLoadConst { dst: r, k });
        r
    }
    fn str(&mut self, s: &str) -> Reg {
        let k = self.m.constant(Const::Bytes(s.as_bytes().to_vec()));
        let r = self.reg();
        self.f.emit(Inst::DLoadConst { dst: r, k });
        r
    }
    /// `[a, b, ...]` as a fresh array.
    fn array(&mut self, items: &[Reg]) -> Reg {
        let (n, r) = (self.f.reg(ValType::I64), self.reg());
        self.f.emit(Inst::LoadInt {
            dst: n,
            val: 0,
            ty: bytecode_lang::IntTy::I64,
        });
        let ty = self.arr;
        self.f.emit(Inst::NewArray { dst: r, len: n, ty });
        for &x in items {
            self.f.emit(Inst::ArrayPush { arr: r, src: x });
        }
        r
    }
    /// `[]` as a PHP array (an ordered map).
    fn map(&mut self) -> Reg {
        let r = self.reg();
        let ty = self.map;
        self.f.emit(Inst::NewMap { dst: r, ty });
        r
    }
    fn get(&mut self, o: Reg, k: Reg) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::DGetIndex {
            dst: r,
            obj: o,
            key: k,
        });
        r
    }
    fn set(&mut self, o: Reg, k: Reg, v: Reg) {
        self.f.emit(Inst::DSetIndex {
            obj: o,
            key: k,
            src: v,
        });
    }
    fn push(&mut self, o: Reg, v: Reg) {
        self.f.emit(Inst::MapPush { map: o, src: v });
    }
    fn dup(&mut self, s: Reg) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::Dup { dst: r, src: s });
        r
    }
    fn sep(&mut self, o: Reg, k: Reg) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::DSepIndex {
            dst: r,
            obj: o,
            key: k,
        });
        r
    }
    fn refi(&mut self, o: Reg, k: Reg) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::DRefIndex {
            dst: r,
            obj: o,
            key: k,
        });
        r
    }
    fn new_ref(&mut self, v: Reg) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::NewRef { dst: r, src: v });
        r
    }
    fn deref(&mut self, c: Reg) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::CellGet { dst: r, cell: c });
        r
    }
    fn assign(&mut self, c: Reg, v: Reg) {
        self.f.emit(Inst::CellSet { cell: c, src: v });
    }
    fn mul(&mut self, a: Reg, b: Reg) -> Reg {
        let r = self.reg();
        let pol = Policy::new().with_overflow(Overflow::Promote);
        self.f.emit(Inst::DMul {
            dst: r,
            lhs: a,
            rhs: b,
            pol,
        });
        r
    }
    fn func_value(&mut self, f: FuncId) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::MakeClosure { dst: r, func: f });
        r
    }
    fn import_value(&mut self, i: ImportId) -> Reg {
        let r = self.reg();
        self.f.emit(Inst::LoadImport { dst: r, import: i });
        r
    }
    /// A call window: `dst` then the arguments (moved in).
    fn window(&mut self, args: &[Reg]) -> Reg {
        let w = self.f.regs(&vec![D; args.len() + 1]);
        for (i, &a) in args.iter().enumerate() {
            self.f.mov(Reg(w.0 + 1 + i as u16), a);
        }
        w
    }
    fn dcall(&mut self, callee: Reg, args: &[Reg]) -> Reg {
        let w = self.window(args);
        self.f.emit(Inst::DCall {
            dst: w,
            callee,
            argc: args.len() as u8,
        });
        w
    }
    fn dcall_shape(&mut self, callee: Reg, args: &[(ArgKind, Reg)]) -> Reg {
        let regs: Vec<Reg> = args.iter().map(|a| a.1).collect();
        let kinds: Vec<ArgKind> = args.iter().map(|a| a.0).collect();
        let w = self.window(&regs);
        self.f.dcall_shape(w, callee, &kinds);
        w
    }
    fn ret(mut self, r: Reg) -> FuncId {
        self.f.ret(r);
        self.m.add_function(self.f).unwrap()
    }
}

/// PHP-like rendering of a value, for comparing whole results.
fn show(vm: &Vm<'_>, v: Value) -> String {
    match v {
        Value::Nil => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::F32(f) => format!("{f:?}"),
        Value::Char(c) => format!("{c:?}"),
        Value::Obj(_) => {
            if let Some(b) = vm.str_bytes(v) {
                return format!("{:?}", String::from_utf8_lossy(b));
            }
            if let Some(items) = vm.elements(v) {
                let parts: Vec<String> = items.into_iter().map(|x| show(vm, x)).collect();
                return format!("[{}]", parts.join(", "));
            }
            if let Some(entries) = vm.entries(v) {
                let parts: Vec<String> = entries
                    .into_iter()
                    .map(|(k, x)| format!("{} => {}", show(vm, k), show(vm, x)))
                    .collect();
                return format!("[{}]", parts.join(", "));
            }
            if let Some(x) = vm.ref_value(v) {
                return format!("&{}", show(vm, x));
            }
            format!("{:?}", vm.kind(v))
        }
        other => format!("{other:?}"),
    }
}

fn run(m: ModuleBuilder, host: &Host, main: FuncId) -> String {
    let p = Program::load(m.finish().unwrap(), host).unwrap();
    let mut vm = Vm::new(&p);
    match vm.run(main, &[]) {
        Ok(v) => show(&vm, v),
        Err(e) => format!("error {e}"),
    }
}

// ---------------------------------------------------------------------------
// Value semantics and separation (LSB §5.16)
// ---------------------------------------------------------------------------

#[test]
fn nested_write_after_copy_does_not_leak_into_the_copy() {
    // $a = [[1]]; $b = $a; $a[0][] = 2; return [$a, $b];
    // => [[[1, 2]], [[1]]]
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let one = p.int(1);
    let inner = p.map();
    p.push(inner, one);
    let a = p.map();
    p.push(a, inner);
    let b = p.dup(a);
    let zero = p.int(0);
    let el = p.sep(a, zero);
    let two = p.int(2);
    p.push(el, two);
    let out = p.array(&[a, b]);
    let main = p.ret(out);
    assert_eq!(
        run(m, &Host::new(), main),
        "[[0 => [0 => 1, 1 => 2]], [0 => [0 => 1]]]"
    );
}

#[test]
fn the_steady_nested_append_loop_keeps_one_inner_array_per_slot() {
    // $g = [[], [], [], []]; for ($i = 0; $i < 400; $i++) $g[$i % 4][] = $i;
    // return [count($g[0]), $g[3][99]];  => [100, 399]
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let g = p.map();
    for _ in 0..4 {
        let e = p.map();
        p.push(g, e);
    }
    let (i, n, four, one, cond) = (
        p.int(0),
        p.int(400),
        p.int(4),
        p.int(1),
        p.f.reg(ValType::Bool),
    );
    let (top, done) = (p.f.label(), p.f.label());
    p.f.bind(top);
    p.f.emit(Inst::DLt {
        dst: cond,
        lhs: i,
        rhs: n,
    });
    p.f.jmp_if_not(cond, done);
    let k = p.reg();
    p.f.emit(Inst::DFloorMod {
        dst: k,
        lhs: i,
        rhs: four,
        pol: Policy::new(),
    });
    let inner = p.sep(g, k);
    p.push(inner, i);
    p.f.emit(Inst::DAdd {
        dst: i,
        lhs: i,
        rhs: one,
        pol: Policy::new(),
    });
    p.f.emit(Inst::Safepoint {});
    p.f.jmp(top);
    p.f.bind(done);
    let zero = p.int(0);
    let g0 = p.get(g, zero);
    let len = p.f.reg(ValType::I64);
    p.f.emit(Inst::DLen { dst: len, src: g0 });
    let lend = p.reg();
    p.f.emit(Inst::ToDyn {
        dst: lend,
        src: len,
        from: bytecode_lang::Prim::I64,
    });
    let three = p.int(3);
    let g3 = p.get(g, three);
    let n99 = p.int(99);
    let last = p.get(g3, n99);
    let out = p.array(&[lend, last]);
    let main = p.ret(out);
    assert_eq!(run(m, &Host::new(), main), "[100, 399]");
}

#[test]
fn separation_of_a_constant_copies_the_shared_inner_array() {
    // $a = [[1, 2]]; (a literal) $a[0][] = 3; $b = [[1, 2]]; return [$a, $b];
    let mut m = ModuleBuilder::new();
    let (k1, k2) = (m.constant(Const::Int(1)), m.constant(Const::Int(2)));
    let inner = m.constant(Const::Array(vec![k1, k2]));
    let outer = m.constant(Const::Array(vec![inner]));
    let mut p = P::new(&mut m, "main", &[]);
    let a = p.reg();
    p.f.emit(Inst::DLoadConst { dst: a, k: outer });
    let zero = p.int(0);
    let el = p.sep(a, zero);
    let three = p.int(3);
    p.f.emit(Inst::ArrayPush {
        arr: el,
        src: three,
    });
    let b = p.reg();
    p.f.emit(Inst::DLoadConst { dst: b, k: outer });
    let out = p.array(&[a, b]);
    let main = p.ret(out);
    assert_eq!(run(m, &Host::new(), main), "[[[1, 2, 3]], [[1, 2]]]");
}

// ---------------------------------------------------------------------------
// References (LSB §5.17)
// ---------------------------------------------------------------------------

#[test]
fn a_variable_reference_aliases_the_variable() {
    // $x = 1; $r = &$x; $r = 2; return $x;  => 2
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let one = p.int(1);
    let x = p.new_ref(one); // $x is taken by reference, so it lives in one
    let r = x;
    let two = p.int(2);
    p.assign(r, two);
    let v = p.deref(x);
    let main = p.ret(v);
    assert_eq!(run(m, &Host::new(), main), "2");
}

#[test]
fn foreach_by_reference_doubles_every_element() {
    // $a = [1, 2, 3]; foreach ($a as $k => &$v) { $v = $v * 2; } unset($v);
    // return $a;  => [2, 4, 6]
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let a = p.map();
    for v in [1, 2, 3] {
        let x = p.int(v);
        p.push(a, x);
    }
    let two = p.int(2);
    for k in 0..3 {
        let key = p.int(k);
        let v = p.refi(a, key);
        let cur = p.deref(v);
        let d = p.mul(cur, two);
        p.assign(v, d);
        // The code generator proves `$v` dead after the loop: unbind.
        p.f.emit(Inst::DUnrefIndex { obj: a, key });
    }
    let main = p.ret(a);
    assert_eq!(run(m, &Host::new(), main), "[0 => 2, 1 => 4, 2 => 6]");
}

#[test]
fn a_copy_shares_a_reference_slot_until_it_is_unbound() {
    // $a = [1]; $r = &$a[0]; $b = $a; $b[0] = 2;
    // return [$a[0], $r, $b[0]];  => [2, 2, 2]  (PHP: the reference is shared)
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let a = p.map();
    let one = p.int(1);
    p.push(a, one);
    let zero = p.int(0);
    let r = p.refi(a, zero);
    let b = p.dup(a);
    let two = p.int(2);
    p.set(b, zero, two);
    let (a0, rv, b0) = (p.get(a, zero), p.deref(r), p.get(b, zero));
    // After `dunref`, a copy no longer shares the slot.
    p.f.emit(Inst::DUnrefIndex { obj: a, key: zero });
    let c = p.dup(a);
    let three = p.int(3);
    p.set(c, zero, three);
    let a0_after = p.get(a, zero);
    let out = p.array(&[a0, rv, b0, a0_after]);
    let main = p.ret(out);
    assert_eq!(run(m, &Host::new(), main), "[2, 2, 2, 2]");
}

#[test]
fn binding_a_slot_to_a_variable_reference() {
    // $x = 1; $a = []; $a['k'] = &$x; $x = 5; $a['k'] = 7;
    // return [$x, $a['k']];  => [7, 7]
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let one = p.int(1);
    let x = p.new_ref(one);
    let a = p.map();
    let k = p.str("k");
    p.f.emit(Inst::DBindIndex {
        obj: a,
        key: k,
        src: x,
    });
    let five = p.int(5);
    p.assign(x, five);
    let seven = p.int(7);
    p.set(a, k, seven);
    let (xv, ak) = (p.deref(x), p.get(a, k));
    let out = p.array(&[xv, ak]);
    let main = p.ret(out);
    assert_eq!(run(m, &Host::new(), main), "[7, 7]");
}

#[test]
fn a_reference_to_a_missing_key_creates_it_as_null() {
    // $a = []; $r = &$a['new']; return $a;  => ['new' => null]
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let a = p.map();
    let k = p.str("new");
    let _r = p.refi(a, k);
    let main = p.ret(a);
    assert_eq!(run(m, &Host::new(), main), "[\"new\" => null]");
}

#[test]
fn a_reference_cycle_is_collected() {
    // $a = []; $a['self'] = &$a; (a reference holding the array that holds
    // it), dropped, then a collection: everything is reclaimed.
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let a = p.map();
    let ra = p.new_ref(a);
    let k = p.str("self");
    p.f.emit(Inst::DBindIndex {
        obj: a,
        key: k,
        src: ra,
    });
    let nil = p.reg();
    let main = p.ret(nil);
    let prog = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let mut vm = Vm::new(&prog);
    assert_eq!(vm.run(main, &[]), Ok(Value::Nil));
    let before = vm.heap_objects();
    vm.collect_garbage();
    assert!(
        vm.heap_objects() < before,
        "{before} -> {}",
        vm.heap_objects()
    );
}

// ---------------------------------------------------------------------------
// Dynamic calls (LSB §5.15)
// ---------------------------------------------------------------------------

/// `function f($a, $b = <default>, ...$rest) { return [$a, $b, $rest, mask]; }`
fn php_variadic(m: &mut ModuleBuilder) -> (FuncId, StrId, StrId) {
    let (a, b) = (m.string("a"), m.string("b"));
    let at = m.add_type(TypeDef::Array(D));
    let mut f = m.function("f", &[D, D, D, ValType::I64], &[D]);
    f.set_params(ParamList::new(vec![
        Param::normal(a),
        Param::normal(b).with_default(),
        Param::new(ParamKind::RestMap, None),
    ]));
    let (n, out, mask) = (f.reg(ValType::I64), f.reg(D), f.reg(D));
    let ty = f.type_ref(at);
    f.emit(Inst::LoadInt {
        dst: n,
        val: 0,
        ty: bytecode_lang::IntTy::I64,
    });
    f.emit(Inst::NewArray {
        dst: out,
        len: n,
        ty,
    });
    for r in 0..3 {
        f.emit(Inst::ArrayPush {
            arr: out,
            src: Reg(r),
        });
    }
    f.emit(Inst::ToDyn {
        dst: mask,
        src: Reg(3),
        from: bytecode_lang::Prim::I64,
    });
    f.emit(Inst::ArrayPush {
        arr: out,
        src: mask,
    });
    f.ret(out);
    (m.add_function(f).unwrap(), a, b)
}

#[test]
fn named_arguments_variadics_and_the_presence_mask() {
    // f(1, 2, 3, x: 4)  => [1, 2, [0 => 3, 'x' => 4], 0b111]
    // f(b: 5, a: 6)     => [6, 5, [], 0b011]
    // f(1)              => [1, null, [], 0b001]  (the callee computes $b's default)
    let mut m = ModuleBuilder::new();
    let (f, a, b) = php_variadic(&mut m);
    let x = m.string("x");
    let mut p = P::new(&mut m, "main", &[]);
    let fv = p.func_value(f);
    let (one, two, three, four) = (p.int(1), p.int(2), p.int(3), p.int(4));
    use ArgKind::{Named, Positional as Pos};
    let r1 = p.dcall_shape(
        fv,
        &[(Pos, one), (Pos, two), (Pos, three), (Named(x), four)],
    );
    let (five, six) = (p.int(5), p.int(6));
    let r2 = p.dcall_shape(fv, &[(Named(b), five), (Named(a), six)]);
    let r3 = p.dcall(fv, &[one]);
    let out = p.array(&[r1, r2, r3]);
    let main = p.ret(out);
    assert_eq!(
        run(m, &Host::new(), main),
        "[[1, 2, [0 => 3, \"x\" => 4], 7], [6, 5, [], 3], [1, null, [], 1]]"
    );
}

#[test]
fn argument_errors_are_raised_at_the_call_uncharged() {
    // PHP's "Cannot use positional argument after named argument", unknown
    // named parameter, overwriting a parameter, and too few arguments: all
    // ArgumentError (E0114), and no fuel is charged for them.
    for case in 0..4 {
        let mut m = ModuleBuilder::new();
        let (f, a, _) = php_variadic(&mut m);
        let mut p = P::new(&mut m, "main", &[]);
        let fv = p.func_value(f);
        let one = p.int(1);
        use ArgKind::{Named, Positional as Pos};
        let window = match case {
            0 => {
                let w = p.window(&[one, one]);
                p.f.emit(Inst::Nop {});
                // positional after named: only a spread can produce it.
                let mm = p.map();
                let ka = p.str("a");
                p.f.emit(Inst::MapSet {
                    map: mm,
                    key: ka,
                    src: one,
                });
                p.f.emit(Inst::MapPush { map: mm, src: one });
                p.f.mov(Reg(w.0 + 1), mm);
                p.f.dcall_shape(w, fv, &[ArgKind::Spread]);
                w
            }
            1 => {
                // An unknown name with no variadic to collect it: `g($a)`
                // called as g(1, zzz: 1).
                let q = p.m.string("zzz");
                let mut g = p.m.function("g", &[D], &[D]);
                g.set_params(ParamList::new(vec![Param::normal(a)]));
                g.ret(Reg(0));
                let g = p.m.add_function(g).unwrap();
                let gv = p.func_value(g);
                p.dcall_shape(gv, &[(Pos, one), (Named(q), one)])
            }
            2 => p.dcall_shape(fv, &[(Pos, one), (Named(a), one)]),
            _ => p.dcall(fv, &[]),
        };
        let main = p.ret(window);
        let prog = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
        let mut vm = Vm::new(&prog);
        let err = vm.run(main, &[]).unwrap_err();
        assert_eq!(err.kind(), Some(ErrorKind::ArgumentError), "case {case}");
        assert_eq!(
            vm.fuel_used(),
            0,
            "case {case}: binding errors are uncharged"
        );
    }
}

#[test]
fn by_reference_arguments_are_decided_at_run_time() {
    // function inc(&$x) { $x = $x + 1; }  $a = ['n' => 1];
    // the code generator asks dparam_ref(inc, 0) before evaluating $a['n'],
    // then passes &$a['n'];  inc(5) passes a temporary.
    // return [$a, dparam_ref(inc, 0), dparam_ref(inc, 1)];
    let mut m = ModuleBuilder::new();
    let x = m.string("x");
    let mut inc = m.function("inc", &[D], &[D]);
    inc.set_params(ParamList::new(vec![Param::normal(x).by_ref()]));
    let (v, one) = (inc.reg(D), inc.reg(D));
    inc.emit(Inst::CellGet {
        dst: v,
        cell: Reg(0),
    });
    inc.emit(Inst::DLoadInt { dst: one, val: 1 });
    inc.emit(Inst::DAdd {
        dst: v,
        lhs: v,
        rhs: one,
        pol: Policy::new(),
    });
    inc.emit(Inst::CellSet {
        cell: Reg(0),
        src: v,
    });
    inc.ret(v);
    let inc = m.add_function(inc).unwrap();
    let mut p = P::new(&mut m, "main", &[]);
    let fv = p.func_value(inc);
    let a = p.map();
    let (k, one) = (p.str("n"), p.int(1));
    p.set(a, k, one);
    let (pos, flag, pos1, flag1) = (
        p.f.reg(ValType::I64),
        p.f.reg(ValType::Bool),
        p.f.reg(ValType::I64),
        p.f.reg(ValType::Bool),
    );
    p.f.emit(Inst::LoadInt {
        dst: pos,
        val: 0,
        ty: bytecode_lang::IntTy::I64,
    });
    p.f.emit(Inst::DParamRef {
        dst: flag,
        callee: fv,
        pos,
    });
    p.f.emit(Inst::LoadInt {
        dst: pos1,
        val: 1,
        ty: bytecode_lang::IntTy::I64,
    });
    p.f.emit(Inst::DParamRef {
        dst: flag1,
        callee: fv,
        pos: pos1,
    });
    let r = p.refi(a, k);
    let _ = p.dcall(fv, &[r]);
    let five = p.int(5);
    let tmp = p.dcall(fv, &[five]);
    let (fd, fd1) = (p.reg(), p.reg());
    p.f.emit(Inst::ToDyn {
        dst: fd,
        src: flag,
        from: bytecode_lang::Prim::Bool,
    });
    p.f.emit(Inst::ToDyn {
        dst: fd1,
        src: flag1,
        from: bytecode_lang::Prim::Bool,
    });
    p.f.emit(Inst::DUnrefIndex { obj: a, key: k });
    let out = p.array(&[a, tmp, fd, fd1]);
    let main = p.ret(out);
    assert_eq!(run(m, &Host::new(), main), "[[\"n\" => 2], 6, true, false]");
}

#[test]
fn host_functions_are_values_with_parameter_lists() {
    // array_push(&$array, ...$values) and count(...) as host functions,
    // passed around as values and called through dcall / dcall_shape:
    // $push = 'array_push'; $a = [1]; $push($a, 2, 3); return [$a, count($a)];
    let mut m = ModuleBuilder::new();
    let (arr_name, values) = (m.string("array"), m.string("values"));
    let sig = m.func_type(&[D, D], &[D]);
    let push = m.import_with_params(
        "php",
        "array_push",
        sig,
        ParamList::new(vec![
            Param::normal(arr_name).by_ref(),
            Param::new(ParamKind::Rest, Some(values)),
        ]),
    );
    let count_sig = m.func_type(&[D], &[D]);
    let count = m.import("php", "count", count_sig);
    let mut host = Host::new();
    host.register("php", "array_push", |ctx, args| {
        // args[0] is the reference, args[1] the rest array.
        let target = ctx
            .ref_get(args[0])
            .ok_or(HostError::Raise(ErrorKind::TypeError))?;
        let mut items: Vec<(Value, Value)> = ctx.entries(target).unwrap_or_default();
        let extra = ctx.elements(args[1]).unwrap_or_default();
        let mut next = items.len() as i64;
        for v in extra {
            items.push((Value::Int(next), v));
            next += 1;
        }
        let new = ctx.new_map(&items)?;
        ctx.ref_set(args[0], new)?;
        Ok(Value::Int(next))
    });
    host.register("php", "count", |ctx, args| {
        let n = ctx.entries(args[0]).map_or(0, |e| e.len());
        Ok(Value::Int(n as i64))
    });
    let mut p = P::new(&mut m, "main", &[]);
    let pv = p.import_value(push);
    let cv = p.import_value(count);
    let a = p.map();
    let one = p.int(1);
    p.push(a, one);
    let ra = p.new_ref(a);
    let (two, three) = (p.int(2), p.int(3));
    let n = p.dcall(pv, &[ra, two, three]);
    let now = p.deref(ra);
    let c = p.dcall_shape(cv, &[(ArgKind::Positional, now)]);
    let out = p.array(&[now, n, c]);
    let main = p.ret(out);
    assert_eq!(run(m, &host, main), "[[0 => 1, 1 => 2, 2 => 3], 3, 3]");
}

#[test]
fn ignore_extra_drops_surplus_positionals_like_php_user_functions() {
    // function g($a) { return $a; } g(1, 2, 3) => 1
    let mut m = ModuleBuilder::new();
    let a = m.string("a");
    let mut g = m.function("g", &[D], &[D]);
    g.set_params(ParamList::new(vec![Param::normal(a)]).ignoring_extra());
    g.ret(Reg(0));
    let g = m.add_function(g).unwrap();
    let mut p = P::new(&mut m, "main", &[]);
    let gv = p.func_value(g);
    let (one, two, three) = (p.int(1), p.int(2), p.int(3));
    let r = p.dcall(gv, &[one, two, three]);
    let main = p.ret(r);
    assert_eq!(run(m, &Host::new(), main), "1");
}

#[test]
fn spreads_bind_like_php_argument_unpacking() {
    // f(...[1, 2], ...['x' => 9]) => [1, 2, ['x' => 9], 0b111]
    let mut m = ModuleBuilder::new();
    let (f, _, _) = php_variadic(&mut m);
    let mut p = P::new(&mut m, "main", &[]);
    let fv = p.func_value(f);
    let pos = p.map();
    let (one, two, nine) = (p.int(1), p.int(2), p.int(9));
    p.push(pos, one);
    p.push(pos, two);
    let named = p.map();
    let x = p.str("x");
    p.set(named, x, nine);
    let r = p.dcall_shape(fv, &[(ArgKind::Spread, pos), (ArgKind::Spread, named)]);
    let main = p.ret(r);
    assert_eq!(run(m, &Host::new(), main), "[1, 2, [\"x\" => 9], 7]");
}

// ---------------------------------------------------------------------------
// OPS v2 arithmetic, generators, match
// ---------------------------------------------------------------------------

#[test]
fn php_pow_abs_and_shifts() {
    // [2 ** -1, 2 ** 63, (-2) ** 63, 3 ** 2, 2.0 ** 0.5, abs(PHP_INT_MIN),
    //  1 << 64, -8 >> 64, PHP_INT_MAX + 1]
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let php = Policy::new()
        .with_overflow(Overflow::Promote)
        .with_shift(Shift::Saturate);
    let mut out = Vec::new();
    for (x, y) in [(2, -1), (2, 63), (-2, 63), (3, 2)] {
        let (a, b, r) = (p.int(x), p.int(y), p.reg());
        p.f.emit(Inst::DPow {
            dst: r,
            lhs: a,
            rhs: b,
            pol: php,
        });
        out.push(r);
    }
    let (two, half, r) = (p.float(2.0), p.float(0.5), p.reg());
    p.f.emit(Inst::DPow {
        dst: r,
        lhs: two,
        rhs: half,
        pol: php,
    });
    out.push(r);
    let (min, r) = (p.big(i64::MIN), p.reg());
    p.f.emit(Inst::DAbs {
        dst: r,
        src: min,
        pol: php,
    });
    out.push(r);
    let (one, sixty_four, r) = (p.int(1), p.int(64), p.reg());
    p.f.emit(Inst::DShl {
        dst: r,
        lhs: one,
        rhs: sixty_four,
        pol: php,
    });
    out.push(r);
    let (neg8, r) = (p.int(-8), p.reg());
    p.f.emit(Inst::DShr {
        dst: r,
        lhs: neg8,
        rhs: sixty_four,
        pol: php,
    });
    out.push(r);
    let (max, r) = (p.big(i64::MAX), p.reg());
    p.f.emit(Inst::DAdd {
        dst: r,
        lhs: max,
        rhs: one,
        pol: php,
    });
    out.push(r);
    let arr = p.array(&out);
    let main = p.ret(arr);
    assert_eq!(
        run(m, &Host::new(), main),
        "[0.5, 9.223372036854776e18, -9223372036854775808, 9, 1.4142135623730951, \
         9.223372036854776e18, 0, -1, 9.223372036854776e18]"
    );
}

#[test]
fn shifting_by_a_negative_amount_is_an_arithmetic_error_and_pow_negative_exponent_too() {
    // PHP: 1 << -1 throws ArithmeticError; intdiv-style integer pow with a
    // negative exponent under a non-promote policy is NegativeExponent.
    for (inst, want) in [
        (0, ErrorKind::ShiftOutOfRange),
        (1, ErrorKind::NegativeExponent),
    ] {
        let mut m = ModuleBuilder::new();
        let mut p = P::new(&mut m, "main", &[]);
        let (one, neg) = (p.int(1), p.int(-1));
        let r = p.reg();
        let pol = Policy::new().with_shift(Shift::Saturate);
        if inst == 0 {
            p.f.emit(Inst::DShl {
                dst: r,
                lhs: one,
                rhs: neg,
                pol,
            });
        } else {
            p.f.emit(Inst::DPow {
                dst: r,
                lhs: one,
                rhs: neg,
                pol,
            });
        }
        let main = p.ret(r);
        let prog = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
        assert_eq!(
            Vm::new(&prog).run(main, &[]).unwrap_err().kind(),
            Some(want)
        );
    }
}

#[test]
fn generator_keys_follow_php() {
    // function g() { yield -5 => 'a'; yield 'b'; yield 10 => 'c'; yield 'd'; }
    // keys: -5, 0, 10, 11
    let mut m = ModuleBuilder::new();
    let mut g = m.function("g", &[], &[]);
    let (s, k, v) = (g.reg(D), g.reg(D), g.reg(D));
    for (key, auto) in [
        (Some(-5), false),
        (None, true),
        (Some(10), false),
        (None, true),
    ] {
        g.emit(Inst::DLoadInt { dst: v, val: 0 });
        match (key, auto) {
            (Some(x), _) => {
                g.emit(Inst::DLoadInt { dst: k, val: x });
                g.emit(Inst::YieldKv {
                    dst: s,
                    key: k,
                    src: v,
                });
            }
            _ => {
                g.emit(Inst::Yield { dst: s, src: v });
            }
        }
    }
    g.ret_void();
    let g = m.add_function(g).unwrap();
    let mut p = P::new(&mut m, "main", &[]);
    let c = p.reg();
    p.f.emit(Inst::CoroNew {
        dst: c,
        func: g,
        argc: 0,
    });
    let mut keys = Vec::new();
    let nil = p.reg();
    for _ in 0..4 {
        let (r, key) = (p.reg(), p.reg());
        p.f.emit(Inst::Resume {
            dst: r,
            coro: c,
            src: nil,
        });
        p.f.emit(Inst::CoroKey { dst: key, coro: c });
        keys.push(key);
    }
    let out = p.array(&keys);
    let main = p.ret(out);
    assert_eq!(run(m, &Host::new(), main), "[-5, 0, 10, 11]");
}

#[test]
fn match_without_an_arm_raises_no_match_with_the_scrutinee() {
    // echo match (42) {};  => UnhandledMatchError carrying 42
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let x = p.int(42);
    p.f.emit(Inst::Raise {
        src: x,
        kind: ErrorKind::NoMatch,
    });
    let main = p.ret(x);
    let prog = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    let err = Vm::new(&prog).run(main, &[]).unwrap_err();
    assert_eq!(
        err,
        VmError::Raised {
            kind: ErrorKind::NoMatch,
            payload: Value::Int(42),
            func: main,
            pc: 1
        }
    );
    assert_eq!(err.code(), Some(200));
    assert_eq!(err.to_string(), "uncaught E0200 NoMatch (42) at f0 @1");
}

// ---------------------------------------------------------------------------
// Separation and references meeting copies (LSB §5.16, §5.17)
// ---------------------------------------------------------------------------

/// `$a = [[1]]; $b = $a;` then `step`, then `[$a, $b]`.
fn after_copy(step: impl FnOnce(&mut P<'_>, Reg)) -> String {
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let one = p.int(1);
    let inner = p.map();
    p.push(inner, one);
    let a = p.map();
    p.push(a, inner);
    let b = p.dup(a);
    step(&mut p, a);
    let out = p.array(&[a, b]);
    let main = p.ret(out);
    run(m, &Host::new(), main)
}

#[test]
fn a_write_to_the_original_after_a_copy_still_separates_its_elements() {
    // $b = $a; $a[] = 2; $a[0][] = 9;  => $b[0] stays [1]
    let got = after_copy(|p, a| {
        let two = p.int(2);
        p.push(a, two);
        let zero = p.int(0);
        let el = p.sep(a, zero);
        let nine = p.int(9);
        p.push(el, nine);
    });
    assert_eq!(got, "[[0 => [0 => 1, 1 => 9], 1 => 2], [0 => [0 => 1]]]");
}

#[test]
fn a_reference_into_a_copied_array_separates_the_element_first() {
    // $b = $a; $r = &$a[0]; $r[] = 9;  => $b[0] stays [1]
    let got = after_copy(|p, a| {
        let zero = p.int(0);
        let r = p.refi(a, zero);
        let el = p.deref(r);
        let nine = p.int(9);
        p.push(el, nine);
    });
    assert_eq!(got, "[[0 => [0 => 1, 1 => 9]], [0 => [0 => 1]]]");
}

#[test]
fn two_references_to_one_slot_are_one_reference() {
    // $r1 = &$a[0]; $r2 = &$a[0]; $r2 = 5;  => $r1 == 5, $a[0] == 5
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let a = p.map();
    let one = p.int(1);
    p.push(a, one);
    let zero = p.int(0);
    let r1 = p.refi(a, zero);
    let r2 = p.refi(a, zero);
    let five = p.int(5);
    p.assign(r2, five);
    let (v1, a0) = (p.deref(r1), p.get(a, zero));
    let out = p.array(&[v1, a0]);
    let main = p.ret(out);
    assert_eq!(run(m, &Host::new(), main), "[5, 5]");
}

#[test]
fn unbinding_a_slot_separates_the_array_it_held() {
    // $x = [1]; $a[0] = &$x; unset-binding of $a[0] (dunref); $x[] = 9;
    // => $a[0] stays [1], $x is [1, 9]
    let mut m = ModuleBuilder::new();
    let mut p = P::new(&mut m, "main", &[]);
    let one = p.int(1);
    let inner = p.map();
    p.push(inner, one);
    let x = p.new_ref(inner);
    let a = p.map();
    let zero = p.int(0);
    p.f.emit(Inst::DBindIndex {
        obj: a,
        key: zero,
        src: x,
    });
    p.f.emit(Inst::DUnrefIndex { obj: a, key: zero });
    let xv = p.deref(x);
    let nine = p.int(9);
    p.push(xv, nine);
    let out = p.array(&[a, xv]);
    let main = p.ret(out);
    assert_eq!(
        run(m, &Host::new(), main),
        "[[0 => [0 => 1]], [0 => 1, 1 => 9]]"
    );
}
