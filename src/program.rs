//! Loading: turning a bytecode-lang [`Module`] into a [`Program`] the VM can
//! run without further checks.
//!
//! LSB has no verifier yet (it lands in bytecode-lang 0.5), and the decoder
//! deliberately checks structure only, not indices (LSB §7.5). So the loader
//! checks, once and in time linear in the module, every fact the interpreter
//! relies on to index without failing: every register operand (including
//! every register of every call window) is inside its frame, every constant,
//! function, import, global, table, name, type, and capture index is in range,
//! every branch, table, and handler target is an instruction, no function can
//! fall off its end, direct calls match their callee's arity, and the type
//! operands of allocating instructions name types of the right kind. With
//! those facts the dispatch loop never meets an out-of-range index.
//!
//! What the loader does *not* check is the verifier's type discipline (V-T):
//! whether each register holds the type an instruction expects. The VM does
//! not need it for safety. Every register is a 64-bit word, every heap access
//! checks the object's kind and liveness, and a word of the wrong type decodes
//! to *some* value deterministically. An ill-typed module computes garbage or
//! raises `TypeError`; it cannot read outside the VM or panic.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use bytecode_lang::{
    Callee, Const, DecodeError, ExportItem, FieldKind, FuncId, Hook, Inst, Module, Opcode,
    Overflow, Prim, TypeDef, TypeId, ValType,
};

use crate::host::{Host, HostFn};

/// The deepest struct inheritance chain a program may declare. Method lookup
/// walks the chain, so the cap bounds `get_prop`'s worst case.
pub const MAX_INHERITANCE_DEPTH: usize = 256;

/// The deepest nesting of aggregate constants (bytecode-lang's default
/// decoding budget). Materialisation recurses over it.
pub const MAX_CONST_DEPTH: u32 = 64;

/// Number of hook codes LSB defines.
pub(crate) const HOOKS: usize = 28;

/// Per-function facts the interpreter uses.
#[derive(Debug)]
pub(crate) struct FuncInfo {
    /// The canonical signature type.
    pub(crate) sig: u32,
    pub(crate) nregs: usize,
    pub(crate) nparams: usize,
    /// Declared register types (struct ids canonical).
    pub(crate) regs: Box<[ValType]>,
    pub(crate) result: Option<ValType>,
    /// Registers that hold references: the frame's GC roots.
    pub(crate) ref_regs: Box<[u16]>,
    pub(crate) captures: Box<[ValType]>,
    /// The per-function name list, as canonical string ids.
    pub(crate) names: Box<[u32]>,
    /// The per-function type list, as canonical type ids.
    pub(crate) type_refs: Box<[u32]>,
    /// For functions with many try regions: per pc, the index of the first
    /// handler (in list order) covering it, or `u32::MAX`. Smaller handler
    /// lists are scanned instead.
    pub(crate) catch_table: Option<Box<[u32]>>,
}

/// Handler lists longer than this get a per-pc lookup table, so raising
/// costs O(1) however many regions a (hostile) function declares.
const CATCH_TABLE_MIN: usize = 8;

/// Per pc, the first handler in list order whose region covers it: a sweep
/// over region boundaries with the set of open regions, O((len + h) log h).
fn catch_table(len: usize, handlers: &[bytecode_lang::Handler]) -> Box<[u32]> {
    let mut starts: Vec<Vec<u32>> = vec![Vec::new(); len + 1];
    let mut ends: Vec<Vec<u32>> = vec![Vec::new(); len + 1];
    for (i, h) in handlers.iter().enumerate() {
        let i = count(i);
        if let Some(v) = starts.get_mut(h.start as usize) {
            v.push(i);
        }
        if let Some(v) = ends.get_mut(h.end as usize) {
            v.push(i);
        }
    }
    let mut open = alloc::collections::BTreeSet::new();
    let mut table = Vec::with_capacity(len);
    for pc in 0..len {
        for &i in ends.get(pc).map_or(&[][..], Vec::as_slice) {
            let _removed = open.remove(&i);
        }
        for &i in starts.get(pc).map_or(&[][..], Vec::as_slice) {
            let _new = open.insert(i);
        }
        table.push(open.first().copied().unwrap_or(u32::MAX));
    }
    table.into_boxed_slice()
}

/// A struct type, prepared for field and method lookup.
#[derive(Debug, Default)]
pub(crate) struct StructInfo {
    pub(crate) parent: Option<u32>,
    pub(crate) fields: Box<[ValType]>,
    pub(crate) ref_fields: Box<[u16]>,
    /// (canonical field name, slot), sorted by name; first slot wins.
    pub(crate) field_names: Box<[(u32, u16)]>,
    /// (canonical method name, function), sorted by name; first wins.
    pub(crate) methods: Box<[(u32, u32)]>,
    /// Preorder interval in the inheritance forest: `t` descends from `a`
    /// exactly when `a.pre <= t.pre < a.post`.
    pub(crate) pre: u32,
    pub(crate) post: u32,
}

/// A type-table entry, with every inner type id canonical.
#[derive(Debug)]
pub(crate) enum TypeInfo {
    Func,
    Struct(StructInfo),
    Array(ValType),
    Map(ValType, ValType),
    Cell(ValType),
    Iter(ValType, ValType),
    Coroutine,
}

/// A bound import.
#[derive(Debug)]
pub(crate) struct ImportInfo {
    pub(crate) params: Box<[ValType]>,
    pub(crate) result: Option<ValType>,
    pub(crate) sig: u32,
    pub(crate) func: HostFn,
}

