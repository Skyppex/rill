//! Types as the checker sees them.
//!
//! A type describes one tick's value. Whether something is a stream or a
//! constant is not part of its type: a constant is a stream that never
//! changes.

use std::fmt;

use super::lexer::Dimension;

#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    Sample,
    F32,
    I32,
    Bool,
    Hz,
    Time,
    Interval,
    /// An unsuffixed number literal not yet pinned to `sample`, `f32` or
    /// `i32`. It becomes whichever plain numeric type it meets.
    Num,
    /// `[elem; size]`. `elem` is always a scalar.
    Frame(Box<Type>, Size),
    /// No value, e.g. `out(x)` or an `if` without `else`.
    Unit,
    /// The expression always `return`s, so it never produces a value.
    Never,
    /// A type parameter of a built-in, e.g. `T` in `min(a: T, b: T) -> T`.
    Param(&'static str),
    /// Stands in after an error so one mistake is reported once.
    Error,
}

/// Channel count of a frame.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Size {
    Const(u32),
    /// A size parameter such as `N` in `mix_down<N>`.
    Var(String),
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Size::Const(n) => write!(f, "{n}"),
            Size::Var(v) => f.write_str(v),
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Sample => f.write_str("sample"),
            Type::F32 => f.write_str("f32"),
            Type::I32 => f.write_str("i32"),
            Type::Bool => f.write_str("bool"),
            Type::Hz => f.write_str("Hz"),
            Type::Time => f.write_str("Time"),
            Type::Interval => f.write_str("Interval"),
            Type::Num => f.write_str("number"),
            Type::Frame(elem, size) => write!(f, "[{elem}; {size}]"),
            Type::Unit => f.write_str("()"),
            Type::Never => f.write_str("!"),
            Type::Param(p) => f.write_str(p),
            Type::Error => f.write_str("{error}"),
        }
    }
}

impl Type {
    pub fn from_dimension(d: Dimension) -> Type {
        match d {
            Dimension::Frequency => Type::Hz,
            Dimension::Time => Type::Time,
            Dimension::Interval => Type::Interval,
        }
    }

    /// Dimensionless numbers that mix freely with each other.
    pub fn is_plain(&self) -> bool {
        matches!(self, Type::Num | Type::Sample | Type::F32 | Type::I32)
    }

    /// Numbers that carry a unit.
    pub fn is_dimensioned(&self) -> bool {
        matches!(self, Type::Hz | Type::Time | Type::Interval)
    }

    pub fn is_scalar(&self) -> bool {
        self.is_plain() || self.is_dimensioned() || *self == Type::Bool
    }

    /// Error or Never: anything goes, the problem is reported elsewhere or the
    /// value never exists.
    pub fn is_wild(&self) -> bool {
        matches!(self, Type::Error | Type::Never)
    }

    /// What an unconstrained binding settles on: literals become `sample`.
    pub fn settle(self) -> Type {
        match self {
            Type::Num => Type::Sample,
            Type::Frame(elem, n) => Type::Frame(Box::new(elem.settle()), n),
            t => t,
        }
    }
}

/// Can a value of type `from` be used where `to` is expected?
///
/// `sample` and `f32` convert both ways, and an unsuffixed literal becomes
/// any plain number. Units never appear or disappear implicitly.
pub fn coerces(from: &Type, to: &Type) -> bool {
    if from == to || from.is_wild() || to.is_wild() {
        return true;
    }
    match (from, to) {
        (Type::Num, Type::Sample | Type::F32 | Type::I32) => true,
        (Type::F32, Type::Sample) | (Type::Sample, Type::F32) => true,
        (Type::Frame(a, n), Type::Frame(b, m)) => n == m && coerces(a, b),
        _ => false,
    }
}

/// The common type of two plain numbers in arithmetic, or two branches of
/// an `if`.
pub fn join(a: &Type, b: &Type) -> Option<Type> {
    if a == b {
        return Some(a.clone());
    }
    match (a, b) {
        (Type::Error, _) | (_, Type::Error) => Some(Type::Error),
        (Type::Never, t) | (t, Type::Never) => Some(t.clone()),
        (Type::Num, t) | (t, Type::Num) if t.is_plain() => Some(t.clone()),
        (Type::Sample, Type::F32) | (Type::F32, Type::Sample) => Some(Type::Sample),
        (Type::Frame(x, n), Type::Frame(y, m)) if n == m => {
            Some(Type::Frame(Box::new(join(x, y)?), n.clone()))
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefKind {
    Fn,
    Rill,
    Builtin,
}

impl DefKind {
    pub fn word(self) -> &'static str {
        match self {
            DefKind::Fn => "fn",
            DefKind::Rill => "rill",
            DefKind::Builtin => "built-in",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParamSig {
    pub name: String,
    pub ty: Type,
    pub has_default: bool,
}

/// The callable shape of a fn, rill or built-in.
#[derive(Clone, Debug, PartialEq)]
pub struct Signature {
    pub kind: DefKind,
    pub name: String,
    pub generics: Vec<String>,
    pub params: Vec<ParamSig>,
    pub ret: Type,
    /// Output rate as `num / den` of the input rate; `(1, 1)` for 1:1.
    pub rate: (u32, u32),
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind.word(), self.name)?;
        if !self.generics.is_empty() {
            write!(f, "<{}>", self.generics.join(", "))?;
        }
        f.write_str("(")?;
        for (i, p) in self.params.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}: {}", p.name, p.ty)?;
            if p.has_default {
                f.write_str(" = ..")?;
            }
        }
        write!(f, ") -> {}", self.ret)?;
        match self.rate {
            (1, 1) => Ok(()),
            (1, d) => write!(f, " @ rate / {d}"),
            (n, _) => write!(f, " @ rate * {n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: Type, n: u32) -> Type {
        Type::Frame(Box::new(t), Size::Const(n))
    }

    #[test]
    fn coercion() {
        assert!(coerces(&Type::Num, &Type::Sample));
        assert!(coerces(&Type::F32, &Type::Sample));
        assert!(coerces(&Type::Sample, &Type::F32));
        assert!(!coerces(&Type::Num, &Type::Hz));
        assert!(!coerces(&Type::Sample, &Type::Hz));
        assert!(!coerces(&Type::Hz, &Type::Time));
        assert!(!coerces(&Type::Sample, &Type::I32));
        assert!(coerces(&frame(Type::Num, 2), &frame(Type::Sample, 2)));
        assert!(!coerces(&frame(Type::Sample, 2), &frame(Type::Sample, 3)));
        assert!(!coerces(&Type::Sample, &frame(Type::Sample, 1)));
    }

    #[test]
    fn joining() {
        assert_eq!(join(&Type::Num, &Type::F32), Some(Type::F32));
        assert_eq!(join(&Type::F32, &Type::Sample), Some(Type::Sample));
        assert_eq!(join(&Type::I32, &Type::Sample), None);
        assert_eq!(join(&Type::Never, &Type::Hz), Some(Type::Hz));
        assert_eq!(join(&Type::Hz, &Type::Sample), None);
    }

    #[test]
    fn display() {
        assert_eq!(frame(Type::Sample, 8).to_string(), "[sample; 8]");
        assert_eq!(
            Type::Frame(Box::new(Type::Sample), Size::Var("N".into())).to_string(),
            "[sample; N]"
        );
    }
}
