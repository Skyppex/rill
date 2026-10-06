//! Built-in nodes.

use crate::node::{Context, Inputs, Node, Signal};

/// Sine oscillator. Input 0 is the frequency in Hz.
#[derive(Clone, Debug, Default)]
pub struct Sine {
    initial_phase: f64,
    /// Position in the cycle, in [0, 1). Kept in f64 so long renders do not
    /// drift audibly; embedded targets will pick their own storage.
    phase: f64,
}

impl Sine {
    pub fn new() -> Self {
        Self::with_phase(0.0)
    }

    /// Start at `phase` cycles (0.25 gives a cosine).
    pub fn with_phase(phase: f64) -> Self {
        let phase = phase - phase.floor();
        Sine {
            initial_phase: phase,
            phase,
        }
    }

    #[inline(always)]
    fn tick(&mut self, increment: f64) -> f32 {
        let y = (self.phase * std::f64::consts::TAU).sin() as f32;
        self.phase += increment;
        self.phase -= self.phase.floor();
        y
    }
}

impl Node for Sine {
    fn name(&self) -> &'static str {
        "sine"
    }

    fn inputs(&self) -> usize {
        1
    }

    fn process(&mut self, ctx: &Context, inputs: &Inputs, out: &mut [f32]) {
        let rate = f64::from(ctx.sample_rate);
        match inputs.get(0) {
            Signal::Const(freq) => {
                let increment = f64::from(freq) / rate;
                for y in out.iter_mut() {
                    *y = self.tick(increment);
                }
            }
            Signal::Buffer(freq) => {
                for (y, &f) in out.iter_mut().zip(freq) {
                    *y = self.tick(f64::from(f) / rate);
                }
            }
        }
    }

    fn reset(&mut self) {
        self.phase = self.initial_phase;
    }
}

/// `x * amount`. Input 0 is the signal, input 1 the amount.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gain;

impl Node for Gain {
    fn name(&self) -> &'static str {
        "gain"
    }

    fn inputs(&self) -> usize {
        2
    }

    fn process(&mut self, _ctx: &Context, inputs: &Inputs, out: &mut [f32]) {
        binary(inputs.get(0), inputs.get(1), out, |a, b| a * b);
    }
}

/// `a + b`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Add;

impl Node for Add {
    fn name(&self) -> &'static str {
        "add"
    }

    fn inputs(&self) -> usize {
        2
    }

    fn process(&mut self, _ctx: &Context, inputs: &Inputs, out: &mut [f32]) {
        binary(inputs.get(0), inputs.get(1), out, |a, b| a + b);
    }
}

/// Element-wise `op`, with the constant cases split out so the loops stay
/// branch-free and vectorize.
#[inline(always)]
fn binary(a: Signal, b: Signal, out: &mut [f32], op: impl Fn(f32, f32) -> f32) {
    match (a, b) {
        (Signal::Buffer(a), Signal::Buffer(b)) => {
            for ((y, &a), &b) in out.iter_mut().zip(a).zip(b) {
                *y = op(a, b);
            }
        }
        (Signal::Buffer(a), Signal::Const(b)) => {
            for (y, &a) in out.iter_mut().zip(a) {
                *y = op(a, b);
            }
        }
        (Signal::Const(a), Signal::Buffer(b)) => {
            for (y, &b) in out.iter_mut().zip(b) {
                *y = op(a, b);
            }
        }
        (Signal::Const(a), Signal::Const(b)) => out.fill(op(a, b)),
    }
}