/// A loaded module: checked, bound to its host functions, and ready to run.
///
/// A `Program` is immutable and can be shared between threads; each
/// [`Vm`](crate::Vm) running it owns its own heap, globals, and stack.
///
/// # Examples
///
/// ```
/// use bvm_lang::{Host, Program, Value, Vm};
/// use bytecode_lang::{Inst, IntOp, IntTy, ModuleBuilder, ValType, ExportItem};
///
/// let mut m = ModuleBuilder::new();
/// let mut f = m.function("add", &[ValType::I64, ValType::I64], &[ValType::I64]);
/// let sum = f.reg(ValType::I64);
/// f.emit(Inst::IAdd { dst: sum, lhs: f.param(0), rhs: f.param(1), op: IntOp::new(IntTy::I64) });
/// f.ret(sum);
/// let add = m.add_function(f).unwrap();
/// m.export("add", ExportItem::Func(add));
///
/// let program = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
/// assert_eq!(program.export("add"), Some(add));
/// let mut vm = Vm::new(&program);
/// assert_eq!(vm.run(add, &[Value::Int(2), Value::Int(40)]), Ok(Value::Int(42)));
/// ```
#[derive(Debug)]
pub struct Program {
    pub(crate) module: Module,
    pub(crate) funcs: Vec<FuncInfo>,
    pub(crate) types: Vec<TypeInfo>,
    pub(crate) imports: Vec<ImportInfo>,
    pub(crate) hooks: [Option<Callee>; HOOKS],
    /// Each type id's canonical id (the first structurally equal type; a
    /// struct is its own).
    pub(crate) canon_ty: Vec<u32>,
    pub(crate) global_types: Vec<ValType>,
    pub(crate) global_refs: Vec<u32>,
}

/// Why a module could not be loaded.
///
/// # Examples
///
/// ```
/// use bvm_lang::{Host, LoadErrorKind, Program};
/// use bytecode_lang::{FuncId, Inst, ModuleBuilder, ValType};
///
/// let mut m = ModuleBuilder::new();
/// let callee = m.function("callee", &[ValType::I64], &[]);
/// let mut main = m.function("main", &[], &[]);
/// let window = main.regs(&[ValType::I64, ValType::I64]);
/// // `call` with argc 0 to a one-parameter function: the builder emits it,
/// // the loader refuses it.
/// main.emit(Inst::Call { dst: window, func: callee.id(), argc: 0 });
/// main.ret_void();
/// let mut callee = callee;
/// callee.ret_void();
/// m.add_function(callee).unwrap();
/// m.add_function(main).unwrap();
/// let err = Program::load(m.finish().unwrap(), &Host::new()).unwrap_err();
/// assert_eq!(err.kind(), &LoadErrorKind::ArityMismatch { expected: 1, found: 0 });
/// assert_eq!((err.func(), err.pc()), (Some(FuncId(1)), Some(0)));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError {
    kind: LoadErrorKind,
    func: Option<FuncId>,
    pc: Option<u32>,
}

/// The reason in a [`LoadError`].
///
/// # Examples
///
/// ```
/// use bvm_lang::LoadErrorKind;
///
/// let k = LoadErrorKind::OutOfRange { what: "register", index: 9 };
/// assert_eq!(k.to_string(), "register 9 out of range");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadErrorKind {
    /// The bytes are not a well-formed module ([`Program::decode`] only).
    Decode(DecodeError),
    /// No host function is registered under the import's module and name.
    UnresolvedImport {
        /// The import's module name.
        module: String,
        /// The import's name.
        name: String,
    },
    /// An index (register, constant, function, import, global, table, name,
    /// type, capture, string, or branch target) is out of range.
    OutOfRange {
        /// What kind of index.
        what: &'static str,
        /// The index.
        index: u32,
    },
    /// A signature is not a `func` type, or has more than one result.
    BadSignature,
    /// A function's registers do not begin with its signature's parameters.
    ParamMismatch,
    /// A function has no instructions.
    EmptyCode,
    /// A function's last instruction can fall through past the end.
    FallsThrough,
    /// A direct call, tail call, or import call passes the wrong number of
    /// arguments.
    ArityMismatch {
        /// The callee's parameter count.
        expected: u32,
        /// The instruction's argument count.
        found: u32,
    },
    /// A function with captures is the target of a direct call, a tail call,
    /// the start function, or an entry point (it needs a closure).
    CalleeHasCaptures,
    /// An instruction carries `overflow = promote` but does not write a `dyn`
    /// register (LSB V-T8).
    PromoteNotDynamic,
    /// A type operand names a type of the wrong kind (`new_array` of a
    /// non-array type, ...).
    WrongTypeKind {
        /// The kind the instruction needs.
        expected: &'static str,
    },
    /// An invalid modifier: `from_dyn` to `ref`, or a bit cast between
    /// widths that are not 32 or 64.
    BadModifier,
    /// A try region is empty, inverted, or reaches past the end.
    InvalidHandler,
    /// A catch register is not `dyn`.
    CatchNotDyn,
    /// A tail call lies inside a try region (LSB V-CF4).
    TailCallInTry,
    /// A struct's parent is not a struct, or its fields do not begin with
    /// the parent's.
    BadParent,
    /// A struct inheritance chain is cyclic or deeper than
    /// [`MAX_INHERITANCE_DEPTH`].
    InheritanceTooDeep,
    /// An aggregate constant refers to itself or a later constant, or nests
    /// deeper than [`MAX_CONST_DEPTH`].
    BadConstant,
    /// A hook's callee does not have the signature the hook requires.
    BadHook(Hook),
    /// The start function does not have the signature `() -> ()`.
    BadStart,
}

