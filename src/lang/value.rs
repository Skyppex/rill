//! Values written outside a program, as on the command line: `-6dB`,
//! `2kHz`, `300ms`, `A4`, or a plain number. They are written the way the
//! language writes them, and checked against the type of what they are for.

use super::lexer::{Dimension, Unit};
use super::types::Type;

/// A value as written, before it is checked against a type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// A number, with its unit if it has one.
    Number { value: f64, unit: Option<Unit> },
    /// A pitch, as `A4` or `C#3`: its number in semitones (A4 is 69).
    Pitch(f32),
}

impl Value {
    /// A plain number.
    pub fn number(value: f64) -> Value {
        Value::Number { value, unit: None }
    }

    /// The value `text` writes: a number with or without a unit, or a pitch.
    /// Units are as in the language (`dB`, `Hz`, `kHz`, `bpm`, `ms`, `s`,
    /// `st`, `cents`, `oct`), though any case will do here (`-6db`).
    pub fn parse(text: &str) -> Result<Value, String> {
        let text = text.trim();
        // The number: everything up to the first letter that does not
        // continue it (`e` in `1e3` does, when a digit follows).
        let bytes = text.as_bytes();
        let mut end = 0;
        while end < bytes.len() {
            let b = bytes[end];
            let exponent = (b == b'e' || b == b'E')
                && end > 0
                && bytes
                    .get(end + 1)
                    .is_some_and(|&n| n.is_ascii_digit() || n == b'-' || n == b'+');
            if b.is_ascii_digit() || b == b'.' || ((b == b'-' || b == b'+') && end == 0) {
                end += 1;
            } else if exponent {
                end += 2;
            } else {
                break;
            }
        }
        let (number, suffix) = text.split_at(end);
        if number.is_empty() {
            return super::check::pitch_literal(text)
                .map(Value::Pitch)
                .ok_or_else(|| {
                    format!("`{text}` is not a number, a number with a unit, or a pitch")
                });
        }
        let value: f64 = number
            .parse()
            .map_err(|_| format!("`{text}` is not a number"))?;
        if suffix.is_empty() {
            return Ok(Value::number(value));
        }
        let unit = Unit::ALL
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(suffix))
            .map(|&(_, unit)| unit)
            .ok_or_else(|| {
                let names = Unit::ALL.iter().map(|(n, _)| *n);
                match super::diag::suggest(suffix, names.clone()) {
                    Some(s) => format!("unknown unit `{suffix}` in `{text}`; did you mean `{s}`?"),
                    None => format!(
                        "unknown unit `{suffix}` in `{text}` (known units: {})",
                        names.collect::<Vec<_>>().join(", ")
                    ),
                }
            })?;
        Ok(Value::Number {
            value,
            unit: Some(unit),
        })
    }

    /// The number this is for a value of type `ty`, in the base units the
    /// program runs on (amplitude for a `Gain`, Hz, seconds, semitones). A
    /// plain number is taken as it is. With `ty` unknown, any unit goes.
    pub fn resolve(&self, ty: Option<&Type>) -> Result<f32, String> {
        match *self {
            Value::Number { value, unit: None } => Ok(value as f32),
            Value::Number {
                value,
                unit: Some(unit),
            } => {
                let wants = ty.map(dimension);
                match wants {
                    None => Ok(unit.to_base(value) as f32),
                    Some(Some(d)) if d == unit.dimension() => Ok(unit.to_base(value) as f32),
                    Some(want) => Err(format!(
                        "`{value}{}` is {}, but this is a {}: {}",
                        unit.name(),
                        what(unit.dimension()),
                        ty.expect("known"),
                        match want {
                            Some(d) => format!("give it in {}, or as a plain number", units(d)),
                            None => "give it as a plain number".into(),
                        }
                    )),
                }
            }
            Value::Pitch(p) => match ty {
                None | Some(Type::Pitch) => Ok(p),
                Some(ty) => Err(format!("a pitch is not a {ty}")),
            },
        }
    }
}

