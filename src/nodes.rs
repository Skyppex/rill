//! Built-in nodes.

use crate::node::{Context, Inputs, Node, Outputs, Signal};

/// Live entry-rill parameter with short linear smoothing.
#[derive(Clone, Copy, Debug)]
pub struct Param {
    initial: f32,
    current: f32,
    target: f32,
    step: f32,
    remaining: usize,
    smoothing_ms: f32,
}

impl Param {
    pub fn new(initial: f32) -> Self {
        Param {
            initial,
            current: initial,
            target: initial,
            step: 0.0,
            remaining: 0,
            smoothing_ms: 5.0,
        }
    }

    pub fn with_smoothing_ms(initial: f32, smoothing_ms: f32) -> Self {
        Param {
            smoothing_ms,
            ..Param::new(initial)
        }
    }
}

impl Node for Param {
    fn name(&self) -> &'static str {
        "param"
    }

    fn inputs(&self) -> usize {
        0
    }

    fn process(&mut self, _ctx: &Context, _inputs: &Inputs, out: &mut Outputs) {
        let out = out.mono();
        for y in out {
            if self.remaining > 0 {
                self.current += self.step;
                self.remaining -= 1;
                if self.remaining == 0 {
                    self.current = self.target;
                    self.step = 0.0;
                }
            }
            *y = self.current;
        }
    }

    fn reset(&mut self) {
        self.current = self.initial;
        self.target = self.initial;
        self.step = 0.0;
        self.remaining = 0;
    }

    fn set_control_value(&mut self, value: f32, sample_rate: f32) -> bool {
        self.target = value;
        let frames = (sample_rate * self.smoothing_ms / 1000.0).round() as usize;
        if frames == 0 {
            self.current = value;
            self.step = 0.0;
            self.remaining = 0;
        } else {
            self.remaining = frames;
            self.step = (self.target - self.current) / frames as f32;
        }
        true
    }
}

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

    fn process(&mut self, ctx: &Context, inputs: &Inputs, out: &mut Outputs) {
        let out = out.mono();
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

    fn process(&mut self, _ctx: &Context, inputs: &Inputs, out: &mut Outputs) {
        binary(inputs.get(0), inputs.get(1), out.mono(), |a, b| a * b);
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

    fn process(&mut self, _ctx: &Context, inputs: &Inputs, out: &mut Outputs) {
        binary(inputs.get(0), inputs.get(1), out.mono(), |a, b| a + b);
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

/// Any two-operand [`Op2`](crate::ops::Op2), per frame.
#[derive(Clone, Copy, Debug)]
pub struct Binary(pub crate::ops::Op2);

impl Node for Binary {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn inputs(&self) -> usize {
        2
    }

    fn process(&mut self, _ctx: &Context, inputs: &Inputs, out: &mut Outputs) {
        let op = self.0;
        binary(inputs.get(0), inputs.get(1), out.mono(), |a, b| {
            op.apply(a, b)
        });
    }
}

/// Any one-operand [`Op1`](crate::ops::Op1), per frame.
#[derive(Clone, Copy, Debug)]
pub struct Unary(pub crate::ops::Op1);

impl Node for Unary {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn inputs(&self) -> usize {
        1
    }

    fn process(&mut self, ctx: &Context, inputs: &Inputs, out: &mut Outputs) {
        let (op, rate) = (self.0, ctx.sample_rate);
        let out = out.mono();
        match inputs.get(0) {
            Signal::Const(x) => out.fill(op.apply(x, rate)),
            Signal::Buffer(x) => {
                for (y, &x) in out.iter_mut().zip(x) {
                    *y = op.apply(x, rate);
                }
            }
        }
    }
}

/// `if cond { a } else { b }` per frame. Inputs: cond, a, b.
#[derive(Clone, Copy, Debug, Default)]
pub struct Select;

impl Node for Select {
    fn name(&self) -> &'static str {
        "select"
    }

    fn inputs(&self) -> usize {
        3
    }

    fn process(&mut self, _ctx: &Context, inputs: &Inputs, out: &mut Outputs) {
        let (cond, a, b) = (inputs.get(0), inputs.get(1), inputs.get(2));
        for (i, y) in out.mono().iter_mut().enumerate() {
            *y = if cond.at(i) != 0.0 { a.at(i) } else { b.at(i) };
        }
    }
}