impl fmt::Display for LoadErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadErrorKind::Decode(e) => write!(f, "decode error: {e}"),
            LoadErrorKind::UnresolvedImport { module, name } => {
                write!(
                    f,
                    "no host function registered for import {module:?}.{name:?}"
                )
            }
            LoadErrorKind::OutOfRange { what, index } => write!(f, "{what} {index} out of range"),
            LoadErrorKind::BadSignature => {
                f.write_str("signature is not a func type with at most one result")
            }
            LoadErrorKind::ParamMismatch => {
                f.write_str("registers do not begin with the signature's parameters")
            }
            LoadErrorKind::EmptyCode => f.write_str("function has no instructions"),
            LoadErrorKind::FallsThrough => f.write_str("last instruction falls through"),
            LoadErrorKind::ArityMismatch { expected, found } => {
                write!(f, "call passes {found} arguments, callee takes {expected}")
            }
            LoadErrorKind::CalleeHasCaptures => {
                f.write_str("function with captures called without a closure")
            }
            LoadErrorKind::PromoteNotDynamic => {
                f.write_str("overflow = promote on an instruction that does not write dyn")
            }
            LoadErrorKind::WrongTypeKind { expected } => {
                write!(f, "type operand is not {expected} type")
            }
            LoadErrorKind::BadModifier => f.write_str("invalid modifier"),
            LoadErrorKind::InvalidHandler => f.write_str("invalid try region"),
            LoadErrorKind::CatchNotDyn => f.write_str("catch register is not dyn"),
            LoadErrorKind::TailCallInTry => f.write_str("tail call inside a try region"),
            LoadErrorKind::BadParent => f.write_str("invalid struct parent"),
            LoadErrorKind::InheritanceTooDeep => {
                f.write_str("struct inheritance is cyclic or too deep")
            }
            LoadErrorKind::BadConstant => f.write_str("invalid aggregate constant"),
            LoadErrorKind::BadHook(h) => write!(f, "hook {h} has the wrong signature"),
            LoadErrorKind::BadStart => f.write_str("start function is not () -> ()"),
        }
    }
}

impl LoadError {
    fn new(kind: LoadErrorKind) -> LoadError {
        LoadError {
            kind,
            func: None,
            pc: None,
        }
    }

    fn at(kind: LoadErrorKind, func: u32, pc: Option<u32>) -> LoadError {
        LoadError {
            kind,
            func: Some(FuncId(func)),
            pc,
        }
    }

    /// The reason.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, LoadErrorKind, Program};
    ///
    /// let err = Program::decode(b"not a module", &Host::new()).unwrap_err();
    /// assert!(matches!(err.kind(), LoadErrorKind::Decode(_)));
    /// ```
    #[must_use]
    pub fn kind(&self) -> &LoadErrorKind {
        &self.kind
    }

    /// The function the problem is in, if it is in one.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program};
    ///
    /// let err = Program::decode(b"", &Host::new()).unwrap_err();
    /// assert_eq!(err.func(), None);
    /// ```
    #[must_use]
    pub fn func(&self) -> Option<FuncId> {
        self.func
    }

    /// The instruction the problem is at, if it is at one.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program};
    ///
    /// let err = Program::decode(b"", &Host::new()).unwrap_err();
    /// assert_eq!(err.pc(), None);
    /// ```
    #[must_use]
    pub fn pc(&self) -> Option<u32> {
        self.pc
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.func, self.pc) {
            (Some(func), Some(pc)) => write!(f, "{func} @{pc}: {}", self.kind),
            (Some(func), None) => write!(f, "{func}: {}", self.kind),
            _ => fmt::Display::fmt(&self.kind, f),
        }
    }
}

impl core::error::Error for LoadError {}

/// A source location from a function's line table.
///
/// # Examples
///
/// ```
/// use bvm_lang::Location;
///
/// let loc = Location { file: "main.mox", line: 3, column: 7 };
/// assert_eq!(loc.to_string(), "main.mox:3:7");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location<'a> {
    /// The file name.
    pub file: &'a str,
    /// The line (as the producer numbered it).
    pub line: u32,
    /// The column (as the producer numbered it).
    pub column: u32,
}

impl fmt::Display for Location<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

/// The arity and result of each hook (LSB §5.8).
const fn hook_shape(h: Hook) -> (usize, bool) {
    match h {
        Hook::Neg | Hook::BitNot | Hook::Truthy | Hook::Iter | Hook::Len | Hook::Spawn => (1, true),
        Hook::SetIndex | Hook::SetProp => (3, false),
        _ => (2, true),
    }
}

/// Instructions that end a block without falling through.
const fn is_terminator(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Ret
            | Opcode::RetVoid
            | Opcode::Throw
            | Opcode::Jmp
            | Opcode::Switch
            | Opcode::TailCall
            | Opcode::TailCallIndirect
            | Opcode::Unreachable
    )
}

fn out_of_range(what: &'static str, index: u32) -> LoadErrorKind {
    LoadErrorKind::OutOfRange { what, index }
}

/// A `u32` count of a slice (modules cannot exceed `u32` entries, but the
/// conversion is checked anyway).
fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

impl Program {
    /// Checks `module` and binds its imports to `host`.
    ///
    /// # Errors
    ///
    /// A [`LoadError`] naming the first problem found, with the function and
    /// instruction where it applies.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, LoadErrorKind, Program};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let mut m = ModuleBuilder::new();
    /// let sig = m.func_type(&[], &[]);
    /// m.import("env", "missing", sig);
    /// let err = Program::load(m.finish().unwrap(), &Host::new()).unwrap_err();
    /// assert!(matches!(err.kind(), LoadErrorKind::UnresolvedImport { .. }));
    /// ```
    pub fn load(module: Module, host: &Host) -> Result<Program, LoadError> {
        Loader::new(&module)
            .run(host)
            .map(|parts| parts.into_program(module))
    }

    /// Decodes LSB bytes with bytecode-lang's default budgets, then loads
    /// the module.
    ///
    /// # Errors
    ///
    /// [`LoadErrorKind::Decode`] for malformed bytes, otherwise as
    /// [`load`](Program::load).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let bytes = bytecode_lang::encode(&ModuleBuilder::new().finish().unwrap());
    /// assert!(Program::decode(&bytes, &Host::new()).is_ok());
    /// ```
    pub fn decode(bytes: &[u8], host: &Host) -> Result<Program, LoadError> {
        let module =
            bytecode_lang::decode(bytes).map_err(|e| LoadError::new(LoadErrorKind::Decode(e)))?;
        Program::load(module, host)
    }