impl Value {
    /// A note on what this probably meant for a value of type `ty`, when it
    /// is allowed but likely a slip: a plain negative number for a `Gain`
    /// is an amplitude (flipped upside down), not decibels.
    pub fn hint(&self, ty: Option<&Type>) -> Option<String> {
        match (*self, ty) {
            (Value::Number { value, unit: None }, Some(Type::Gain)) if value < 0.0 => Some(
                format!("`{value}` is an amplitude, upside down; for decibels, write `{value}dB`"),
            ),
            _ => None,
        }
    }
}

/// What a unit on a value of type `ty` must measure, if it may have one.
fn dimension(ty: &Type) -> Option<Dimension> {
    match ty {
        Type::Freq => Some(Dimension::Frequency),
        Type::Time => Some(Dimension::Time),
        Type::Interval => Some(Dimension::Interval),
        Type::Gain => Some(Dimension::Level),
        _ => None,
    }
}

fn what(d: Dimension) -> &'static str {
    match d {
        Dimension::Frequency => "a frequency",
        Dimension::Time => "a time",
        Dimension::Interval => "an interval",
        Dimension::Level => "a level",
    }
}

/// The units for `d`, as a list to read.
fn units(d: Dimension) -> String {
    let names: Vec<String> = Unit::ALL
        .iter()
        .filter(|(_, u)| u.dimension() == d)
        .map(|(n, _)| format!("`{n}`"))
        .collect();
    match names.as_slice() {
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
        [] => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(text: &str, ty: Type) -> Result<f32, String> {
        Value::parse(text)?.resolve(Some(&ty))
    }

    #[test]
    fn units_are_checked_against_the_type() {
        assert!((resolve("-6dB", Type::Gain).unwrap() - 0.501).abs() < 1e-3);
        assert!((resolve("-6db", Type::Gain).unwrap() - 0.501).abs() < 1e-3);
        assert_eq!(resolve("2kHz", Type::Freq).unwrap(), 2000.0);
        assert_eq!(resolve("120bpm", Type::Freq).unwrap(), 2.0);
        assert_eq!(resolve("300ms", Type::Time).unwrap(), 0.3);
        assert_eq!(resolve("1oct", Type::Interval).unwrap(), 12.0);
        assert_eq!(resolve("1.5e3Hz", Type::Freq).unwrap(), 1500.0);
        // A plain number is the base unit, whatever the type.
        assert_eq!(resolve("0.5", Type::Gain).unwrap(), 0.5);
        assert_eq!(resolve("-6", Type::Float).unwrap(), -6.0);
        let wrong = resolve("-6dB", Type::Freq).unwrap_err();
        assert!(wrong.contains("`Hz`, `kHz` or `bpm`"), "{wrong}");
        assert!(resolve("3dB", Type::Float).is_err());
    }

    #[test]
    fn pitches_and_mistakes() {
        assert_eq!(resolve("A4", Type::Pitch).unwrap(), 69.0);
        assert!(resolve("A4", Type::Freq).is_err());
        let typo = Value::parse("-6dV").unwrap_err();
        assert!(typo.contains("did you mean `dB`"), "{typo}");
        assert!(Value::parse("loud").is_err());
        // Without a type, a unit is converted as it says.
        let any = Value::parse("-6dB").unwrap().resolve(None).unwrap();
        assert!((any - 0.501).abs() < 1e-3);
        // Allowed, but probably meant in decibels.
        assert!(
            Value::parse("-6")
                .unwrap()
                .hint(Some(&Type::Gain))
                .is_some()
        );
        assert!(
            Value::parse("0.5")
                .unwrap()
                .hint(Some(&Type::Gain))
                .is_none()
        );
        assert!(
            Value::parse("-6dB")
                .unwrap()
                .hint(Some(&Type::Gain))
                .is_none()
        );
    }
}
