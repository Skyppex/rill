//! The prelude: built-in constants and functions.
//!
//! Built-ins use type parameters, which user code cannot declare:
//!
//! - `T`: a plain number (`sample`, `f32`, `i32` or a literal)
//! - `S`: any number, with or without a unit, or a pitch

use super::types::{DefKind, ParamSig, Signature, Size, Type};

pub const CONSTANTS: &[(&str, Type)] = &[
    ("PI", Type::F32),
    ("TAU", Type::F32),
    // The host sample rate.
    ("RATE", Type::Hz),
];

pub fn constant(name: &str) -> Option<&'static Type> {
    CONSTANTS.iter().find(|(n, _)| *n == name).map(|(_, t)| t)
}

/// Does `ty` satisfy the constraint named by the type parameter `param`?
pub fn satisfies(param: &str, ty: &Type) -> bool {
    match param {
        "T" => ty.is_plain(),
        "S" => ty.is_quantity(),
        _ => unreachable!("unknown builtin type parameter {param}"),
    }
}

/// All signatures named `name`, one per arity.
pub fn lookup(name: &str) -> Vec<Signature> {
    let t = || Type::Param("T");
    let s = || Type::Param("S");
    let frame_t = || Type::Frame(Box::new(Type::Param("T")), Size::Var("N".into()));

    let sig = |params: &[(&str, Type)], ret: Type| Signature {
        kind: DefKind::Builtin,
        name: name.to_owned(),
        generics: if params.iter().any(|(_, ty)| matches!(ty, Type::Frame(..))) {
            vec!["N".to_owned()]
        } else {
            Vec::new()
        },
        params: params
            .iter()
            .map(|(n, ty)| ParamSig {
                name: (*n).to_owned(),
                ty: ty.clone(),
                has_default: default_value(name, n).is_some(),
            })
            .collect(),
        ret,
        rate: (1, 1),
    };

    match name {
        "sin" | "cos" | "tan" | "tanh" | "exp" | "log" | "sqrt" | "abs" | "floor" | "ceil"
        | "round" | "wrap" => vec![sig(&[("x", t())], t())],
        "pow" => vec![sig(&[("x", t()), ("y", t())], t())],
        "min" | "max" => vec![
            sig(&[("x", frame_t())], t()),
            sig(&[("a", s()), ("b", s())], s()),
        ],
        "sum" => vec![sig(&[("x", frame_t())], t())],
        "clamp" => vec![sig(&[("x", s()), ("lo", s()), ("hi", s())], s())],
        // Per-tick multiplier that decays by 60 dB over `time`.
        "decay" => vec![sig(&[("time", Type::Time)], Type::Sample)],
        // Tunings: a pitch (or a chord of them) in, a frequency out.
        "equal" => vec![sig(
            &[
                ("pitch", Type::Pitch),
                ("steps", Type::I32),
                ("a4", Type::Hz),
            ],
            Type::Hz,
        )],
        "just" | "pythagorean" | "meantone" => vec![sig(
            &[
                ("pitch", Type::Pitch),
                ("root", Type::Pitch),
                ("a4", Type::Hz),
            ],
            Type::Hz,
        )],
        "f32" => vec![sig(&[("x", t())], Type::F32)],
        "i32" => vec![sig(&[("x", t())], Type::I32)],
        "sample" => vec![sig(&[("x", t())], Type::Sample)],
        _ => Vec::new(),
    }
}

/// The default of built-in parameter `param` of `name`, if it has one.
pub fn default_value(name: &str, param: &str) -> Option<f32> {
    match (name, param) {
        ("equal", "steps") => Some(12.0),
        ("equal" | "just" | "pythagorean" | "meantone", "a4") => Some(440.0),
        _ => None,
    }
}

/// Built-ins that accept a frame in their first parameter and return one
/// result per channel. Tunings do, so a chord can be tuned in one go.
pub fn takes_frames(name: &str) -> bool {
    matches!(name, "equal" | "just" | "pythagorean" | "meantone")
}

/// Every built-in function name, for suggestions.
pub const FUNCTIONS: &[&str] = &[
    "sin",
    "cos",
    "tan",
    "tanh",
    "exp",
    "log",
    "sqrt",
    "abs",
    "floor",
    "ceil",
    "round",
    "wrap",
    "pow",
    "min",
    "max",
    "sum",
    "clamp",
    "decay",
    "equal",
    "just",
    "pythagorean",
    "meantone",
    "f32",
    "i32",
    "sample",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_function_has_a_signature() {
        for name in FUNCTIONS {
            assert!(!lookup(name).is_empty(), "{name}");
        }
        assert!(lookup("nope").is_empty());
    }
}