    /// The module this program was loaded from.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let p = Program::load(ModuleBuilder::new().finish().unwrap(), &Host::new()).unwrap();
    /// assert_eq!(p.module().functions().len(), 0);
    /// ```
    #[must_use]
    pub fn module(&self) -> &Module {
        &self.module
    }

    /// The function exported under `name`.
    ///
    /// # Examples
    ///
    /// See [`Program`].
    #[must_use]
    pub fn export(&self, name: &str) -> Option<FuncId> {
        self.module.exports().iter().find_map(|e| match e.item {
            ExportItem::Func(f) if self.module.string(e.name) == Some(name) => Some(f),
            _ => None,
        })
    }

    /// The source location of instruction `pc` of `func`, from the module's
    /// line table.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::{Host, Program};
    /// use bytecode_lang::ModuleBuilder;
    ///
    /// let mut m = ModuleBuilder::new();
    /// let file = m.string("main.mox");
    /// let mut f = m.function("main", &[], &[]);
    /// f.set_location(file, 4, 2);
    /// f.ret_void();
    /// let id = m.add_function(f).unwrap();
    /// let p = Program::load(m.finish().unwrap(), &Host::new()).unwrap();
    /// assert_eq!(p.location(id, 0).map(|l| l.line), Some(4));
    /// ```
    #[must_use]
    pub fn location(&self, func: FuncId, pc: u32) -> Option<Location<'_>> {
        let rows = self.module.function(func)?.lines();
        let i = rows.partition_point(|r| r.pc <= pc).checked_sub(1)?;
        let row = rows.get(i)?;
        Some(Location {
            file: self.module.string(row.file)?,
            line: row.line,
            column: row.column,
        })
    }

    /// The code of a function (its index was checked at load).
    #[inline]
    pub(crate) fn code(&self, func: u32) -> &[Inst] {
        self.module
            .function(FuncId(func))
            .map_or(&[], bytecode_lang::Function::code)
    }

    /// The function's facts.
    #[inline]
    pub(crate) fn func(&self, func: u32) -> Option<&FuncInfo> {
        self.funcs.get(func as usize)
    }

    /// The struct's facts.
    #[inline]
    pub(crate) fn struct_info(&self, ty: u32) -> Option<&StructInfo> {
        match self.types.get(ty as usize)? {
            TypeInfo::Struct(s) => Some(s),
            _ => None,
        }
    }

    /// Reference-typed field slots of a struct (for the collector).
    pub(crate) fn struct_ref_fields(&self, ty: u32) -> &[u16] {
        self.struct_info(ty).map_or(&[], |s| &s.ref_fields)
    }

    /// Capture types of a function (for the collector).
    pub(crate) fn capture_types(&self, func: u32) -> &[ValType] {
        self.funcs.get(func as usize).map_or(&[], |f| &f.captures)
    }

    /// Whether struct `t` is `a` or descends from it.
    #[inline]
    pub(crate) fn descends(&self, t: u32, a: u32) -> bool {
        match (self.struct_info(t), self.struct_info(a)) {
            (Some(ts), Some(as_)) => as_.pre <= ts.pre && ts.pre < as_.post,
            _ => false,
        }
    }

    /// The text of a canonical string id.
    pub(crate) fn string(&self, id: u32) -> &str {
        self.module.string(bytecode_lang::StrId(id)).unwrap_or("")
    }
}

fn canon_with(canon_ty: &[u32], v: ValType) -> ValType {
    match v {
        ValType::Ref(t) => ValType::Ref(TypeId(canon_ty.get(t.index()).copied().unwrap_or(t.0))),
        other => other,
    }
}

/// Everything the loader derives, before the module moves into the program.
struct Parts {
    funcs: Vec<FuncInfo>,
    types: Vec<TypeInfo>,
    imports: Vec<ImportInfo>,
    hooks: [Option<Callee>; HOOKS],
    canon_ty: Vec<u32>,
    global_types: Vec<ValType>,
    global_refs: Vec<u32>,
}

impl Parts {
    fn into_program(self, module: Module) -> Program {
        Program {
            module,
            funcs: self.funcs,
            types: self.types,
            imports: self.imports,
            hooks: self.hooks,
            canon_ty: self.canon_ty,
            global_types: self.global_types,
            global_refs: self.global_refs,
        }
    }
}

struct Loader<'m> {
    m: &'m Module,
    canon_ty: Vec<u32>,
}

