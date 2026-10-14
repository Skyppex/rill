//! The prelude: the constants and functions every program has without
//! naming them. Only what cannot be written in Rill (or would be much
//! slower in it) belongs here; everything else goes in the core library.
//!
//! Prelude functions use type parameters, which user code cannot declare:
//!
//! - `T`: a plain number (`Sample`, `Float`, `Int` or a literal)
//! - `S`: any number, with or without a unit, or a pitch
//! - `F`: a plain number, or a frame of them (reductions take the outer
//!   layer off a nested frame)

use super::types::{DefKind, ParamSig, Signature, Size, Type};

pub const CONSTANTS: &[(&str, Type)] = &[
    ("PI", Type::Float),
    ("TAU", Type::Float),
    // The host sample rate.
    ("RATE", Type::Freq),
];

pub fn constant(name: &str) -> Option<&'static Type> {
    CONSTANTS.iter().find(|(n, _)| *n == name).map(|(_, t)| t)
}

/// Does `ty` satisfy the constraint named by the type parameter `param`?
pub fn satisfies(param: &str, ty: &Type) -> bool {
    match param {
        "T" => ty.is_plain(),
        "S" => ty.is_quantity(),
        "F" => ty.leaf().is_plain(),
        _ => unreachable!("unknown prelude type parameter {param}"),
    }
}

/// All signatures named `name`, one per arity.
pub fn lookup(name: &str) -> Vec<Signature> {
    let t = || Type::Param("T");
    let s = || Type::Param("S");
    let f = || Type::Param("F");
    let frame_f = || Type::Frame(Box::new(Type::Param("F")), Size::Var("N".into()));

    let sig = |params: &[(&str, Type)], ret: Type| Signature {
        kind: DefKind::Prelude,
        name: name.to_owned(),
        generics: if params
            .iter()
            .any(|(_, ty)| matches!(ty, Type::Frame(..) | Type::Audio(Some(_))))
        {
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
            sig(&[("x", frame_f())], f()),
            sig(&[("a", s()), ("b", s())], s()),
        ],
        "sum" => vec![sig(&[("x", frame_f())], f())],
        "clamp" => vec![sig(&[("x", s()), ("lo", s()), ("hi", s())], s())],
        // Tunings: a pitch (or a chord of them) in, a frequency out.
        "equal" => vec![sig(
            &[
                ("pitch", Type::Pitch),
                ("steps", Type::Int),
                ("a4", Type::Freq),
            ],
            Type::Freq,
        )],
        "just" | "pythagorean" | "meantone" => vec![sig(
            &[
                ("pitch", Type::Pitch),
                ("root", Type::Pitch),
                ("a4", Type::Freq),
            ],
            Type::Freq,
        )],
        // The level of an amplitude, and the amplitude factor of a level.
        "level" => vec![sig(&[("x", t())], Type::Gain)],
        "amp" => vec![sig(&[("gain", Type::Gain)], Type::Float)],
        // A number picked when the program is built.
        "random" => vec![sig(&[], Type::Float), sig(&[("lo", s()), ("hi", s())], s())],
        // A new number every sample.
        "noise" => vec![sig(&[], Type::Sample)],
        "ceiling" => vec![sig(&[("x", s()), ("start", s())], s())],
        // As written for documentation; the checker gives `delay` its own
        // rule, as `time` can be a frame of taps and `x` a frame of
        // channels.
        "delay" => vec![sig(
            &[
                ("x", Type::Sample),
                ("time", Type::Time),
                ("max", Type::Time),
            ],
            Type::Sample,
        )],
        // What an audio file holds at a time, one value per channel; or at
        // a frame (at the host rate) and a part of one, exact far into a
        // long file.
        "read" => {
            let audio = || ("audio", Type::Audio(Some(Size::Var("N".into()))));
            let ret = || Type::Frame(Box::new(Type::Sample), Size::Var("N".into()));
            vec![
                sig(&[audio(), ("at", Type::Time)], ret()),
                sig(
                    &[audio(), ("frame", Type::Float), ("part", Type::Float)],
                    ret(),
                ),
            ]
        }
        // Red, green, blue and alpha of an image at a place, 0 to 1.
        "pixel" => vec![sig(
            &[
                ("image", Type::Image(None)),
                ("x", Type::Float),
                ("y", Type::Float),
            ],
            Type::Frame(Box::new(Type::Float), Size::Const(4)),
        )],
        _ => Vec::new(),
    }
}

/// The default of prelude parameter `param` of `name`, if it has one.
pub fn default_value(name: &str, param: &str) -> Option<f32> {
    match (name, param) {
        ("equal", "steps") => Some(12.0),
        ("equal" | "just" | "pythagorean" | "meantone", "a4") => Some(440.0),
        // Worked out from the times when they are constants.
        ("delay", "max") => Some(0.0),
        _ => None,
    }
}

/// Prelude functions that keep something between samples, as a rill does:
/// only rills can call them, and they are not values or constants.
pub fn keeps_state(name: &str) -> bool {
    matches!(name, "noise" | "delay")
}

/// Prelude functions that accept a frame in their first parameter and return one
/// result per channel. Tunings do, so a chord can be tuned in one go.
pub fn takes_frames(name: &str) -> bool {
    matches!(name, "equal" | "just" | "pythagorean" | "meantone")
}

