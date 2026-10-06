//! Hard-coded graphs standing in for Rill source until the parser exists.

use crate::graph::Graph;

/// `out(sine(freq) * gain)`
pub fn sine(freq: f32, gain: f32) -> Graph {
    let mut g = Graph::new();
    let osc = g.sine(freq);
    let voice = g.gain(osc, gain);
    g.out(voice);
    g
}

/// The example from the design doc:
///
/// ```text
/// let lfo   = sine(0.5Hz) * 20Hz + 440Hz
/// let voice = sine(lfo) * 0.3
/// out(voice)
/// ```
pub fn vibrato(freq: f32, gain: f32) -> Graph {
    let mut g = Graph::new();
    let lfo = g.sine(0.5);
    let depth = g.mul(lfo, 20.0);
    let pitch = g.add(depth, freq);
    let osc = g.sine(pitch);
    let voice = g.gain(osc, gain);
    g.out(voice);
    g
}

/// Names accepted by [`by_name`].
pub const NAMES: &[&str] = &["sine", "vibrato"];

pub fn by_name(name: &str, freq: f32, gain: f32) -> Option<Graph> {
    match name {
        "sine" => Some(sine(freq, gain)),
        "vibrato" => Some(vibrato(freq, gain)),
        _ => None,
    }
}