impl<'m> Loader<'m> {
    fn new(m: &'m Module) -> Loader<'m> {
        Loader {
            m,
            canon_ty: Vec::new(),
        }
    }

    /// Checks a value type's struct reference.
    fn valtype(&self, v: ValType) -> Result<ValType, LoadErrorKind> {
        if let ValType::Ref(t) = v {
            if t.index() >= self.m.types().len() {
                return Err(out_of_range("type", t.0));
            }
        }
        Ok(canon_with(&self.canon_ty, v))
    }

    fn valtypes(&self, vs: &[ValType]) -> Result<Box<[ValType]>, LoadErrorKind> {
        vs.iter().map(|&v| self.valtype(v)).collect()
    }

    /// A `func` type's parameters and result.
    fn signature(&self, t: TypeId) -> Result<(Box<[ValType]>, Option<ValType>), LoadErrorKind> {
        match self.m.type_def(t) {
            Some(TypeDef::Func(sig)) if sig.results.len() <= 1 => Ok((
                self.valtypes(&sig.params)?,
                sig.results.first().map(|&r| self.valtype(r)).transpose()?,
            )),
            Some(_) => Err(LoadErrorKind::BadSignature),
            None => Err(out_of_range("type", t.0)),
        }
    }

    fn run(mut self, host: &Host) -> Result<Parts, LoadError> {
        let m = self.m;
        let canon_str = canonical_strings(m);
        self.canon_ty = canonical_types(m);
        let types = self.types(&canon_str).map_err(LoadError::new)?;
        self.consts().map_err(LoadError::new)?;
        let imports = self.imports(host)?;
        let mut global_types = Vec::with_capacity(m.globals().len());
        let mut global_refs = Vec::new();
        for (i, g) in m.globals().iter().enumerate() {
            let ty = self.valtype(g.ty).map_err(LoadError::new)?;
            if let Some(k) = g.init {
                if k.index() >= m.consts().len() {
                    return Err(LoadError::new(out_of_range("constant", k.0)));
                }
            }
            if ty.is_reference() {
                global_refs.push(count(i));
            }
            global_types.push(ty);
        }
        let mut funcs = Vec::with_capacity(m.functions().len());
        for (i, f) in m.functions().iter().enumerate() {
            funcs.push(self.func_info(count(i), f, &canon_str)?);
        }
        for (i, f) in m.functions().iter().enumerate() {
            self.check_code(count(i), f, &funcs, &imports)?;
        }
        let hooks = self.hooks(&funcs, &imports)?;
        if let Some(start) = m.start() {
            let f = funcs
                .get(start.index())
                .ok_or_else(|| LoadError::new(out_of_range("function", start.0)))?;
            if f.nparams != 0 || f.result.is_some() {
                return Err(LoadError::new(LoadErrorKind::BadStart));
            }
            if !f.captures.is_empty() {
                return Err(LoadError::new(LoadErrorKind::CalleeHasCaptures));
            }
        }
        for e in m.exports() {
            let (what, index, len) = match e.item {
                ExportItem::Func(f) => ("function", f.0, m.functions().len()),
                ExportItem::Global(g) => ("global", g.0, m.globals().len()),
                ExportItem::Type(t) => ("type", t.0, m.types().len()),
            };
            if index as usize >= len {
                return Err(LoadError::new(out_of_range(what, index)));
            }
        }
        Ok(Parts {
            funcs,
            types,
            imports,
            hooks,
            canon_ty: self.canon_ty,
            global_types,
            global_refs,
        })
    }

    fn types(&self, canon_str: &[u32]) -> Result<Vec<TypeInfo>, LoadErrorKind> {
        let m = self.m;
        let defs = m.types();
        let mut out = Vec::with_capacity(defs.len());
        for def in defs {
            out.push(match def {
                TypeDef::Func(sig) => {
                    if sig.results.len() > 1 {
                        return Err(LoadErrorKind::BadSignature);
                    }
                    let _params = self.valtypes(&sig.params)?;
                    if let Some(&r) = sig.results.first() {
                        let _result = self.valtype(r)?;
                    }
                    TypeInfo::Func
                }
                TypeDef::Struct(s) => {
                    let str_id = |id: bytecode_lang::StrId| {
                        canon_str
                            .get(id.index())
                            .copied()
                            .ok_or(out_of_range("string", id.0))
                    };
                    let fields = s
                        .fields
                        .iter()
                        .map(|f| self.valtype(f.ty))
                        .collect::<Result<Box<[ValType]>, _>>()?;
                    let ref_fields = fields
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| t.is_reference())
                        .map(|(i, _)| u16::try_from(i).unwrap_or(u16::MAX))
                        .collect();
                    let mut field_names = Vec::with_capacity(s.fields.len());
                    for (i, f) in s.fields.iter().enumerate() {
                        let Ok(slot) = u16::try_from(i) else {
                            break; // slots past 65535 are unaddressable
                        };
                        field_names.push((str_id(f.name)?, slot));
                    }
                    // Stable sort: the first field with a name wins.
                    field_names.sort_by_key(|&(n, _)| n);
                    field_names.dedup_by_key(|&mut (n, _)| n);
                    let mut methods = Vec::with_capacity(s.methods.len());
                    for meth in &s.methods {
                        if meth.func.index() >= m.functions().len() {
                            return Err(out_of_range("function", meth.func.0));
                        }
                        methods.push((str_id(meth.name)?, meth.func.0));
                    }
                    methods.sort_by_key(|&(n, _)| n);
                    methods.dedup_by_key(|&mut (n, _)| n);
                    let _ = str_id(s.name)?;
                    TypeInfo::Struct(StructInfo {
                        parent: s.parent.map(|p| p.0),
                        fields,
                        ref_fields,
                        field_names: field_names.into_boxed_slice(),
                        methods: methods.into_boxed_slice(),
                        pre: 0,
                        post: 0,
                    })
                }
                TypeDef::Array(e) => TypeInfo::Array(self.valtype(*e)?),
                TypeDef::Map { key, value } => {
                    TypeInfo::Map(self.valtype(*key)?, self.valtype(*value)?)
                }
                TypeDef::Cell(e) => TypeInfo::Cell(self.valtype(*e)?),
                TypeDef::Iter { key, value } => {
                    TypeInfo::Iter(self.valtype(*key)?, self.valtype(*value)?)
                }
                TypeDef::Coroutine => TypeInfo::Coroutine,
            });
        }
        check_inheritance(defs, &mut out)?;
        Ok(out)
    }

    fn consts(&self) -> Result<(), LoadErrorKind> {
        let m = self.m;
        let mut depth: Vec<u32> = Vec::with_capacity(m.consts().len());
        for (i, k) in m.consts().iter().enumerate() {
            let d = match k {
                Const::Str(s) => {
                    if s.index() >= m.string_count() {
                        return Err(out_of_range("string", s.0));
                    }
                    0
                }
                Const::Array(items) => child_depth(items.iter().copied(), i, &depth)?,
                Const::Map(entries) => {
                    child_depth(entries.iter().flat_map(|&(k, v)| [k, v]), i, &depth)?
                }
                _ => 0,
            };
            if d > MAX_CONST_DEPTH {
                return Err(LoadErrorKind::BadConstant);
            }
            depth.push(d);
        }
        Ok(())
    }

