//! Bytecode for compiled rill bodies, and the node that runs it.
//!
//! A compiled instance is a flat list of register instructions run once per
//! tick. `state` lives in registers that keep their value between ticks.
//! Jumps only go forward, so every tick finishes in at most `instrs.len()`
//! steps: there are no loops at run time.

use crate::node::{Context, Event, Inputs, Node, Outputs};
use crate::ops::{Op1, Op2};

/// An instruction operand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Operand {
    Reg(u16),
    Const(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Instr {
    Op2 {
        op: Op2,
        dst: u16,
        a: Operand,
        b: Operand,
    },
    Op1 {
        op: Op1,
        dst: u16,
        x: Operand,
    },
    Copy {
        dst: u16,
        src: Operand,
    },
    /// `dst = tunings[tuning](pitch)`, indexing [`Code::tunings`].
    Tune {
        tuning: u16,
        dst: u16,
        pitch: Operand,
    },
    /// `dst = if cond { a } else { b }`
    Select {
        dst: u16,
        cond: Operand,
        a: Operand,
        b: Operand,
    },
    /// Skip ahead to `target` unless `cond` is true.
    JumpUnless {
        cond: Operand,
        target: u32,
    },
    Jump {
        target: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TuningSpec {
    Equal { steps: f32, a4: f32 },
    Just { root: f32, a4: f32 },
    Pythagorean { root: f32, a4: f32 },
    Meantone { root: f32, a4: f32 },
}

impl TuningSpec {
    pub fn frequency(&self, pitch: f32) -> f32 {
        match *self {
            TuningSpec::Equal { steps, a4 } => {
                // Note names land on the nearest of `steps` equal divisions;
                // anything between notes (cents, bends) stays continuous.
                let note = pitch.floor();
                let step = ((note - 69.0) * steps / 12.0).round();
                a4 * 2.0f32.powf(step / steps + (pitch - note) / 12.0)
            }
            TuningSpec::Just { root, a4 } => ratio_tuning(pitch, root, a4, JUST_RATIOS),
            TuningSpec::Pythagorean { root, a4 } => {
                ratio_tuning(pitch, root, a4, PYTHAGOREAN_RATIOS)
            }
            TuningSpec::Meantone { root, a4 } => ratio_tuning(pitch, root, a4, MEANTONE_RATIOS),
        }
    }
}

const JUST_RATIOS: [f32; 12] = [
    1.0,
    16.0 / 15.0,
    9.0 / 8.0,
    6.0 / 5.0,
    5.0 / 4.0,
    4.0 / 3.0,
    45.0 / 32.0,
    3.0 / 2.0,
    8.0 / 5.0,
    5.0 / 3.0,
    9.0 / 5.0,
    15.0 / 8.0,
];

const PYTHAGOREAN_RATIOS: [f32; 12] = [
    1.0,
    256.0 / 243.0,
    9.0 / 8.0,
    32.0 / 27.0,
    81.0 / 64.0,
    4.0 / 3.0,
    729.0 / 512.0,
    3.0 / 2.0,
    128.0 / 81.0,
    27.0 / 16.0,
    16.0 / 9.0,
    243.0 / 128.0,
];

const MEANTONE_RATIOS: [f32; 12] = [
    1.0, 1.069984, 1.118034, 1.196279, 1.25, 1.33748, 1.397542, 1.495349, 1.6, 1.67185, 1.788854,
    1.869186,
];

/// A twelve-note scale built from `ratios` above `root`, scaled so that A4
/// sounds at exactly `a4`. Fractions of a semitone are added on top, so
/// bends and cents stay continuous.
fn ratio_tuning(pitch: f32, root: f32, a4: f32, ratios: [f32; 12]) -> f32 {
    // Frequency relative to the root, which sits at 1.0.
    let relative = |pitch: f32| {
        let degree = (pitch - root).floor();
        let octave = (degree / 12.0).floor();
        let index = degree.rem_euclid(12.0) as usize;
        let frac = pitch - root - degree;
        2.0f32.powf(octave) * ratios[index] * 2.0f32.powf(frac / 12.0)
    };
    a4 * relative(pitch) / relative(69.0)
}

/// One compiled instance, ready to be turned into a [`Program`] node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Code {
    pub instrs: Vec<Instr>,
    pub regs: usize,
    /// Register loaded from each node input at the start of every tick.
    pub input_regs: Vec<u16>,
    /// What each output channel holds at the end of a tick.
    pub output: Vec<Operand>,
    /// Registers that are `state`, with their initial values.
    pub state_init: Vec<(u16, f32)>,
    pub events: Vec<EventCode>,
    /// Tunings used by `Tune` instructions, here and in event handlers. Kept
    /// out of the instructions so those stay small.
    pub tunings: Vec<TuningSpec>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventCode {
    pub name: String,
    pub bindings: Vec<EventBinding>,
    pub instrs: Vec<Instr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EventBinding {
    Scalar {
        name: String,
        reg: u16,
    },
    Field {
        path: String,
        fallback: String,
        reg: u16,
    },
}

impl Code {
    /// Human-readable listing, for debugging.
    pub fn listing(&self) -> String {
        use std::fmt::Write as _;
        let op = |o: &Operand| match o {
            Operand::Reg(r) => format!("r{r}"),
            Operand::Const(c) => format!("{c}"),
        };
        let mut s = String::new();
        let _ = writeln!(
            s,
            "; {} regs, inputs {:?}, state {:?}, events {}",
            self.regs,
            self.input_regs,
            self.state_init,
            self.events.len()
        );
        for (pc, i) in self.instrs.iter().enumerate() {
            let line = match i {
                Instr::Op2 { op: o, dst, a, b } => {
                    format!("r{dst} = {} {} {}", o.name(), op(a), op(b))
                }
                Instr::Op1 { op: o, dst, x } => format!("r{dst} = {} {}", o.name(), op(x)),
                Instr::Copy { dst, src } => format!("r{dst} = {}", op(src)),
                Instr::Tune { tuning, dst, pitch } => {
                    format!("r{dst} = tune#{tuning} {}", op(pitch))
                }
                Instr::Select { dst, cond, a, b } => {
                    format!("r{dst} = select {} {} {}", op(cond), op(a), op(b))
                }
                Instr::JumpUnless { cond, target } => format!("unless {} goto {target}", op(cond)),
                Instr::Jump { target } => format!("goto {target}"),
            };
            let _ = writeln!(s, "{pc:4}: {line}");
        }
        let outs: Vec<String> = self.output.iter().map(op).collect();
        let _ = writeln!(s, "out {}", outs.join(", "));
        s
    }
}

/// Runs a compiled rill instance as an engine node.
pub struct Program {
    code: Code,
    regs: Box<[f32]>,
}

impl Program {
    pub fn new(code: Code) -> Program {
        for (pc, instr) in code.instrs.iter().enumerate() {
            if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = instr {
                assert!(
                    *target as usize > pc && *target as usize <= code.instrs.len(),
                    "jump at {pc} to {target} is not forward"
                );
            }
        }
        let mut p = Program {
            regs: vec![0.0; code.regs].into_boxed_slice(),
            code,
        };
        p.reset();
        p
    }
}

impl Node for Program {
    fn name(&self) -> &'static str {
        "rill"
    }

    fn inputs(&self) -> usize {
        self.code.input_regs.len()
    }

    fn outputs(&self) -> usize {
        self.code.output.len()
    }

    fn process(&mut self, ctx: &Context, inputs: &Inputs, out: &mut Outputs) {
        let regs = &mut self.regs[..];
        let code = &self.code;
        let rate = ctx.sample_rate;
        let val = |regs: &[f32], o: Operand| match o {
            Operand::Reg(r) => regs[r as usize],
            Operand::Const(c) => c,
        };
        for i in 0..ctx.frames {
            for (k, &r) in code.input_regs.iter().enumerate() {
                regs[r as usize] = inputs.get(k).at(i);
            }
            let mut pc = 0;
            while let Some(instr) = code.instrs.get(pc) {
                pc += 1;
                match *instr {
                    Instr::Op2 { op, dst, a, b } => {
                        regs[dst as usize] = op.apply(val(regs, a), val(regs, b));
                    }
                    Instr::Op1 { op, dst, x } => {
                        regs[dst as usize] = op.apply(val(regs, x), rate);
                    }
                    Instr::Copy { dst, src } => regs[dst as usize] = val(regs, src),
                    Instr::Tune { tuning, dst, pitch } => {
                        regs[dst as usize] =
                            code.tunings[tuning as usize].frequency(val(regs, pitch));
                    }
                    Instr::Select { dst, cond, a, b } => {
                        regs[dst as usize] = if val(regs, cond) != 0.0 {
                            val(regs, a)
                        } else {
                            val(regs, b)
                        };
                    }
                    Instr::JumpUnless { cond, target } => {
                        if val(regs, cond) == 0.0 {
                            pc = target as usize;
                        }
                    }
                    Instr::Jump { target } => pc = target as usize,
                }
            }
            for (c, &o) in code.output.iter().enumerate() {
                out.set(c, i, val(regs, o));
            }
        }
    }

    fn reset(&mut self) {
        self.regs.fill(0.0);
        for &(r, v) in &self.code.state_init {
            self.regs[r as usize] = v;
        }
    }

    fn handle_event(&mut self, event: &Event<'_>, _sample_rate: f32) -> bool {
        let Some(handler) = self
            .code
            .events
            .iter()
            .find(|handler| handler.name == event.name)
        else {
            return false;
        };
        let regs = &mut self.regs[..];
        let val = |regs: &[f32], o: Operand| match o {
            Operand::Reg(r) => regs[r as usize],
            Operand::Const(c) => c,
        };
        let event_value = |name: &str| {
            event
                .values
                .iter()
                .find(|value| value.name == name)
                .map(|value| value.value)
        };
        for binding in &handler.bindings {
            match binding {
                EventBinding::Scalar { name, reg } => {
                    regs[*reg as usize] = event_value(name).unwrap_or(0.0);
                }
                EventBinding::Field {
                    path,
                    fallback,
                    reg,
                } => {
                    regs[*reg as usize] = event_value(path)
                        .or_else(|| event_value(fallback))
                        .unwrap_or(0.0);
                }
            }
        }
        let mut pc = 0;
        while let Some(instr) = handler.instrs.get(pc) {
            pc += 1;
            match *instr {
                Instr::Op2 { op, dst, a, b } => {
                    regs[dst as usize] = op.apply(val(regs, a), val(regs, b));
                }
                Instr::Op1 { op, dst, x } => {
                    regs[dst as usize] = op.apply(val(regs, x), _sample_rate);
                }
                Instr::Copy { dst, src } => regs[dst as usize] = val(regs, src),
                Instr::Tune { tuning, dst, pitch } => {
                    regs[dst as usize] =
                        self.code.tunings[tuning as usize].frequency(val(regs, pitch));
                }
                Instr::Select { dst, cond, a, b } => {
                    regs[dst as usize] = if val(regs, cond) != 0.0 {
                        val(regs, a)
                    } else {
                        val(regs, b)
                    };
                }
                Instr::JumpUnless { cond, target } => {
                    if val(regs, cond) == 0.0 {
                        pc = target as usize;
                    }
                }
                Instr::Jump { target } => pc = target as usize,
            }
        }
        true
    }
}