/// What a prelude function or constant does, for tools such as editors.
pub fn doc(name: &str) -> Option<&'static str> {
    Some(match name {
        "PI" => "Half a turn, π ≈ 3.14159.",
        "TAU" => {
            "A full turn, 2π ≈ 6.28319. `sin(phase * TAU)` makes one cycle as `phase` goes from 0 to 1."
        }
        "RATE" => "The host sample rate. One tick lasts `1 / RATE`.",
        "sin" => "Sine of `x`, in radians.",
        "cos" => "Cosine of `x`, in radians.",
        "tan" => "Tangent of `x`, in radians.",
        "tanh" => "Hyperbolic tangent: a smooth soft clipper that maps any number into (-1, 1).",
        "exp" => "e raised to the power `x`.",
        "log" => "Natural logarithm of `x`.",
        "sqrt" => "Square root of `x`.",
        "abs" => "`x` without its sign.",
        "floor" => "The largest whole number not above `x`.",
        "ceil" => "The smallest whole number not below `x`.",
        "round" => "`x` rounded to the nearest whole number.",
        "wrap" => {
            "The fractional part of `x`, in [0, 1). Keeps a phase from growing forever: `phase = wrap(phase + freq / RATE)`."
        }
        "pow" => "`x` raised to the power `y`.",
        "min" => {
            "The smaller of `a` and `b`, or the smallest element of a frame. For a frame of frames, \
             the smallest channel by channel."
        }
        "max" => {
            "The larger of `a` and `b`, or the largest element of a frame. For a frame of frames, \
             the largest channel by channel."
        }
        "sum" => {
            "The elements of a frame added together; mixes a frame down to one value. For a frame \
             of frames, adds them channel by channel, so `sum` of stereo voices is a stereo mix."
        }
        "clamp" => "`x` limited to the range [`lo`, `hi`].",
        "equal" => {
            "Equal temperament: places `pitch` on the nearest of `steps` equal divisions of the octave, with A4 at `a4`. Accepts a chord and tunes each pitch."
        }
        "just" => {
            "Just intonation built from whole-number ratios above `root`, with A4 at `a4`. Accepts a chord and tunes each pitch."
        }
        "pythagorean" => {
            "Pythagorean tuning built from pure fifths above `root`, with A4 at `a4`. Accepts a chord and tunes each pitch."
        }
        "meantone" => {
            "Quarter-comma meantone built on `root`, with A4 at `a4`. Accepts a chord and tunes each pitch."
        }
        "level" => {
            "The level of an amplitude, as a `Gain`: `level(1)` is 0dB. Silence is held at -120dB instead of -inf."
        }
        "amp" => "The amplitude factor of a level: `amp(-6dB)` is about 0.5.",
        "ceiling" => {
            "How big to make what has to exist before audio starts, such as a filter's stages, for a value `x` that may move: `x` itself if it is a constant when the program is built, otherwise `start` (which must be a constant). While playing, `x` going past it is noticed, and a live program is rebuilt with room for 1.5 times as much."
        }
        "noise" => {
            "A new random number in [-1, 1) every sample: white noise. Every call has its own sequence, so two voices never share one; the same `--seed` plays the same noise."
        }
        "pixel" => {
            "Red, green, blue and alpha of `image` at `x`, `y` in pixels, each 0 to 1: `[r, g, b, a]`. `y` 0 is the top; a pixel's colour is at its centre (`x + 0.5`), and between centres it is blended from the four nearest. Outside the image everything is 0. `@gen/spectral` plays an image as sound with it."
        }
        "read" => {
            "What `audio` holds at `at`: one value per channel, read between samples, and silence before the start and after the end. `read(KICK, 0s)` is its first frame. The file is resampled to the host rate when the program is built, so a second is a second. `read(audio, frame, part)` reads at whole frame `frame` (at the host rate) plus `part` of one instead: a time far into a long file is not exact as one number, while a whole frame and a part are. Playing a file is moving along it: `@gen/sampler` has `samples` and `play`."
        }
        "delay" => {
            "What `x` was `time` ago. With a frame of times, several taps from one buffer, each its own stream: `delay(x, [10ms + lfo * 2ms, 25ms])`. Reads between samples are interpolated, so a moving time does not crackle. `max` is the longest any time can be, which sets the buffer's size when the program is built; it can be left out when the times are constants. A frame of channels gets a buffer each. Feed back through `state` for echoes and combs."
        }
        "random" => {
            "A random number in [0, 1), or in [`lo`, `hi`) of any number type (levels are picked \
             evenly in dB). Each `random()` in the program picks its own number once, when the \
             program is built: it stays the same while playing, not a new one every sample. Use \
             `each random()` for a different number in every copy of a rill. Runs differ unless \
             a seed is given with `--seed`."
        }
        _ => return None,
    })
}

/// Every prelude function name, for suggestions.
pub const FUNCTIONS: &[&str] = &[
    "level",
    "amp",
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
    "equal",
    "just",
    "pythagorean",
    "meantone",
    "random",
    "ceiling",
    "noise",
    "delay",
    "read",
    "pixel",
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

    #[test]
    fn everything_built_in_is_documented() {
        let constants = CONSTANTS.iter().map(|(n, _)| n);
        for name in FUNCTIONS.iter().chain(constants) {
            assert!(doc(name).is_some(), "{name}");
        }
    }
}
