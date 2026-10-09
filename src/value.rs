//! The value type the VM exchanges with its host: entry arguments, results,
//! host-function arguments and results, and uncaught thrown values.

use core::fmt;

/// A value crossing between the VM and the host.
///
/// Inside the VM every register is a 64-bit slot whose meaning comes from the
/// register's declared LSB type; `Value` is that slot decoded. Integers of
/// signed types and `dyn` ints are [`Int`](Value::Int), unsigned types are
/// [`UInt`](Value::UInt), `f64` and `dyn` floats are [`Float`](Value::Float),
/// `f32` is [`F32`](Value::F32), and every heap object (string, array, map,
/// struct, function, cell, iterator, error) is an [`Obj`](Value::Obj) handle
/// that [`Vm`](crate::Vm) can inspect.
///
/// # Examples
///
/// ```
/// use bvm_lang::Value;
///
/// let v = Value::Int(-3);
/// assert_eq!(v.as_int(), Some(-3));
/// assert_eq!(Value::Nil.as_int(), None);
/// assert_eq!(v.to_string(), "-3");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum Value {
    /// `nil`: the null reference, and the result of a void function.
    #[default]
    Nil,
    /// A boolean.
    Bool(bool),
    /// A signed integer (`i8`..`i64`) or a `dyn` int.
    Int(i64),
    /// An unsigned integer (`u8`..`u64`).
    UInt(u64),
    /// An `f32`.
    F32(f32),
    /// An `f64` or a `dyn` float.
    Float(f64),
    /// A character (Unicode scalar value).
    Char(char),
    /// A heap object, inspected through the [`Vm`](crate::Vm) that made it.
    Obj(Obj),
}

/// A handle to a heap object of one [`Vm`](crate::Vm).
///
/// A handle is valid while the object is reachable: objects are collected
/// only while bytecode runs, so a handle returned by a run stays valid until
/// the next run on the same VM. A handle to a collected object reads as
/// `nil`, never as another object (slots are generation-checked and retired
/// before their generation could wrap).
///
/// # Examples
///
/// ```
/// use bvm_lang::{Program, Value, Vm};
/// use bytecode_lang::ModuleBuilder;
///
/// let mut m = ModuleBuilder::new();
/// let mut f = m.function("f", &[], &[]);
/// f.ret_void();
/// m.add_function(f).unwrap();
/// let program = Program::load(m.finish().unwrap(), &Default::default()).unwrap();
/// let mut vm = Vm::new(&program);
/// let s = vm.new_str(b"hi").unwrap();
/// assert!(matches!(s, Value::Obj(_)));
/// assert_eq!(vm.str_bytes(s), Some(&b"hi"[..]));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Obj(pub(crate) u64);

impl Value {
    /// The integer, for [`Int`](Value::Int) and for a [`UInt`](Value::UInt)
    /// that fits in `i64`.
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Value;
    ///
    /// assert_eq!(Value::UInt(7).as_int(), Some(7));
    /// assert_eq!(Value::UInt(u64::MAX).as_int(), None);
    /// ```
    #[must_use]
    pub fn as_int(self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(i),
            Value::UInt(u) => i64::try_from(u).ok(),
            _ => None,
        }
    }

    /// The float, for [`Float`](Value::Float) and (widened)
    /// [`F32`](Value::F32).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Value;
    ///
    /// assert_eq!(Value::F32(1.5).as_float(), Some(1.5));
    /// assert_eq!(Value::Int(1).as_float(), None);
    /// ```
    #[must_use]
    pub fn as_float(self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(f),
            Value::F32(f) => Some(f64::from(f)),
            _ => None,
        }
    }

    /// The boolean, for [`Bool`](Value::Bool).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Value;
    ///
    /// assert_eq!(Value::Bool(true).as_bool(), Some(true));
    /// ```
    #[must_use]
    pub fn as_bool(self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// Whether this is [`Nil`](Value::Nil).
    ///
    /// # Examples
    ///
    /// ```
    /// use bvm_lang::Value;
    ///
    /// assert!(Value::Nil.is_nil());
    /// assert!(!Value::Int(0).is_nil());
    /// ```
    #[must_use]
    pub fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }
}

impl fmt::Display for Value {
    /// Scalars print as their value; heap objects as `<obj>` (the [`Vm`]
    /// that owns them can describe them).
    ///
    /// [`Vm`]: crate::Vm
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => f.write_str("nil"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::UInt(u) => write!(f, "{u}"),
            Value::F32(x) => write!(f, "{x:?}"),
            Value::Float(x) => write!(f, "{x:?}"),
            Value::Char(c) => write!(f, "{c:?}"),
            Value::Obj(_) => f.write_str("<obj>"),
        }
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<u64> for Value {
    fn from(u: u64) -> Self {
        Value::UInt(u)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<char> for Value {
    fn from(c: char) -> Self {
        Value::Char(c)
    }
}
