//! A deliberately simple reference interpreter for a subset of LSB, written
//! independently of the crate (values as `i128`/`f64`/enums, no NaN boxing,
//! no shared arithmetic), used by the differential property tests.
//!
//! Subset: integer and float arithmetic and comparisons at every policy,
//! conversions between them, `bool` logic, `to_dyn`/`from_dyn`, the dynamic
//! numeric instructions (every policy, `promote` included) and comparisons,
//! moves, constants, `safepoint`, `jmp`/`jmp_if`/`jmp_if_not`/`switch`, and
//! `ret`. Fuel is charged exactly as the VM documents: one unit per
//! `safepoint` and per taken backward branch.

#![allow(dead_code, clippy::unwrap_used, clippy::too_many_lines)]

use bytecode_lang::{
    DivZero, ErrorKind, FloatToInt, FloatTy, Inst, IntConv, IntOp, IntTy, JumpTable, Overflow,
    Policy, Prim, Shift, ValType,
};

/// A dynamic value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dv {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
}

/// A register's contents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rv {
    /// An integer at its register's type.
    Int(i128),
    Float(f64),
    F32(f32),
    Bool(bool),
    Dyn(Dv),
}

/// How a run ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Ret(Rv),
    Raised(ErrorKind, u32),
    Trapped(ErrorKind, u32),
}

pub fn range(ty: IntTy) -> (i128, i128) {
    let bits = ty.bits();
    if ty.is_signed() {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    }
}

pub fn wrap(ty: IntTy, v: i128) -> i128 {
    let m = 1i128 << ty.bits();
    let low = v.rem_euclid(m);
    if ty.is_signed() && low >= m / 2 {
        low - m
    } else {
        low
    }
}

type R<T> = Result<T, (ErrorKind, bool)>;

fn overflow(o: Overflow, wrapped: i128) -> R<i128> {
    match o {
        Overflow::Wrap => Ok(wrapped),
        Overflow::Trap => Err((ErrorKind::ArithOverflow, true)),
        _ => Err((ErrorKind::ArithOverflow, false)),
    }
}

fn fit(ty: IntTy, o: Overflow, exact: i128) -> R<i128> {
    let (lo, hi) = range(ty);
    if (lo..=hi).contains(&exact) {
        Ok(exact)
    } else {
        overflow(o, wrap(ty, exact))
    }
}

fn dz(p: Policy) -> R<i128> {
    Err((ErrorKind::DivByZero, p.div_zero() == DivZero::Trap))
}

