//! Arithmetic shared by constant folding, the bytecode VM and graph nodes,
//! so a value computes the same whether it is folded at build time or run
//! per tick.
//!
//! Every value is an `f32`. Booleans are `0.0` (false) or `1.0` (true); any
//! non-zero value counts as true.

/// Two-operand operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op2 {
    Add,
    Sub,
    Mul,
    Div,
    /// Truncating remainder, like Rust's `%`.
    Rem,
    /// Division rounded toward zero, for `Int / Int`.
    IDiv,
    Pow,
    Min,
    Max,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

/// One-operand operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op1 {
    Neg,
    Not,
    Abs,
    Sin,
    Cos,
    Tan,
    Tanh,
    Exp,
    Log,
    Sqrt,
    Floor,
    Ceil,
    Round,
    /// Fractional part, in [0, 1).
    Wrap,
    /// Toward zero, for `x as Int`.
    Trunc,
}

/// `x.floor()`, written so it vectorizes on plain x86-64 (which has no SIMD
/// floor instruction). It gives exactly the same result for every input,
/// including `-0.0`, infinities and NaN.
#[inline(always)]
pub fn floor(x: f32) -> f32 {
    // Beyond 2^23 every float is a whole number; this also passes on
    // infinities and NaN.
    if x.abs().partial_cmp(&8_388_608.0) != Some(std::cmp::Ordering::Less) {
        return x;
    }
    // SAFETY: |x| < 2^23 here, so it fits an `i32`. An unchecked
    // conversion, unlike `as`, needs no saturation and vectorizes.
    let t = unsafe { x.to_int_unchecked::<i32>() } as f32;
    let f = if t > x { t - 1.0 } else { t };
    f.copysign(x)
}

fn truth(x: bool) -> f32 {
    if x { 1.0 } else { 0.0 }
}

impl Op2 {
    #[inline(always)]
    pub fn apply(self, a: f32, b: f32) -> f32 {
        match self {
            Op2::Add => a + b,
            Op2::Sub => a - b,
            Op2::Mul => a * b,
            Op2::Div => a / b,
            Op2::Rem => a % b,
            Op2::IDiv => (a / b).trunc(),
            Op2::Pow => a.powf(b),
            Op2::Min => a.min(b),
            Op2::Max => a.max(b),
            Op2::Lt => truth(a < b),
            Op2::Le => truth(a <= b),
            Op2::Gt => truth(a > b),
            Op2::Ge => truth(a >= b),
            Op2::Eq => truth(a == b),
            Op2::Ne => truth(a != b),
            Op2::And => truth(a != 0.0 && b != 0.0),
            Op2::Or => truth(a != 0.0 || b != 0.0),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Op2::Add => "add",
            Op2::Sub => "sub",
            Op2::Mul => "mul",
            Op2::Div => "div",
            Op2::Rem => "rem",
            Op2::IDiv => "idiv",
            Op2::Pow => "pow",
            Op2::Min => "min",
            Op2::Max => "max",
            Op2::Lt => "lt",
            Op2::Le => "le",
            Op2::Gt => "gt",
            Op2::Ge => "ge",
            Op2::Eq => "eq",
            Op2::Ne => "ne",
            Op2::And => "and",
            Op2::Or => "or",
        }
    }
}

impl Op1 {
    #[inline(always)]
    pub fn apply(self, x: f32) -> f32 {
        match self {
            Op1::Neg => -x,
            Op1::Not => truth(x == 0.0),
            Op1::Abs => x.abs(),
            Op1::Sin => x.sin(),
            Op1::Cos => x.cos(),
            Op1::Tan => x.tan(),
            Op1::Tanh => x.tanh(),
            Op1::Exp => x.exp(),
            Op1::Log => x.ln(),
            Op1::Sqrt => x.sqrt(),
            Op1::Floor => x.floor(),
            Op1::Ceil => x.ceil(),
            Op1::Round => x.round(),
            Op1::Wrap => x - x.floor(),
            Op1::Trunc => x.trunc(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Op1::Neg => "neg",
            Op1::Not => "not",
            Op1::Abs => "abs",
            Op1::Sin => "sin",
            Op1::Cos => "cos",
            Op1::Tan => "tan",
            Op1::Tanh => "tanh",
            Op1::Exp => "exp",
            Op1::Log => "log",
            Op1::Sqrt => "sqrt",
            Op1::Floor => "floor",
            Op1::Ceil => "ceil",
            Op1::Round => "round",
            Op1::Wrap => "wrap",
            Op1::Trunc => "trunc",
        }
    }

    /// The op behind a one-argument prelude function, if it is one.
    pub fn builtin(name: &str) -> Option<Op1> {
        Some(match name {
            "abs" => Op1::Abs,
            "sin" => Op1::Sin,
            "cos" => Op1::Cos,
            "tan" => Op1::Tan,
            "tanh" => Op1::Tanh,
            "exp" => Op1::Exp,
            "log" => Op1::Log,
            "sqrt" => Op1::Sqrt,
            "floor" => Op1::Floor,
            "ceil" => Op1::Ceil,
            "round" => Op1::Round,
            "wrap" => Op1::Wrap,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logic_and_integers() {
        assert_eq!(Op2::Lt.apply(1.0, 2.0), 1.0);
        assert_eq!(Op2::And.apply(1.0, 0.0), 0.0);
        assert_eq!(Op1::Not.apply(0.0), 1.0);
        assert_eq!(Op2::IDiv.apply(-7.0, 2.0), -3.0);
        assert_eq!(Op2::Rem.apply(-7.0, 2.0), -1.0);
        assert_eq!(Op1::Wrap.apply(-0.25), 0.75);
    }
}

#[cfg(test)]
mod floor_tests {
    #[test]
    fn floor_matches_the_standard_one_bit_for_bit() {
        let mut xs = vec![
            0.0f32,
            -0.0,
            0.5,
            -0.5,
            1.0,
            -1.0,
            2.5,
            -2.5,
            0.999_999_9,
            -0.999_999_9,
            8_388_607.5,
            -8_388_607.5,
            8_388_608.0,
            1e20,
            -1e20,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            1e-45,
            -1e-45,
        ];
        // And a sweep across many magnitudes.
        let mut v = 1e-30f32;
        while v < 1e30 {
            xs.extend([v, -v, v * 1.37, -v * 1.37]);
            v *= 1.7;
        }
        for x in xs {
            assert_eq!(super::floor(x).to_bits(), x.floor().to_bits(), "{x}");
        }
        assert!(super::floor(f32::NAN).is_nan());
    }
}