    fn imports(&self, host: &Host) -> Result<Vec<ImportInfo>, LoadError> {
        let m = self.m;
        let mut out = Vec::with_capacity(m.imports().len());
        for imp in m.imports() {
            let (params, result) = self.signature(imp.sig).map_err(LoadError::new)?;
            let (Some(module), Some(name)) = (m.string(imp.module), m.string(imp.name)) else {
                return Err(LoadError::new(out_of_range(
                    "string",
                    imp.module.0.max(imp.name.0),
                )));
            };
            let func = host.lookup(module, name).cloned().ok_or_else(|| {
                LoadError::new(LoadErrorKind::UnresolvedImport {
                    module: module.to_string(),
                    name: name.to_string(),
                })
            })?;
            out.push(ImportInfo {
                params,
                result,
                sig: self
                    .canon_ty
                    .get(imp.sig.index())
                    .copied()
                    .unwrap_or(imp.sig.0),
                func,
            });
        }
        Ok(out)
    }

    fn func_info(
        &self,
        id: u32,
        f: &bytecode_lang::Function,
        canon_str: &[u32],
    ) -> Result<FuncInfo, LoadError> {
        let err = |k| LoadError::at(k, id, None);
        let (params, result) = self.signature(f.sig()).map_err(err)?;
        let regs = self.valtypes(f.regs()).map_err(err)?;
        if regs.len() > usize::from(u16::MAX) + 1 {
            return Err(err(out_of_range("register", count(regs.len()))));
        }
        if regs.len() < params.len() || regs.iter().zip(params.iter()).any(|(a, b)| a != b) {
            return Err(err(LoadErrorKind::ParamMismatch));
        }
        let ref_regs = regs
            .iter()
            .enumerate()
            .filter(|(_, t)| t.is_reference())
            .map(|(i, _)| u16::try_from(i).unwrap_or(u16::MAX))
            .collect();
        let captures = self.valtypes(f.captures()).map_err(err)?;
        let names = f
            .names()
            .iter()
            .map(|s| {
                canon_str
                    .get(s.index())
                    .copied()
                    .ok_or(out_of_range("string", s.0))
            })
            .collect::<Result<Box<[u32]>, _>>()
            .map_err(err)?;
        let type_refs = f
            .type_refs()
            .iter()
            .map(|t| {
                self.canon_ty
                    .get(t.index())
                    .copied()
                    .ok_or(out_of_range("type", t.0))
            })
            .collect::<Result<Box<[u32]>, _>>()
            .map_err(err)?;
        Ok(FuncInfo {
            sig: self
                .canon_ty
                .get(f.sig().index())
                .copied()
                .unwrap_or(f.sig().0),
            nregs: regs.len(),
            nparams: params.len(),
            regs,
            result,
            ref_regs,
            captures,
            names,
            type_refs,
            catch_table: (f.handlers().len() > CATCH_TABLE_MIN)
                .then(|| catch_table(f.code().len(), f.handlers())),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn check_code(
        &self,
        id: u32,
        f: &bytecode_lang::Function,
        funcs: &[FuncInfo],
        imports: &[ImportInfo],
    ) -> Result<(), LoadError> {
        let Some(info) = funcs.get(id as usize) else {
            return Err(LoadError::new(out_of_range("function", id)));
        };
        let code = f.code();
        let len = code.len();
        let Some(last) = code.last() else {
            return Err(LoadError::at(LoadErrorKind::EmptyCode, id, None));
        };
        if !is_terminator(last.opcode()) {
            return Err(LoadError::at(
                LoadErrorKind::FallsThrough,
                id,
                Some(count(len - 1)),
            ));
        }
        let nregs = info.nregs;
        // Handlers and coverage (for the tail-call rule).
        let mut cover = vec![0i32; len + 1];
        for h in f.handlers() {
            let bad = h.start >= h.end
                || h.end as usize > len
                || h.target.index() >= len
                || h.catch.index() >= nregs;
            if bad {
                return Err(LoadError::at(LoadErrorKind::InvalidHandler, id, None));
            }
            if info.regs.get(h.catch.index()) != Some(&ValType::Dyn) {
                return Err(LoadError::at(LoadErrorKind::CatchNotDyn, id, None));
            }
            if let Some(c) = cover.get_mut(h.start as usize) {
                *c += 1;
            }
            if let Some(c) = cover.get_mut(h.end as usize) {
                *c -= 1;
            }
        }
        for t in f.tables() {
            for target in t.targets.iter().chain(core::iter::once(&t.default)) {
                if target.index() >= len {
                    return Err(LoadError::at(
                        out_of_range("table target", target.0),
                        id,
                        None,
                    ));
                }
            }
        }
        let mut covered = 0i32;
        for (pc, inst) in code.iter().enumerate() {
            covered += cover.get(pc).copied().unwrap_or(0);
            let at = |k| LoadError::at(k, id, Some(count(pc)));
            self.check_fields(inst, info, f, len).map_err(at)?;
            self.check_inst(inst, info, funcs, imports, covered > 0)
                .map_err(at)?;
        }
        Ok(())
    }

    /// Range-checks every index operand, generically from the opcode's
    /// field table.
    fn check_fields(
        &self,
        inst: &Inst,
        info: &FuncInfo,
        f: &bytecode_lang::Function,
        len: usize,
    ) -> Result<(), LoadErrorKind> {
        let m = self.m;
        let word = u64::from_le_bytes(inst.to_bytes());
        for field in inst.opcode().fields() {
            let width = field.slot.width();
            let raw = ((word >> field.slot.shift()) & ((1u64 << width) - 1)) as u32;
            let (what, limit) = match field.kind {
                FieldKind::Reg => ("register", info.nregs),
                FieldKind::Target => ("branch target", len),
                FieldKind::Const => ("constant", m.consts().len()),
                FieldKind::Func => ("function", m.functions().len()),
                FieldKind::Import => ("import", m.imports().len()),
                FieldKind::Global => ("global", m.globals().len()),
                FieldKind::Table => ("table", f.tables().len()),
                FieldKind::Name => ("name", info.names.len()),
                FieldKind::TypeRef => ("type ref", info.type_refs.len()),
                FieldKind::Upval => ("capture", info.captures.len()),
                _ => continue,
            };
            if raw as usize >= limit {
                return Err(out_of_range(what, raw));
            }
        }
        Ok(())
    }

    /// The opcode-specific rules: windows, arities, type kinds, promote,
    /// modifiers, tail calls.
    #[allow(clippy::too_many_lines)]
    fn check_inst(
        &self,
        inst: &Inst,
        info: &FuncInfo,
        funcs: &[FuncInfo],
        imports: &[ImportInfo],
        in_try: bool,
    ) -> Result<(), LoadErrorKind> {
        let nregs = info.nregs;
        // `first..first+n` must lie inside the frame.
        let window = |first: usize, n: usize| -> Result<(), LoadErrorKind> {
            if first + n > nregs {
                Err(out_of_range("register", count(first + n - 1)))
            } else {
                Ok(())
            }
        };
        let callee = |func: FuncId| {
            funcs
                .get(func.index())
                .ok_or(out_of_range("function", func.0))
        };
        let type_kind = |r: bytecode_lang::TypeRef, expected: &'static str| {
            let t = info.type_refs.get(r.index()).copied().unwrap_or(u32::MAX);
            let ok = matches!(
                (self.m.types().get(t as usize), expected),
                (Some(TypeDef::Struct(_)), "a struct")
                    | (Some(TypeDef::Array(_)), "an array")
                    | (Some(TypeDef::Map { .. }), "a map")
                    | (Some(TypeDef::Cell(_)), "a cell")
            );
            if ok {
                Ok(())
            } else {
                Err(LoadErrorKind::WrongTypeKind { expected })
            }
        };
        let no_promote = |o: Option<Overflow>| {
            if o == Some(Overflow::Promote) {
                Err(LoadErrorKind::PromoteNotDynamic)
            } else {
                Ok(())
            }
        };
        match *inst {
            Inst::IAdd { .. }
            | Inst::ISub { .. }
            | Inst::IMul { .. }
            | Inst::IDiv { .. }
            | Inst::IRem { .. }
            | Inst::IFloorDiv { .. }
            | Inst::IFloorMod { .. }
            | Inst::IAnd { .. }
            | Inst::IOr { .. }
            | Inst::IXor { .. }
            | Inst::IShl { .. }
            | Inst::IShr { .. }
            | Inst::IMin { .. }
            | Inst::IMax { .. }
            | Inst::INeg { .. }
            | Inst::INot { .. }
            | Inst::IAbs { .. }
            | Inst::IntCast { .. }
            | Inst::F32ToInt { .. }
            | Inst::F64ToInt { .. } => no_promote(inst.overflow())?,
            Inst::DAdd { dst, pol, .. }
            | Inst::DSub { dst, pol, .. }
            | Inst::DMul { dst, pol, .. }
            | Inst::DDiv { dst, pol, .. }
            | Inst::DRem { dst, pol, .. }
            | Inst::DFloorDiv { dst, pol, .. }
            | Inst::DFloorMod { dst, pol, .. }
            | Inst::DAnd { dst, pol, .. }
            | Inst::DOr { dst, pol, .. }
            | Inst::DXor { dst, pol, .. }
            | Inst::DShl { dst, pol, .. }
            | Inst::DShr { dst, pol, .. }
            | Inst::DNeg { dst, pol, .. }
            | Inst::DNot { dst, pol, .. }
                if pol.overflow() == Overflow::Promote
                    && info.regs.get(dst.index()) != Some(&ValType::Dyn) =>
            {
                return Err(LoadErrorKind::PromoteNotDynamic);
            }
            Inst::FromDyn { to: Prim::Ref, .. } => return Err(LoadErrorKind::BadModifier),
            Inst::FloatToBits { ty, .. } | Inst::BitsToFloat { ty, .. }
                if !matches!(ty.bits(), 32 | 64) =>
            {
                return Err(LoadErrorKind::BadModifier);
            }
            Inst::Call { dst, func, argc } => {
                let c = callee(func)?;
                window(dst.index() + 1, usize::from(argc))?;
                arity(c.nparams, argc)?;
                if !c.captures.is_empty() {
                    return Err(LoadErrorKind::CalleeHasCaptures);
                }
            }
            Inst::CallIndirect { dst, argc, .. }
            | Inst::DCall { dst, argc, .. }
            | Inst::CoroNewIndirect { dst, argc, .. }
            | Inst::Spawn { dst, argc, .. } => window(dst.index() + 1, usize::from(argc))?,
            Inst::CoroNew { dst, func, argc } => {
                let _body = callee(func)?;
                window(dst.index() + 1, usize::from(argc))?;
            }
            Inst::CallImport { dst, import, argc } => {
                let imp = imports
                    .get(import.index())
                    .ok_or(out_of_range("import", import.0))?;
                window(dst.index() + 1, usize::from(argc))?;
                arity(imp.params.len(), argc)?;
            }
            Inst::TailCall { func, args, argc } => {
                let c = callee(func)?;
                window(args.index(), usize::from(argc))?;
                arity(c.nparams, argc)?;
                if !c.captures.is_empty() {
                    return Err(LoadErrorKind::CalleeHasCaptures);
                }
                if in_try {
                    return Err(LoadErrorKind::TailCallInTry);
                }
            }
            Inst::TailCallIndirect { args, argc, .. } => {
                window(args.index(), usize::from(argc))?;
                if in_try {
                    return Err(LoadErrorKind::TailCallInTry);
                }
            }
            Inst::MakeClosure { dst, func } => {
                let c = callee(func)?;
                window(dst.index() + 1, c.captures.len())?;
            }
            Inst::NewStruct { ty, .. } => type_kind(ty, "a struct")?,
            Inst::NewArray { ty, .. } => type_kind(ty, "an array")?,
            Inst::NewMap { ty, .. } => type_kind(ty, "a map")?,
            Inst::NewCell { ty, .. } => type_kind(ty, "a cell")?,
            Inst::StrConcatN {
                first, count: n, ..
            } => window(first.index(), usize::from(n))?,
            Inst::StrSlice { range, .. } => window(range.index(), 2)?,
            _ => {}
        }
        Ok(())
    }

    fn hooks(
        &self,
        funcs: &[FuncInfo],
        imports: &[ImportInfo],
    ) -> Result<[Option<Callee>; HOOKS], LoadError> {
        let mut out = [None; HOOKS];
        for b in self.m.hooks() {
            let (n, has_result) = hook_shape(b.hook);
            let (params, result, captures): (&[ValType], Option<ValType>, bool) = match b.callee {
                Callee::Func(f) => {
                    let info = funcs
                        .get(f.index())
                        .ok_or_else(|| LoadError::new(out_of_range("function", f.0)))?;
                    (
                        &info.regs[..info.nparams],
                        info.result,
                        !info.captures.is_empty(),
                    )
                }
                Callee::Import(i) => {
                    let info = imports
                        .get(i.index())
                        .ok_or_else(|| LoadError::new(out_of_range("import", i.0)))?;
                    (&info.params, info.result, false)
                }
            };
            let ok = !captures
                && params.len() == n
                && params.iter().all(|&p| p == ValType::Dyn)
                && matches!(
                    (has_result, result),
                    (true, Some(ValType::Dyn)) | (false, None)
                );
            if !ok {
                return Err(LoadError::new(LoadErrorKind::BadHook(b.hook)));
            }
            if let Some(slot) = out.get_mut(usize::from(b.hook.code())) {
                *slot = Some(b.callee);
            }
        }
        Ok(out)
    }
}

fn arity(expected: usize, found: u8) -> Result<(), LoadErrorKind> {
    if expected == usize::from(found) {
        Ok(())
    } else {
        Err(LoadErrorKind::ArityMismatch {
            expected: count(expected),
            found: u32::from(found),
        })
    }
}

fn child_depth(
    children: impl Iterator<Item = bytecode_lang::ConstId>,
    own: usize,
    depth: &[u32],
) -> Result<u32, LoadErrorKind> {
    let mut d = 0;
    for c in children {
        if c.index() >= own {
            return Err(LoadErrorKind::BadConstant);
        }
        d = d.max(depth.get(c.index()).copied().unwrap_or(0) + 1);
    }
    Ok(d)
}

/// Maps every string id to the first id with the same text, so names compare
/// as integers.
fn canonical_strings(m: &Module) -> Vec<u32> {
    let mut first: BTreeMap<&str, u32> = BTreeMap::new();
    m.strings()
        .enumerate()
        .map(|(i, s)| *first.entry(s).or_insert(count(i)))
        .collect()
}

/// Maps every type id to the first structurally equal type (structs are
/// nominal: each is its own).
fn canonical_types(m: &Module) -> Vec<u32> {
    let mut first: BTreeMap<&TypeDef, u32> = BTreeMap::new();
    m.types()
        .iter()
        .enumerate()
        .map(|(i, t)| match t {
            TypeDef::Struct(_) => count(i),
            _ => *first.entry(t).or_insert(count(i)),
        })
        .collect()
}

/// Checks parents (structs, acyclic, at most [`MAX_INHERITANCE_DEPTH`] deep,
/// field prefixes) and numbers the inheritance forest for O(1) descent
/// tests.
fn check_inheritance(defs: &[TypeDef], out: &mut [TypeInfo]) -> Result<(), LoadErrorKind> {
    let parent_of = |t: usize| match defs.get(t) {
        Some(TypeDef::Struct(s)) => s.parent.map(|p| p.index()),
        _ => None,
    };
    for (t, def) in defs.iter().enumerate() {
        let TypeDef::Struct(s) = def else { continue };
        let Some(p) = s.parent else { continue };
        let Some(TypeDef::Struct(ps)) = defs.get(p.index()) else {
            return Err(LoadErrorKind::BadParent);
        };
        let prefix_ok = ps.fields.len() <= s.fields.len()
            && ps.fields.iter().zip(s.fields.iter()).all(|(a, b)| a == b);
        if !prefix_ok {
            return Err(LoadErrorKind::BadParent);
        }
        // Walk up; a chain longer than the cap is too deep or a cycle.
        let mut cur = Some(t);
        let mut steps = 0;
        while let Some(c) = cur {
            steps += 1;
            if steps > MAX_INHERITANCE_DEPTH {
                return Err(LoadErrorKind::InheritanceTooDeep);
            }
            cur = parent_of(c);
        }
    }
    // Children lists, then an iterative preorder walk of each root.
    let mut children: Vec<Vec<u32>> = vec![Vec::new(); defs.len()];
    let mut roots = Vec::new();
    for (t, def) in defs.iter().enumerate() {
        if let TypeDef::Struct(s) = def {
            match s.parent {
                Some(p) => {
                    if let Some(c) = children.get_mut(p.index()) {
                        c.push(count(t));
                    }
                }
                None => roots.push(count(t)),
            }
        }
    }
    let mut clock = 0u32;
    let mut stack: Vec<(u32, usize)> = Vec::new();
    for root in roots {
        stack.push((root, 0));
        if let Some(TypeInfo::Struct(s)) = out.get_mut(root as usize) {
            s.pre = clock;
        }
        clock += 1;
        while let Some(&mut (node, ref mut next)) = stack.last_mut() {
            let kids = children.get(node as usize).map_or(&[][..], Vec::as_slice);
            if let Some(&child) = kids.get(*next) {
                *next += 1;
                if let Some(TypeInfo::Struct(s)) = out.get_mut(child as usize) {
                    s.pre = clock;
                }
                clock += 1;
                stack.push((child, 0));
            } else {
                if let Some(TypeInfo::Struct(s)) = out.get_mut(node as usize) {
                    s.post = clock;
                }
                let _ = stack.pop();
            }
        }
    }
    Ok(())
}