fn fdiv(a: i128, b: i128) -> i128 {
    let q = a / b;
    if a % b != 0 && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// The OPS integer binary operations by name.
pub fn int_bin(name: &str, ty: IntTy, p: Policy, a: i128, b: i128) -> R<i128> {
    let o = p.overflow();
    let bits = i128::from(ty.bits());
    match name {
        "add" => fit(ty, o, a + b),
        "sub" => fit(ty, o, a - b),
        "mul" => match a.checked_mul(b) {
            Some(exact) => fit(ty, o, exact),
            None => overflow(o, wrap(ty, a.wrapping_mul(b))),
        },
        "div" => {
            if b == 0 {
                dz(p)
            } else {
                fit(ty, o, a / b)
            }
        }
        "rem" => {
            if b == 0 {
                dz(p)
            } else {
                Ok(a % b)
            }
        }
        "floor_div" => {
            if b == 0 {
                dz(p)
            } else {
                fit(ty, o, fdiv(a, b))
            }
        }
        "floor_mod" => {
            if b == 0 {
                dz(p)
            } else {
                Ok(a - b * fdiv(a, b))
            }
        }
        "and" => Ok(wrap(ty, a & b)),
        "or" => Ok(wrap(ty, a | b)),
        "xor" => Ok(wrap(ty, a ^ b)),
        "shl" | "shr" => {
            let n = if b < 0 || b >= bits {
                match p.shift() {
                    Shift::Mask => b.rem_euclid(bits),
                    // PHP: 0, or -1 for a negative value shifted right; a
                    // negative amount is still an error (OPS v2).
                    Shift::Saturate if b >= 0 => {
                        return Ok(if name == "shr" && a < 0 { -1 } else { 0 });
                    }
                    _ => return Err((ErrorKind::ShiftOutOfRange, false)),
                }
            } else {
                b
            };
            if name == "shl" {
                Ok(wrap(ty, ((a as u128) << n) as i128))
            } else {
                Ok(a >> n)
            }
        }
        "min" => Ok(a.min(b)),
        "max" => Ok(a.max(b)),
        "pow" => {
            // OPS v2: a negative exponent is never an overflow.
            if b < 0 {
                return Err((ErrorKind::NegativeExponent, false));
            }
            let exact = match a {
                0 => Some(i128::from(b == 0)),
                1 => Some(1),
                -1 => Some(if b % 2 == 0 { 1 } else { -1 }),
                // |a| >= 2: anything past 2^127 overflows every type.
                _ if b >= 127 => None,
                _ => (0..b).try_fold(1i128, |acc, _| acc.checked_mul(a)),
            };
            // The low bits of the exact power, by square and multiply with
            // wrapping at the type after every product.
            let (mut low, mut base, mut e) = (wrap(ty, 1), wrap(ty, a), b);
            while e > 0 {
                if e & 1 == 1 {
                    low = wrap(ty, low * base);
                }
                base = wrap(ty, base * base);
                e >>= 1;
            }
            match exact {
                Some(x) => fit(ty, o, x),
                None => overflow(o, low),
            }
        }
        _ => unreachable!("{name}"),
    }
}

/// The correctly rounded quotient of two i64s (long division, 66 bits plus
/// a sticky bit, round to nearest even).
pub fn quotient(a: i64, b: i64) -> f64 {
    let neg = (a < 0) != (b < 0);
    let (n, d) = (u128::from(a.unsigned_abs()), u128::from(b.unsigned_abs()));
    if n == 0 {
        return if neg { -0.0 } else { 0.0 };
    }
    let mut q = n / d;
    let mut r = n % d;
    let mut exp = 0i32;
    while q < (1u128 << 66) {
        q <<= 1;
        r <<= 1;
        if r >= d {
            q |= 1;
            r -= d;
        }
        exp -= 1;
    }
    let len = 128 - q.leading_zeros() as i32;
    let drop = len - 53;
    let mut m = q >> drop;
    let rest = q & ((1u128 << drop) - 1);
    let half = 1u128 << (drop - 1);
    if rest > half || (rest == half && (r != 0 || m & 1 == 1)) {
        m += 1;
    }
    let v = (m as f64) * 2f64.powi(exp + drop);
    if neg { -v } else { v }
}

/// CPython's float floor division and modulo (LSB §5.6).
pub fn py(a: f64, b: f64) -> (f64, f64) {
    let mut m = a % b;
    let mut q = (a - m) / b;
    if m != 0.0 || m.is_nan() {
        if (b < 0.0) != (m < 0.0) {
            m += b;
            q -= 1.0;
        }
    } else {
        m = 0f64.copysign(b);
    }
    let fq = if q != 0.0 || q.is_nan() {
        let mut fq = q.floor();
        if q - fq > 0.5 {
            fq += 1.0;
        }
        fq
    } else {
        0f64.copysign(a / b)
    };
    (fq, m)
}

/// A dynamic arithmetic instruction's fast path; `None` means "hook"
/// (here: `TypeError`, no hooks in generated programs).
pub fn dyn_bin(name: &str, p: Policy, a: Dv, b: Dv) -> Option<R<Dv>> {
    match (a, b) {
        (Dv::Int(x), Dv::Int(y)) => {
            let (x1, y1) = (i128::from(x), i128::from(y));
            if p.overflow() == Overflow::Promote {
                let promoted = |e: i128| match i64::try_from(e) {
                    Ok(i) => Dv::Int(i),
                    Err(_) => Dv::Float(e as f64),
                };
                match name {
                    "add" => return Some(Ok(promoted(x1 + y1))),
                    "sub" => return Some(Ok(promoted(x1 - y1))),
                    "mul" => return Some(Ok(promoted(x1 * y1))),
                    "div" => {
                        if y == 0 {
                            return Some(dz(p).map(|_| Dv::Nil));
                        }
                        if x1 % y1 == 0 && i64::try_from(x1 / y1).is_ok() {
                            return Some(Ok(Dv::Int((x1 / y1) as i64)));
                        }
                        return Some(Ok(Dv::Float(quotient(x, y))));
                    }
                    "floor_div" if y != 0 && i64::try_from(fdiv(x1, y1)).is_err() => {
                        return Some(Ok(Dv::Float(fdiv(x1, y1) as f64)));
                    }
                    // PHP's `**`: the exact power, its nearest f64 when it
                    // does not fit, the float rule for a negative exponent.
                    "pow" if y < 0 => {
                        return Some(Ok(Dv::Float(super::lspow::ls_pow(x as f64, y as f64))));
                    }
                    "pow" => {
                        let r = int_bin("pow", IntTy::I64, Policy::new(), x1, y1);
                        return Some(Ok(match r {
                            Ok(v) => Dv::Int(v as i64),
                            Err(_) => Dv::Float(super::lspow::exact_pow_f64(x, y)),
                        }));
                    }
                    _ => {}
                }
            }
            Some(int_bin(name, IntTy::I64, p, x1, y1).map(|v| Dv::Int(v as i64)))
        }
        (Dv::Int(_) | Dv::Float(_), Dv::Int(_) | Dv::Float(_)) => {
            let f = |d: Dv| match d {
                Dv::Int(i) => i as f64,
                Dv::Float(f) => f,
                _ => 0.0,
            };
            let (x, y) = (f(a), f(b));
            Some(Ok(Dv::Float(match name {
                "add" => x + y,
                "sub" => x - y,
                "mul" => x * y,
                "div" => x / y,
                "rem" => x % y,
                "floor_div" => py(x, y).0,
                "floor_mod" => py(x, y).1,
                "pow" => super::lspow::ls_pow(x, y),
                _ => return None,
            })))
        }
        _ => None,
    }
}

/// `dabs` on numbers (`None`: the hook, here `TypeError`).
pub fn dyn_abs(p: Policy, a: Dv) -> Option<R<Dv>> {
    match a {
        Dv::Int(i) => Some(match i.checked_abs() {
            Some(v) => Ok(Dv::Int(v)),
            None => match p.overflow() {
                Overflow::Wrap => Ok(Dv::Int(i)),
                Overflow::Promote => Ok(Dv::Float(9_223_372_036_854_775_808.0)),
                Overflow::Trap => Err((ErrorKind::ArithOverflow, true)),
                _ => Err((ErrorKind::ArithOverflow, false)),
            },
        }),
        Dv::Float(f) => Some(Ok(Dv::Float(f.abs()))),
        _ => None,
    }
}

/// Exact numeric comparison of two dynamic numbers.
pub fn dyn_cmp(a: Dv, b: Dv) -> Option<core::cmp::Ordering> {
    // Compare exactly via i128 scaling: an f64 that is integral and within
    // range converts exactly; otherwise compare through the float with a
    // tie-break on the integer side.
    fn num(d: Dv) -> Option<(i128, f64, bool)> {
        match d {
            Dv::Int(i) => Some((i128::from(i), i as f64, true)),
            Dv::Float(f) => Some((0, f, false)),
            _ => None,
        }
    }
    let ((ai, af, a_int), (bi, bf, b_int)) = (num(a)?, num(b)?);
    match (a_int, b_int) {
        (true, true) => Some(ai.cmp(&bi)),
        (false, false) => af.partial_cmp(&bf),
        (true, false) => int_vs_float(ai, bf),
        (false, true) => int_vs_float(bi, af).map(core::cmp::Ordering::reverse),
    }
}

fn int_vs_float(i: i128, f: f64) -> Option<core::cmp::Ordering> {
    use core::cmp::Ordering;
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    // |f| < 2^1024: compare i with floor(f) and the fraction. Work in i128
    // when f is small enough; beyond 2^126 every i64 is smaller in magnitude.
    if f.abs() >= 1e38 {
        return Some(if f > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let fl = f.floor();
    let fi = fl as i128;
    match i.cmp(&fi) {
        Ordering::Equal => Some(if f > fl {
            Ordering::Less
        } else {
            Ordering::Equal
        }),
        o => Some(o),
    }
}

/// Runs `code` over registers of `types` with `args` in the first
/// registers; `tables` are the function's jump tables. Returns the outcome,
/// the fuel used, and the globals (`set_global` targets, in a vector of
/// `nglobals`).
pub fn run(
    code: &[Inst],
    types: &[ValType],
    tables: &[JumpTable],
    args: &[Rv],
    mut fuel: u64,
    nglobals: usize,
) -> (Outcome, u64, Vec<Rv>) {
    let start_fuel = fuel;
    let mut globals = vec![Rv::Int(0); nglobals];
    let mut regs: Vec<Rv> = types
        .iter()
        .map(|t| match t {
            ValType::Bool => Rv::Bool(false),
            ValType::F64 => Rv::Float(0.0),
            ValType::F32 => Rv::F32(0.0),
            ValType::Dyn => Rv::Dyn(Dv::Nil),
            _ => Rv::Int(0),
        })
        .collect();
    for (i, a) in args.iter().enumerate() {
        regs[i] = *a;
    }
    let int = |r: &Rv| match r {
        Rv::Int(i) => *i,
        _ => 0,
    };
    let fl = |r: &Rv| match r {
        Rv::Float(f) => *f,
        _ => 0.0,
    };
    let bo = |r: &Rv| matches!(r, Rv::Bool(true));
    let dy = |r: &Rv| match r {
        Rv::Dyn(d) => *d,
        _ => Dv::Nil,
    };
    let mut pc = 0usize;
    macro_rules! fail {
        ($e:expr) => {{
            let (kind, trap): (ErrorKind, bool) = $e;
            let o = if trap {
                Outcome::Trapped(kind, pc as u32)
            } else {
                Outcome::Raised(kind, pc as u32)
            };
            return (o, start_fuel - fuel, globals);
        }};
    }
    macro_rules! jump {
        ($t:expr) => {{
            let t = $t as usize;
            if t <= pc {
                if fuel == 0 {
                    fail!((ErrorKind::OutOfFuel, true));
                }
                fuel -= 1;
            }
            pc = t;
            continue;
        }};
    }
    loop {
        let inst = code[pc];
        let ibin = |name: &str,
                    d: bytecode_lang::Reg,
                    l: bytecode_lang::Reg,
                    r: bytecode_lang::Reg,
                    op: IntOp,
                    regs: &mut Vec<Rv>|
         -> Result<(), (ErrorKind, bool)> {
            let v = int_bin(
                name,
                op.ty(),
                op.policy(),
                int(&regs[l.index()]),
                int(&regs[r.index()]),
            )?;
            regs[d.index()] = Rv::Int(v);
            Ok(())
        };
        let res: Result<(), (ErrorKind, bool)> = match inst {
            Inst::Nop {} => Ok(()),
            Inst::Mov { dst, src } => {
                regs[dst.index()] = regs[src.index()];
                Ok(())
            }
            Inst::LoadInt { dst, val, ty } => {
                regs[dst.index()] = Rv::Int(wrap(ty, i128::from(val)));
                Ok(())
            }
            Inst::LoadBool { dst, val } => {
                regs[dst.index()] = Rv::Bool(val);
                Ok(())
            }
            Inst::DLoadInt { dst, val } => {
                regs[dst.index()] = Rv::Dyn(Dv::Int(i64::from(val)));
                Ok(())
            }
            Inst::IAdd { dst, lhs, rhs, op } => ibin("add", dst, lhs, rhs, op, &mut regs),
            Inst::ISub { dst, lhs, rhs, op } => ibin("sub", dst, lhs, rhs, op, &mut regs),
            Inst::IMul { dst, lhs, rhs, op } => ibin("mul", dst, lhs, rhs, op, &mut regs),
            Inst::IDiv { dst, lhs, rhs, op } => ibin("div", dst, lhs, rhs, op, &mut regs),
            Inst::IRem { dst, lhs, rhs, op } => ibin("rem", dst, lhs, rhs, op, &mut regs),
            Inst::IFloorDiv { dst, lhs, rhs, op } => {
                ibin("floor_div", dst, lhs, rhs, op, &mut regs)
            }
            Inst::IFloorMod { dst, lhs, rhs, op } => {
                ibin("floor_mod", dst, lhs, rhs, op, &mut regs)
            }
            Inst::IAnd { dst, lhs, rhs, op } => ibin("and", dst, lhs, rhs, op, &mut regs),
            Inst::IOr { dst, lhs, rhs, op } => ibin("or", dst, lhs, rhs, op, &mut regs),
            Inst::IXor { dst, lhs, rhs, op } => ibin("xor", dst, lhs, rhs, op, &mut regs),
            Inst::IShl { dst, lhs, rhs, op } => ibin("shl", dst, lhs, rhs, op, &mut regs),
            Inst::IShr { dst, lhs, rhs, op } => ibin("shr", dst, lhs, rhs, op, &mut regs),
            Inst::IMin { dst, lhs, rhs, op } => ibin("min", dst, lhs, rhs, op, &mut regs),
            Inst::IMax { dst, lhs, rhs, op } => ibin("max", dst, lhs, rhs, op, &mut regs),
            Inst::IPow { dst, lhs, rhs, op } => ibin("pow", dst, lhs, rhs, op, &mut regs),
            Inst::INeg { dst, src, op } => {
                fit(op.ty(), op.policy().overflow(), -int(&regs[src.index()]))
                    .map(|v| regs[dst.index()] = Rv::Int(v))
            }
            Inst::IBitNot { dst, src, op } => {
                regs[dst.index()] = Rv::Int(wrap(op.ty(), !int(&regs[src.index()])));
                Ok(())
            }
            Inst::IAbs { dst, src, op } => {
                let a = int(&regs[src.index()]);
                let r = if op.ty().is_signed() {
                    fit(op.ty(), op.policy().overflow(), a.abs())
                } else {
                    Ok(a)
                };
                r.map(|v| regs[dst.index()] = Rv::Int(v))
            }
            Inst::IEq { dst, lhs, rhs, .. } => {
                regs[dst.index()] = Rv::Bool(int(&regs[lhs.index()]) == int(&regs[rhs.index()]));
                Ok(())
            }
            Inst::ILt { dst, lhs, rhs, .. } => {
                regs[dst.index()] = Rv::Bool(int(&regs[lhs.index()]) < int(&regs[rhs.index()]));
                Ok(())
            }
            Inst::IGe { dst, lhs, rhs, .. } => {
                regs[dst.index()] = Rv::Bool(int(&regs[lhs.index()]) >= int(&regs[rhs.index()]));
                Ok(())
            }
            Inst::IntCast { dst, src, conv } => {
                int_cast(conv, int(&regs[src.index()])).map(|v| regs[dst.index()] = Rv::Int(v))
            }
            Inst::IntToF64 { dst, src, .. } => {
                regs[dst.index()] = Rv::Float(int(&regs[src.index()]) as f64);
                Ok(())
            }
            Inst::F64ToInt { dst, src, conv } => {
                f2i(fl(&regs[src.index()]), conv).map(|v| regs[dst.index()] = Rv::Int(v))
            }
            Inst::FAdd {
                dst,
                lhs,
                rhs,
                ty: FloatTy::F64,
            } => {
                regs[dst.index()] = Rv::Float(fl(&regs[lhs.index()]) + fl(&regs[rhs.index()]));
                Ok(())
            }
            Inst::FSub {
                dst,
                lhs,
                rhs,
                ty: FloatTy::F64,
            } => {
                regs[dst.index()] = Rv::Float(fl(&regs[lhs.index()]) - fl(&regs[rhs.index()]));
                Ok(())
            }
            Inst::FMul {
                dst,
                lhs,
                rhs,
                ty: FloatTy::F64,
            } => {
                regs[dst.index()] = Rv::Float(fl(&regs[lhs.index()]) * fl(&regs[rhs.index()]));
                Ok(())
            }
            Inst::FDiv {
                dst,
                lhs,
                rhs,
                ty: FloatTy::F64,
            } => {
                regs[dst.index()] = Rv::Float(fl(&regs[lhs.index()]) / fl(&regs[rhs.index()]));
                Ok(())
            }
            Inst::FLt {
                dst,
                lhs,
                rhs,
                ty: FloatTy::F64,
            } => {
                regs[dst.index()] = Rv::Bool(fl(&regs[lhs.index()]) < fl(&regs[rhs.index()]));
                Ok(())
            }
            Inst::BNot { dst, src } => {
                regs[dst.index()] = Rv::Bool(!bo(&regs[src.index()]));
                Ok(())
            }
            Inst::BAnd { dst, lhs, rhs } => {
                regs[dst.index()] = Rv::Bool(bo(&regs[lhs.index()]) && bo(&regs[rhs.index()]));
                Ok(())
            }
            Inst::ToDyn { dst, src, from } => {
                let d = match from {
                    Prim::F64 => Dv::Float(fl(&regs[src.index()])),
                    Prim::Bool => Dv::Bool(bo(&regs[src.index()])),
                    _ => Dv::Int(int(&regs[src.index()]) as i64),
                };
                regs[dst.index()] = Rv::Dyn(d);
                Ok(())
            }
            Inst::FromDyn { dst, src, to } => match (to, dy(&regs[src.index()])) {
                (Prim::I64, Dv::Int(i)) => {
                    regs[dst.index()] = Rv::Int(i128::from(i));
                    Ok(())
                }
                (Prim::F64, Dv::Float(f)) => {
                    regs[dst.index()] = Rv::Float(f);
                    Ok(())
                }
                _ => Err((ErrorKind::TypeError, false)),
            },
            Inst::DAdd { dst, lhs, rhs, pol } => dyn_op("add", dst, lhs, rhs, pol, &mut regs),
            Inst::DSub { dst, lhs, rhs, pol } => dyn_op("sub", dst, lhs, rhs, pol, &mut regs),
            Inst::DMul { dst, lhs, rhs, pol } => dyn_op("mul", dst, lhs, rhs, pol, &mut regs),
            Inst::DDiv { dst, lhs, rhs, pol } => dyn_op("div", dst, lhs, rhs, pol, &mut regs),
            Inst::DRem { dst, lhs, rhs, pol } => dyn_op("rem", dst, lhs, rhs, pol, &mut regs),
            Inst::DFloorDiv { dst, lhs, rhs, pol } => {
                dyn_op("floor_div", dst, lhs, rhs, pol, &mut regs)
            }
            Inst::DFloorMod { dst, lhs, rhs, pol } => {
                dyn_op("floor_mod", dst, lhs, rhs, pol, &mut regs)
            }
            Inst::DShl { dst, lhs, rhs, pol } => dyn_op("shl", dst, lhs, rhs, pol, &mut regs),
            Inst::DShr { dst, lhs, rhs, pol } => dyn_op("shr", dst, lhs, rhs, pol, &mut regs),
            Inst::DPow { dst, lhs, rhs, pol } => dyn_op("pow", dst, lhs, rhs, pol, &mut regs),
            Inst::DAbs { dst, src, pol } => match dyn_abs(pol, dy(&regs[src.index()])) {
                Some(Ok(v)) => {
                    regs[dst.index()] = Rv::Dyn(v);
                    Ok(())
                }
                Some(Err(e)) => Err(e),
                None => Err((ErrorKind::TypeError, false)),
            },
            Inst::FPow {
                dst,
                lhs,
                rhs,
                ty: FloatTy::F64,
            } => {
                let r = super::lspow::ls_pow(fl(&regs[lhs.index()]), fl(&regs[rhs.index()]));
                regs[dst.index()] = Rv::Float(r);
                Ok(())
            }
            Inst::DLt { dst, lhs, rhs } => {
                match dyn_cmp(dy(&regs[lhs.index()]), dy(&regs[rhs.index()])) {
                    Some(o) => {
                        regs[dst.index()] = Rv::Bool(o == core::cmp::Ordering::Less);
                        Ok(())
                    }
                    None => match (dy(&regs[lhs.index()]), dy(&regs[rhs.index()])) {
                        (Dv::Int(_) | Dv::Float(_), Dv::Int(_) | Dv::Float(_)) => {
                            regs[dst.index()] = Rv::Bool(false); // NaN
                            Ok(())
                        }
                        _ => Err((ErrorKind::TypeError, false)),
                    },
                }
            }
            Inst::DEq { dst, lhs, rhs } => {
                let (a, b) = (dy(&regs[lhs.index()]), dy(&regs[rhs.index()]));
                let e = match dyn_cmp(a, b) {
                    Some(o) => o == core::cmp::Ordering::Equal,
                    None => match (a, b) {
                        (Dv::Nil, Dv::Nil) => true,
                        (Dv::Bool(x), Dv::Bool(y)) => x == y,
                        _ => false,
                    },
                };
                regs[dst.index()] = Rv::Bool(e);
                Ok(())
            }
            Inst::Safepoint {} => {
                if fuel == 0 {
                    fail!((ErrorKind::OutOfFuel, true));
                }
                fuel -= 1;
                Ok(())
            }
            Inst::Jmp { target } => jump!(target.0),
            Inst::JmpIf { cond, target } => {
                if bo(&regs[cond.index()]) {
                    jump!(target.0);
                }
                Ok(())
            }
            Inst::JmpIfNot { cond, target } => {
                if !bo(&regs[cond.index()]) {
                    jump!(target.0);
                }
                Ok(())
            }
            Inst::Switch { src, table, .. } => {
                let v = int(&regs[src.index()]);
                let t = &tables[table.index()];
                let target = usize::try_from(v)
                    .ok()
                    .and_then(|i| t.targets.get(i))
                    .copied()
                    .unwrap_or(t.default);
                jump!(target.0);
            }
            Inst::SetGlobal { global, src } => {
                globals[global.index()] = regs[src.index()];
                Ok(())
            }
            Inst::Ret { src } => {
                return (Outcome::Ret(regs[src.index()]), start_fuel - fuel, globals);
            }
            other => unreachable!("reference does not model {other}"),
        };
        if let Err(e) = res {
            fail!(e);
        }
        pc += 1;
    }
}

fn dyn_op(
    name: &str,
    d: bytecode_lang::Reg,
    l: bytecode_lang::Reg,
    r: bytecode_lang::Reg,
    p: Policy,
    regs: &mut [Rv],
) -> R<()> {
    let get = |x: &Rv| match x {
        Rv::Dyn(d) => *d,
        _ => Dv::Nil,
    };
    match dyn_bin(name, p, get(&regs[l.index()]), get(&regs[r.index()])) {
        Some(Ok(v)) => {
            regs[d.index()] = Rv::Dyn(v);
            Ok(())
        }
        Some(Err(e)) => Err(e),
        None => Err((ErrorKind::TypeError, false)),
    }
}

fn int_cast(conv: IntConv, v: i128) -> R<i128> {
    fit(conv.to(), conv.overflow(), v)
}

fn f2i(x: f64, op: bytecode_lang::FloatConv) -> R<i128> {
    let (lo, hi) = range(op.ty());
    let t = x.trunc();
    let ok =
        !x.is_nan() && t >= lo as f64 && t <= hi as f64 && (t as i128) >= lo && (t as i128) <= hi;
    if ok {
        Ok(t as i128)
    } else if op.float_to_int() == FloatToInt::Saturate {
        Ok(if x.is_nan() {
            0
        } else if t < lo as f64 {
            lo
        } else {
            hi
        })
    } else {
        Err((ErrorKind::InvalidConversion, false))
    }
}
