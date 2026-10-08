//! Bytecode for compiled rill bodies, and the node that runs it.
//!
//! A compiled instance is a flat list of register instructions run once per
//! tick. `state` lives in registers that keep their value between ticks.
//! Jumps only go forward, so every tick finishes in at most `instrs.len()`
//! steps: there are no loops at run time.

use std::collections::VecDeque;

use crate::event::{EventDecl, EventId, Payload, Sender};
use crate::node::{Context, Inputs, Node, Outputs};
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
    /// `dst = tuning(pitch, setting, a4)`; see [`Tuning::frequency`].
    Tune {
        tuning: Tuning,
        dst: u16,
        pitch: Operand,
        setting: Operand,
        a4: Operand,
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
    /// Start a sequence as described by [`Code::calls`]`[call]`, writing
    /// the instance id to the call's `dst`.
    InvokeSeq {
        call: u16,
    },
    /// Send the declared event `event`, with its payload's values in
    /// [`EventKind::fields`](crate::event::EventKind::fields) order.
    InvokeEvent {
        event: EventId,
        values: [Operand; 3],
    },
    /// Stop instance `id` of sequence `seq`, or all of them.
    Halt {
        seq: u16,
        id: Option<Operand>,
    },
    /// `dst` = captured value `index` of instance slot `slot` of `seq`.
    LoadCapture {
        dst: u16,
        seq: u16,
        slot: u16,
        index: u8,
    },
    /// Set setting `setting` (see [`Source`]) of instance slot `slot` of
    /// `seq`, if call `call` started it.
    SetSlot {
        seq: u16,
        slot: u16,
        call: u16,
        setting: u8,
        value: Operand,
    },
    /// Run [`Code::vectors`]`[block]`: the copies of a voice pool, all at
    /// once.
    Vector {
        block: u32,
    },
}

/// The copies of a voice pool run together: each instruction does the same
/// for every copy (a lane). A lane group is `lanes` registers in a row, one
/// per copy.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VectorBlock {
    pub lanes: u16,
    pub instrs: Vec<VInstr>,
}

/// An operand of a [`VInstr`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VArg {
    /// A lane group starting at this register: a value per copy.
    Lanes(u16),
    /// One register shared by every copy.
    Scalar(u16),
    Const(f32),
}

/// An instruction over every lane of a [`VectorBlock`]. `d` is the first
/// register of the lane group it writes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VInstr {
    Op2 {
        op: Op2,
        d: u16,
        a: VArg,
        b: VArg,
    },
    Op1 {
        op: Op1,
        d: u16,
        x: VArg,
    },
    Copy {
        d: u16,
        s: VArg,
    },
    /// `d = if cond { a } else { b }`, per lane.
    Select {
        d: u16,
        cond: VArg,
        a: VArg,
        b: VArg,
    },
    Tune {
        tuning: Tuning,
        d: u16,
        pitch: VArg,
        setting: VArg,
        a4: VArg,
    },
}

/// The built-in tunings, as run by [`Instr::Tune`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tuning {
    Equal,
    Just,
    Pythagorean,
    Meantone,
}

impl Tuning {
    /// The tuning behind a built-in function name.
    pub fn builtin(name: &str) -> Option<Tuning> {
        Some(match name {
            "equal" => Tuning::Equal,
            "just" => Tuning::Just,
            "pythagorean" => Tuning::Pythagorean,
            "meantone" => Tuning::Meantone,
            _ => return None,
        })
    }

    /// Frequency of `pitch`. `setting` is the number of equal steps per
    /// octave for [`Tuning::Equal`], and the root pitch for the scale
    /// tunings. A4 sounds at `a4`.
    pub fn frequency(self, pitch: f32, setting: f32, a4: f32) -> f32 {
        match self {
            Tuning::Equal => {
                // Note names land on the nearest of `setting` equal divisions;
                // anything between notes (cents, bends) stays continuous.
                let steps = setting;
                let note = pitch.floor();
                let step = ((note - 69.0) * steps / 12.0).round();
                a4 * 2.0f32.powf(step / steps + (pitch - note) / 12.0)
            }
            Tuning::Just => ratio_tuning(pitch, setting, a4, JUST_RATIOS),
            Tuning::Pythagorean => ratio_tuning(pitch, setting, a4, PYTHAGOREAN_RATIOS),
            Tuning::Meantone => ratio_tuning(pitch, setting, a4, MEANTONE_RATIOS),
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

/// An instruction as the interpreter runs it. Programs are translated into
/// these when loaded ([`lower`]): the most common operations get their own
/// variant for each shape of operands (register or constant), so running
/// one is a single dispatch with no operand checks, and every register has
/// been checked to exist, so they are read without bounds checks.
#[derive(Clone, Copy, Debug)]
enum Fast {
    AddRR {
        d: u16,
        a: u16,
        b: u16,
    },
    AddRC {
        d: u16,
        a: u16,
        c: f32,
    },
    SubRR {
        d: u16,
        a: u16,
        b: u16,
    },
    SubRC {
        d: u16,
        a: u16,
        c: f32,
    },
    SubCR {
        d: u16,
        c: f32,
        b: u16,
    },
    MulRR {
        d: u16,
        a: u16,
        b: u16,
    },
    MulRC {
        d: u16,
        a: u16,
        c: f32,
    },
    DivRR {
        d: u16,
        a: u16,
        b: u16,
    },
    DivCR {
        d: u16,
        c: f32,
        b: u16,
    },
    LtRC {
        d: u16,
        a: u16,
        c: f32,
    },
    GtRC {
        d: u16,
        a: u16,
        c: f32,
    },
    Wrap {
        d: u16,
        x: u16,
    },
    CopyR {
        d: u16,
        s: u16,
    },
    CopyC {
        d: u16,
        c: f32,
    },
    Op2RR {
        op: Op2,
        d: u16,
        a: u16,
        b: u16,
    },
    Op2RC {
        op: Op2,
        d: u16,
        a: u16,
        c: f32,
    },
    Op2CR {
        op: Op2,
        d: u16,
        c: f32,
        b: u16,
    },
    Op1 {
        op: Op1,
        d: u16,
        x: u16,
    },
    /// Skip ahead unless register `cond` is true.
    JumpUnless {
        cond: u16,
        target: u32,
    },
    Jump {
        target: u32,
    },
    /// Run a [`VectorBlock`].
    Vector {
        block: u32,
    },
    /// Anything else: run instruction `index` of the original list.
    Slow {
        index: u32,
    },
}

/// Translate `instrs` for the interpreter, checking every register is below
/// `regs` and every jump goes forward within the list.
fn lower(instrs: &[Instr], regs: usize) -> Vec<Fast> {
    let reg = |r: u16| {
        assert!((r as usize) < regs, "register r{r} out of range");
        r
    };
    let check = |i: &Instr| {
        super::opt::reads(i, &mut |r| {
            reg(r);
        });
        super::opt::writes(i, &mut |r| {
            reg(r);
        });
    };
    let len = instrs.len();
    instrs
        .iter()
        .enumerate()
        .map(|(pc, i)| {
            check(i);
            use Operand::{Const as C, Reg as R};
            match *i {
                Instr::Op2 { op, dst: d, a, b } => match (op, a, b) {
                    (Op2::Add, R(a), R(b)) => Fast::AddRR { d, a, b },
                    (Op2::Add, R(a), C(c)) | (Op2::Add, C(c), R(a)) => Fast::AddRC { d, a, c },
                    (Op2::Sub, R(a), R(b)) => Fast::SubRR { d, a, b },
                    (Op2::Sub, R(a), C(c)) => Fast::SubRC { d, a, c },
                    (Op2::Sub, C(c), R(b)) => Fast::SubCR { d, c, b },
                    (Op2::Mul, R(a), R(b)) => Fast::MulRR { d, a, b },
                    (Op2::Mul, R(a), C(c)) | (Op2::Mul, C(c), R(a)) => Fast::MulRC { d, a, c },
                    (Op2::Div, R(a), R(b)) => Fast::DivRR { d, a, b },
                    (Op2::Div, C(c), R(b)) => Fast::DivCR { d, c, b },
                    (Op2::Lt, R(a), C(c)) => Fast::LtRC { d, a, c },
                    (Op2::Gt, R(a), C(c)) => Fast::GtRC { d, a, c },
                    (op, R(a), R(b)) => Fast::Op2RR { op, d, a, b },
                    (op, R(a), C(c)) => Fast::Op2RC { op, d, a, c },
                    (op, C(c), R(b)) => Fast::Op2CR { op, d, c, b },
                    (op, C(a), C(b)) => Fast::CopyC {
                        d,
                        c: op.apply(a, b),
                    },
                },
                Instr::Op1 {
                    op: Op1::Wrap,
                    dst: d,
                    x: R(x),
                } => Fast::Wrap { d, x },
                Instr::Op1 {
                    op,
                    dst: d,
                    x: R(x),
                } => Fast::Op1 { op, d, x },
                Instr::Copy { dst: d, src: R(s) } => Fast::CopyR { d, s },
                Instr::Copy { dst: d, src: C(c) } => Fast::CopyC { d, c },
                Instr::JumpUnless {
                    cond: R(cond),
                    target,
                } => {
                    assert!(
                        target as usize > pc && target as usize <= len,
                        "jump at {pc} to {target} is not forward"
                    );
                    Fast::JumpUnless { cond, target }
                }
                Instr::JumpUnless {
                    cond: C(0.0),
                    target,
                } => Fast::Jump { target },
                Instr::Jump { target } => {
                    assert!(
                        target as usize > pc && target as usize <= len,
                        "jump at {pc} to {target} is not forward"
                    );
                    Fast::Jump { target }
                }
                Instr::Vector { block } => Fast::Vector { block },
                _ => Fast::Slow { index: pc as u32 },
            }
        })
        .collect()
}

/// One compiled instance, ready to be turned into a [`Program`] node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Code {
    pub instrs: Vec<Instr>,
    /// Runs every tick after `instrs`: settings of playing sequences that
    /// follow streams.
    pub post: Vec<Instr>,
    pub regs: usize,
    /// Register loaded from each node input at the start of every tick.
    pub input_regs: Vec<u16>,
    /// What each output channel holds at the end of a tick.
    pub output: Vec<Operand>,
    /// Registers that are `state`, with their initial values.
    pub state_init: Vec<(u16, f32)>,
    pub events: Vec<EventCode>,
    /// The program's event declarations, for matching notes from sequences.
    pub decls: Vec<EventDecl>,
    pub seqs: Vec<SeqTable>,
    /// Every `invoke` or `trigger` of a sequence, by [`Instr::InvokeSeq`].
    pub calls: Vec<InvokeCall>,
    /// Voice pools: per pool, per copy, the operands of the copy's output.
    pub pools: Vec<Vec<Vec<Operand>>>,
    /// Code that repeats with different values, which can run as lanes
    /// together: per voice pool, then per `for` loop, the range of
    /// [`Code::instrs`] each copy (or iteration) fills, when it is in the
    /// tick's code.
    pub lane_ranges: Vec<Option<Vec<(u32, u32)>>>,
    /// Copies of pools that run together, by [`Instr::Vector`].
    pub vectors: Vec<VectorBlock>,
}

/// What a handler handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handles {
    Start,
    Event(EventId),
}

/// How a handler shares events over the copies of a voice pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Plain,
    /// Runs in one copy, which then holds the note. A released copy is free
    /// once its output has been silent for `tail` ticks.
    Claim {
        tail: u32,
    },
    /// Runs in the copy holding the note.
    Release,
}

/// One `on` handler: the code to run when what it handles happens.
#[derive(Clone, Debug, PartialEq)]
pub struct EventCode {
    pub handles: Handles,
    /// Registers that receive the payload's values, in
    /// [`EventKind::fields`](crate::event::EventKind::fields) order.
    pub payload: Vec<u16>,
    pub instrs: Vec<Instr>,
    pub mode: Mode,
    /// The voice pool and copy of the rill instance it belongs to.
    pub voice: Option<(u16, u16)>,
}

/// A sequence, ready to play.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeqTable {
    pub name: String,
    /// Length of one step in beats.
    pub step_beats: f64,
    pub steps: Vec<SeqStep>,
    /// Defaults: tempo in beats per second, gate, velocity.
    pub settings: [f32; 3],
    pub repeat: u32,
    pub looping: bool,
    pub instances: u16,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeqStep {
    /// Empty for a rest.
    pub pitches: Vec<f32>,
    pub velocity: Option<f32>,
}

/// Index of each followed setting, in [`SeqTable::settings`] and
/// [`InvokeCall::settings`].
pub const TEMPO: usize = 0;
pub const GATE: usize = 1;
pub const VELOCITY: usize = 2;

/// Where a playing sequence takes a setting from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Source {
    /// The sequence's own default.
    #[default]
    Default,
    /// Read once, when invoked.
    Now(Operand),
    /// A register, read every tick while playing.
    Follow(u16),
    /// Written every tick by [`Instr::SetSlot`] in [`Code::post`]; starts
    /// at `initial`.
    PerSlot { initial: Operand },
}

/// One `invoke` or `trigger` of a sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct InvokeCall {
    pub seq: u16,
    pub id: Option<Operand>,
    /// For `trigger`: the step to start at, counting from 1.
    pub step: Option<Operand>,
    /// Receives the instance id.
    pub dst: u16,
    /// Tempo, gate and velocity.
    pub settings: [Source; 3],
    pub repeat: Option<Operand>,
    pub looping: Option<Operand>,
    /// Values kept with the instance for settings computed per instance.
    pub captures: Vec<Operand>,
}

impl Code {
    /// Human-readable listing, for debugging.
    pub fn listing(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "; {} regs, inputs {:?}, state {:?}, events {}, seqs {}, pools {}",
            self.regs,
            self.input_regs,
            self.state_init,
            self.events.len(),
            self.seqs.len(),
            self.pools.len()
        );
        list(&mut s, &self.instrs);
        if !self.post.is_empty() {
            let _ = writeln!(s, "; post");
            list(&mut s, &self.post);
        }
        let outs: Vec<String> = self.output.iter().map(show).collect();
        let _ = writeln!(s, "out {}", outs.join(", "));
        s
    }
}

fn show(o: &Operand) -> String {
    match o {
        Operand::Reg(r) => format!("r{r}"),
        Operand::Const(c) => format!("{c}"),
    }
}

fn list(s: &mut String, instrs: &[Instr]) {
    use std::fmt::Write as _;
    let op = show;
    for (pc, i) in instrs.iter().enumerate() {
        let line = match i {
            Instr::Op2 { op: o, dst, a, b } => {
                format!("r{dst} = {} {} {}", o.name(), op(a), op(b))
            }
            Instr::Op1 { op: o, dst, x } => format!("r{dst} = {} {}", o.name(), op(x)),
            Instr::Copy { dst, src } => format!("r{dst} = {}", op(src)),
            Instr::Tune {
                tuning,
                dst,
                pitch,
                setting,
                a4,
            } => format!(
                "r{dst} = tune {tuning:?} {} {} {}",
                op(pitch),
                op(setting),
                op(a4)
            ),
            Instr::Select { dst, cond, a, b } => {
                format!("r{dst} = select {} {} {}", op(cond), op(a), op(b))
            }
            Instr::JumpUnless { cond, target } => format!("unless {} goto {target}", op(cond)),
            Instr::Jump { target } => format!("goto {target}"),
            Instr::InvokeSeq { call } => format!("invoke call {call}"),
            Instr::InvokeEvent { event, values } => format!(
                "invoke event {} {} {} {}",
                event.0,
                op(&values[0]),
                op(&values[1]),
                op(&values[2])
            ),
            Instr::Halt { seq, id } => {
                format!("halt seq {seq} {}", id.as_ref().map_or("all".into(), op))
            }
            Instr::LoadCapture {
                dst,
                seq,
                slot,
                index,
            } => format!("r{dst} = capture {index} of seq {seq} slot {slot}"),
            Instr::SetSlot {
                seq,
                slot,
                call,
                setting,
                value,
            } => format!(
                "seq {seq} slot {slot} setting {setting} = {} if from call {call}",
                op(value)
            ),
            Instr::Vector { block } => format!("vector block {block}"),
        };
        let _ = writeln!(s, "{pc:4}: {line}");
    }
}

/// The release velocity of notes ended by a sequence, as MIDI's default.
const SEQ_RELEASE: f32 = 0.5;

/// Positions add up tempo / rate every tick, so a step due at 0.5 beats
/// can be reached as 0.49999999999; this keeps it on its own sample.
const EPS: f64 = 1e-9;

/// -90dB: below this a releasing voice counts as silent.
const SILENCE: f32 = 3.162_277_7e-5;

/// One playing (or idle) instance of a sequence.
#[derive(Clone, Debug, Default)]
struct Slot {
    active: bool,
    id: i32,
    /// The call that started it.
    call: u16,
    /// Tick it started at, to replace the oldest when all are busy.
    started: u64,
    /// Position in beats within the current pass.
    pos: f64,
    /// Next step to start, and how many of its notes have started.
    next: usize,
    chord: usize,
    /// Passes left, counting the current one, unless looping.
    repeats: u32,
    looping: bool,
    /// Tempo, gate and velocity.
    values: [f32; 3],
    sources: [Source; 3],
    captures: Vec<f32>,
    /// Notes sounding: pitch, and the position their note-off is due.
    held: Vec<(f32, f64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoiceState {
    Free,
    Holding,
    Releasing,
}

#[derive(Clone, Copy, Debug)]
struct Voice {
    state: VoiceState,
    from: Sender,
    pitch: f32,
    instance: i32,
    /// Tick it was claimed or released at.
    since: u64,
    silent: u32,
    tail: u32,
}

/// Something to run handlers for, in order.
#[derive(Clone, Copy, Debug)]
enum Pending {
    Start,
    /// A declared event, from `from`.
    To(EventId, Payload, Sender),
    /// A note from a sequence: every declaration that accepts it.
    Note(Sender, Payload),
}

/// Everything that changes while a [`Program`] runs.
struct Run {
    regs: Box<[f32]>,
    /// Per sequence, its instance slots.
    slots: Vec<Vec<Slot>>,
    /// Per pool, per copy.
    voices: Vec<Vec<Voice>>,
    queue: VecDeque<Pending>,
    draining: bool,
    started: bool,
    /// Ticks since the start.
    now: u64,
    /// The next fresh instance id; fresh ids count down from -1.
    fresh: i32,
    rate: f32,
    /// Per pool: the copy chosen for the event being delivered, and which
    /// delivery that was.
    picks: Vec<(u64, Option<u16>)>,
    delivery: u64,
}

/// Runs a compiled program as an engine node.
pub struct Program {
    code: Code,
    run: Run,
    /// [`Code::instrs`], [`Code::post`] and each handler, lowered.
    tick: Vec<Fast>,
    post: Vec<Fast>,
    handlers: Vec<Vec<Fast>>,
    /// Whether any voice pool has `claim` handlers to keep track of.
    claims: bool,
}

impl Program {
    pub fn new(code: Code) -> Program {
        let tick = lower(&code.instrs, code.regs);
        let post = lower(&code.post, code.regs);
        let handlers = code
            .events
            .iter()
            .map(|e| lower(&e.instrs, code.regs))
            .collect();
        let check = |r: u16| assert!((r as usize) < code.regs, "register r{r} out of range");
        for &r in code
            .input_regs
            .iter()
            .chain(code.events.iter().flat_map(|e| &e.payload))
        {
            check(r);
        }
        for o in code
            .output
            .iter()
            .chain(code.pools.iter().flatten().flatten())
        {
            if let Operand::Reg(r) = *o {
                check(r);
            }
        }
        for block in &code.vectors {
            check_vector(block, code.regs);
        }
        let claims = code
            .events
            .iter()
            .any(|e| matches!(e.mode, Mode::Claim { .. }) && e.voice.is_some());
        let captures = |seq: usize| {
            code.calls
                .iter()
                .filter(|c| usize::from(c.seq) == seq)
                .map(|c| c.captures.len())
                .max()
                .unwrap_or(0)
        };
        let slots = code
            .seqs
            .iter()
            .enumerate()
            .map(|(j, t)| {
                let chord = t.steps.iter().map(|s| s.pitches.len()).max().unwrap_or(0);
                // Each slot gets its own buffers: a clone of a `Vec` does
                // not keep its capacity, and nothing may grow while playing.
                (0..t.instances)
                    .map(|_| Slot {
                        captures: vec![0.0; captures(j)],
                        // A note can still be held when the next step starts.
                        held: Vec::with_capacity(chord * 2),
                        ..Slot::default()
                    })
                    .collect()
            })
            .collect();
        let voice = Voice {
            state: VoiceState::Free,
            from: Sender::Host(0),
            pitch: 0.0,
            instance: 0,
            since: 0,
            silent: 0,
            tail: 0,
        };
        let voices = code.pools.iter().map(|p| vec![voice; p.len()]).collect();
        // Every handler can queue a few events; this is a generous bound,
        // so the queue does not grow while playing.
        let queue = 64 + 8 * code.events.len() + 4 * code.seqs.len();
        let mut p = Program {
            run: Run {
                regs: vec![0.0; code.regs].into_boxed_slice(),
                slots,
                voices,
                queue: VecDeque::with_capacity(queue),
                draining: false,
                started: false,
                now: 0,
                fresh: -1,
                rate: 48_000.0,
                picks: vec![(0, None); code.pools.len()],
                delivery: 0,
            },
            code,
            tick,
            post,
            handlers,
            claims,
        };
        p.reset();
        p
    }

    /// Queue `p`, and run it and everything it leads to, unless a delivery
    /// is already running (it then gets to `p` in turn).
    fn deliver(&mut self, p: Pending) {
        self.run.queue.push_back(p);
        if self.run.draining {
            return;
        }
        self.run.draining = true;
        while let Some(p) = self.run.queue.pop_front() {
            match p {
                Pending::Start => self.run_handlers(Handles::Start, None, Sender::Host(0)),
                Pending::To(id, payload, from) => {
                    self.run_handlers(Handles::Event(id), Some(payload), from)
                }
                Pending::Note(from, payload) => {
                    for i in 0..self.code.decls.len() {
                        if self.code.decls[i].accepts(from, 0, payload.kind()) {
                            let id = EventId(i as u16);
                            self.run_handlers(Handles::Event(id), Some(payload), from);
                        }
                    }
                }
            }
        }
        self.run.draining = false;
    }

    /// Run the `on start` handlers, once, before anything else.
    fn ensure_started(&mut self) {
        if !self.run.started {
            self.run.started = true;
            self.deliver(Pending::Start);
        }
    }

    /// Run every handler of `handles`, sharing notes out over voice pools.
    fn run_handlers(&mut self, handles: Handles, payload: Option<Payload>, from: Sender) {
        self.run.delivery += 1;
        for h in 0..self.code.events.len() {
            let handler = &self.code.events[h];
            if handler.handles != handles {
                continue;
            }
            // In a voice pool, `claim` and `release` handlers run in one copy.
            let runs = match (handler.mode, handler.voice, payload) {
                (Mode::Claim { tail }, Some((pool, copy)), Some(payload)) => {
                    self.claim(pool, tail, from, &payload) == copy
                }
                (Mode::Release, Some((pool, copy)), Some(payload)) => {
                    self.release(pool, from, &payload) == Some(copy)
                }
                _ => true,
            };
            if !runs {
                continue;
            }
            let handler = &self.code.events[h];
            if let Some(payload) = payload {
                for (&reg, value) in handler.payload.iter().zip(payload.values()) {
                    self.run.regs[reg as usize] = value;
                }
            }
            exec(
                &self.handlers[h],
                &handler.instrs,
                &self.code,
                &mut self.run,
            );
        }
    }

    /// The copy of `pool` that takes a new note: a free one, else the one
    /// released longest ago, else the one holding its note longest.
    fn claim(&mut self, pool: u16, tail: u32, from: Sender, payload: &Payload) -> u16 {
        let p = usize::from(pool);
        if let (stamp, Some(copy)) = self.run.picks[p]
            && stamp == self.run.delivery
        {
            return copy;
        }
        let voices = &mut self.run.voices[p];
        let oldest = |state: VoiceState| {
            voices
                .iter()
                .enumerate()
                .filter(|(_, v)| v.state == state)
                .min_by_key(|(_, v)| v.since)
                .map(|(i, _)| i)
        };
        let copy = voices
            .iter()
            .position(|v| v.state == VoiceState::Free)
            .or_else(|| oldest(VoiceState::Releasing))
            .or_else(|| oldest(VoiceState::Holding))
            .unwrap_or(0);
        let (pitch, instance) = payload.note().unwrap_or((0.0, 0));
        voices[copy] = Voice {
            state: VoiceState::Holding,
            from,
            pitch,
            instance,
            since: self.run.now,
            silent: 0,
            tail,
        };
        self.run.picks[p] = (self.run.delivery, Some(copy as u16));
        copy as u16
    }

    /// The copy of `pool` holding the note `payload` ends, which is then
    /// releasing.
    fn release(&mut self, pool: u16, from: Sender, payload: &Payload) -> Option<u16> {
        let p = usize::from(pool);
        let (stamp, pick) = self.run.picks[p];
        if stamp == self.run.delivery {
            return pick;
        }
        let (pitch, instance) = payload.note()?;
        let voices = &mut self.run.voices[p];
        let copy = voices
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v.state == VoiceState::Holding
                    && v.from == from
                    && v.pitch == pitch
                    && v.instance == instance
            })
            .min_by_key(|(_, v)| v.since)
            .map(|(i, _)| i);
        if let Some(c) = copy {
            voices[c].state = VoiceState::Releasing;
            voices[c].since = self.run.now;
            voices[c].silent = 0;
        }
        let pick = copy.map(|c| c as u16);
        self.run.picks[p] = (self.run.delivery, pick);
        pick
    }

    /// Notes of every playing sequence that are due now.
    fn fire_sequences(&mut self) {
        for j in 0..self.code.seqs.len() {
            for k in 0..self.run.slots[j].len() {
                while let Some(p) = next_note(&self.code.seqs[j], j, &mut self.run.slots[j][k]) {
                    self.deliver(p);
                }
            }
        }
    }

    /// Move every playing sequence on by one tick.
    fn advance_sequences(&mut self) {
        let rate = f64::from(self.run.rate);
        for slots in &mut self.run.slots {
            for slot in slots.iter_mut().filter(|s| s.active) {
                for s in 0..3 {
                    if let Source::Follow(r) = slot.sources[s] {
                        slot.values[s] = self.run.regs[r as usize];
                    }
                }
                slot.pos += f64::from(slot.values[TEMPO].max(0.0)) / rate;
            }
        }
    }

    /// Releasing voices whose output stayed silent for their tail are free.
    fn track_voices(&mut self) {
        let regs = &self.run.regs;
        for (p, copies) in self.code.pools.iter().enumerate() {
            for (c, outputs) in copies.iter().enumerate() {
                let v = &mut self.run.voices[p][c];
                if v.state != VoiceState::Releasing {
                    continue;
                }
                let quiet = outputs.iter().all(|&o| val(regs, o).abs() < SILENCE);
                v.silent = if quiet { v.silent + 1 } else { 0 };
                if v.silent >= v.tail {
                    v.state = VoiceState::Free;
                }
            }
        }
    }
}

fn val(regs: &[f32], o: Operand) -> f32 {
    match o {
        Operand::Reg(r) => regs[r as usize],
        Operand::Const(c) => c,
    }
}

/// The next due note of `slot` (an instance of sequence `seq`), moving it
/// on; `None` when nothing more is due this tick.
fn next_note(table: &SeqTable, seq: usize, slot: &mut Slot) -> Option<Pending> {
    let from = Sender::Seq(seq as u16);
    let sb = table.step_beats;
    let len = table.steps.len();
    let total = len as f64 * sb;
    loop {
        if !slot.active {
            return None;
        }
        if let Some(h) = slot.held.iter().position(|&(_, off)| off <= slot.pos + EPS) {
            let (pitch, _) = slot.held.swap_remove(h);
            return Some(Pending::Note(
                from,
                Payload::NoteOff {
                    pitch,
                    release: SEQ_RELEASE,
                    instance: slot.id,
                },
            ));
        }
        if slot.next < len && slot.next as f64 * sb <= slot.pos + EPS {
            let step = &table.steps[slot.next];
            if let Some(&pitch) = step.pitches.get(slot.chord) {
                slot.chord += 1;
                let gate = f64::from(slot.values[GATE].clamp(1e-6, 1.0));
                slot.held.push((pitch, slot.next as f64 * sb + gate * sb));
                return Some(Pending::Note(
                    from,
                    Payload::NoteOn {
                        pitch,
                        velocity: step.velocity.unwrap_or(slot.values[VELOCITY]),
                        instance: slot.id,
                    },
                ));
            }
            slot.next += 1;
            slot.chord = 0;
            continue;
        }
        if slot.next >= len && slot.pos + EPS >= total {
            if slot.looping || slot.repeats > 1 {
                if !slot.looping {
                    slot.repeats -= 1;
                }
                slot.pos -= total;
                for h in &mut slot.held {
                    h.1 -= total;
                }
                slot.next = 0;
                slot.chord = 0;
                continue;
            }
            if slot.held.is_empty() {
                slot.active = false;
            }
        }
        return None;
    }
}

/// Stop `slot`, queueing a note-off for every note it holds.
fn halt(slot: &mut Slot, seq: usize, queue: &mut VecDeque<Pending>) {
    for (pitch, _) in slot.held.drain(..) {
        queue.push_back(Pending::Note(
            Sender::Seq(seq as u16),
            Payload::NoteOff {
                pitch,
                release: SEQ_RELEASE,
                instance: slot.id,
            },
        ));
    }
    slot.active = false;
}

/// Run `fast` (lowered from `instrs`) to the end.
fn exec(fast: &[Fast], instrs: &[Instr], code: &Code, run: &mut Run) {
    let rate = run.rate;
    let mut pc = 0;
    while let Some(&f) = fast.get(pc) {
        pc += 1;
        let regs = &mut run.regs[..];
        // SAFETY: `lower` checked that every register in `fast` is below
        // `code.regs`, the length of `regs`.
        macro_rules! r {
            ($i:expr) => {
                *unsafe { regs.get_unchecked($i as usize) }
            };
        }
        macro_rules! set {
            ($i:expr, $v:expr) => {{
                let v = $v;
                *unsafe { regs.get_unchecked_mut($i as usize) } = v;
            }};
        }
        match f {
            Fast::AddRR { d, a, b } => set!(d, r!(a) + r!(b)),
            Fast::AddRC { d, a, c } => set!(d, r!(a) + c),
            Fast::SubRR { d, a, b } => set!(d, r!(a) - r!(b)),
            Fast::SubRC { d, a, c } => set!(d, r!(a) - c),
            Fast::SubCR { d, c, b } => set!(d, c - r!(b)),
            Fast::MulRR { d, a, b } => set!(d, r!(a) * r!(b)),
            Fast::MulRC { d, a, c } => set!(d, r!(a) * c),
            Fast::DivRR { d, a, b } => set!(d, r!(a) / r!(b)),
            Fast::DivCR { d, c, b } => set!(d, c / r!(b)),
            Fast::LtRC { d, a, c } => set!(d, if r!(a) < c { 1.0 } else { 0.0 }),
            Fast::GtRC { d, a, c } => set!(d, if r!(a) > c { 1.0 } else { 0.0 }),
            Fast::Wrap { d, x } => {
                let x = r!(x);
                set!(d, x - x.floor())
            }
            Fast::CopyR { d, s } => set!(d, r!(s)),
            Fast::CopyC { d, c } => set!(d, c),
            Fast::Op2RR { op, d, a, b } => set!(d, op.apply(r!(a), r!(b))),
            Fast::Op2RC { op, d, a, c } => set!(d, op.apply(r!(a), c)),
            Fast::Op2CR { op, d, c, b } => set!(d, op.apply(c, r!(b))),
            Fast::Op1 { op, d, x } => set!(d, op.apply(r!(x), rate)),
            Fast::JumpUnless { cond, target } => {
                if r!(cond) == 0.0 {
                    pc = target as usize;
                }
            }
            Fast::Jump { target } => pc = target as usize,
            Fast::Vector { block } => run_vector(&code.vectors[block as usize], regs, rate),
            Fast::Slow { index } => {
                if let Some(target) = step(&instrs[index as usize], code, run) {
                    pc = target;
                }
            }
        }
    }
}

/// Check that every register a vector block uses exists.
fn check_vector(block: &VectorBlock, regs: usize) {
    let n = usize::from(block.lanes);
    assert!(n > 0, "a vector block needs lanes");
    let arg = |a: VArg| match a {
        VArg::Lanes(b) => assert!(usize::from(b) + n <= regs, "lane group r{b} out of range"),
        VArg::Scalar(r) => assert!(usize::from(r) < regs, "register r{r} out of range"),
        VArg::Const(_) => {}
    };
    for i in &block.instrs {
        let d = match *i {
            VInstr::Op2 { d, a, b, .. } => {
                arg(a);
                arg(b);
                d
            }
            VInstr::Op1 { d, x, .. } => {
                arg(x);
                d
            }
            VInstr::Copy { d, s } => {
                arg(s);
                d
            }
            VInstr::Select { d, cond, a, b } => {
                arg(cond);
                arg(a);
                arg(b);
                d
            }
            VInstr::Tune {
                d,
                pitch,
                setting,
                a4,
                ..
            } => {
                arg(pitch);
                arg(setting);
                arg(a4);
                d
            }
        };
        arg(VArg::Lanes(d));
    }
}

/// Where a vector operand's values come from, settled before a loop runs.
#[derive(Clone, Copy)]
enum Src {
    /// One value per lane, starting here.
    Lanes(*const f32),
    /// The same value in every lane.
    Splat(f32),
}

/// Run every instruction of `block` over all of its lanes.
fn run_vector(block: &VectorBlock, regs: &mut [f32], rate: f32) {
    let n = usize::from(block.lanes);
    let p = regs.as_mut_ptr();
    // SAFETY: `check_vector` made sure every lane group and register is
    // inside `regs`. Lane groups never partly overlap: a write goes to the
    // same lane it reads, or to a different group.
    unsafe {
        let src = |a: VArg| match a {
            VArg::Lanes(b) => Src::Lanes(p.add(usize::from(b))),
            VArg::Scalar(r) => Src::Splat(*p.add(usize::from(r))),
            VArg::Const(c) => Src::Splat(c),
        };
        // Each loop has its operand shapes fixed and a single operation in
        // its body, so the compiler can turn it into SIMD.
        macro_rules! unary {
            ($d:expr, $x:expr, $f:expr) => {{
                let d = p.add(usize::from($d));
                let f = $f;
                match src($x) {
                    Src::Lanes(x) => {
                        for k in 0..n {
                            *d.add(k) = f(*x.add(k));
                        }
                    }
                    Src::Splat(x) => {
                        let v = f(x);
                        for k in 0..n {
                            *d.add(k) = v;
                        }
                    }
                }
            }};
        }
        macro_rules! binary {
            ($d:expr, $a:expr, $b:expr, $f:expr) => {{
                let d = p.add(usize::from($d));
                let f = $f;
                match (src($a), src($b)) {
                    (Src::Lanes(a), Src::Lanes(b)) => {
                        for k in 0..n {
                            *d.add(k) = f(*a.add(k), *b.add(k));
                        }
                    }
                    (Src::Lanes(a), Src::Splat(y)) => {
                        for k in 0..n {
                            *d.add(k) = f(*a.add(k), y);
                        }
                    }
                    (Src::Splat(x), Src::Lanes(b)) => {
                        for k in 0..n {
                            *d.add(k) = f(x, *b.add(k));
                        }
                    }
                    (Src::Splat(x), Src::Splat(y)) => {
                        let v = f(x, y);
                        for k in 0..n {
                            *d.add(k) = v;
                        }
                    }
                }
            }};
        }
        let truth = |b: bool| if b { 1.0f32 } else { 0.0 };
        for i in &block.instrs {
            match *i {
                VInstr::Op2 { op, d, a, b } => match op {
                    Op2::Add => binary!(d, a, b, |x: f32, y: f32| x + y),
                    Op2::Sub => binary!(d, a, b, |x: f32, y: f32| x - y),
                    Op2::Mul => binary!(d, a, b, |x: f32, y: f32| x * y),
                    Op2::Div => binary!(d, a, b, |x: f32, y: f32| x / y),
                    Op2::Min => binary!(d, a, b, |x: f32, y: f32| x.min(y)),
                    Op2::Max => binary!(d, a, b, |x: f32, y: f32| x.max(y)),
                    Op2::Lt => binary!(d, a, b, |x: f32, y: f32| truth(x < y)),
                    Op2::Le => binary!(d, a, b, |x: f32, y: f32| truth(x <= y)),
                    Op2::Gt => binary!(d, a, b, |x: f32, y: f32| truth(x > y)),
                    Op2::Ge => binary!(d, a, b, |x: f32, y: f32| truth(x >= y)),
                    Op2::Eq => binary!(d, a, b, |x: f32, y: f32| truth(x == y)),
                    Op2::Ne => binary!(d, a, b, |x: f32, y: f32| truth(x != y)),
                    Op2::And => binary!(d, a, b, |x: f32, y: f32| truth(x != 0.0 && y != 0.0)),
                    Op2::Or => binary!(d, a, b, |x: f32, y: f32| truth(x != 0.0 || y != 0.0)),
                    op => binary!(d, a, b, |x: f32, y: f32| op.apply(x, y)),
                },
                VInstr::Op1 { op, d, x } => match op {
                    Op1::Wrap => unary!(d, x, |v: f32| v - crate::ops::floor(v)),
                    Op1::Floor => unary!(d, x, crate::ops::floor),
                    Op1::Neg => unary!(d, x, |v: f32| -v),
                    Op1::Abs => unary!(d, x, f32::abs),
                    Op1::Not => unary!(d, x, |v: f32| truth(v == 0.0)),
                    op => unary!(d, x, |v: f32| op.apply(v, rate)),
                },
                VInstr::Copy { d, s } => unary!(d, s, |v: f32| v),
                VInstr::Select { d, cond, a, b } => {
                    let dp = p.add(usize::from(d));
                    match (src(cond), src(a), src(b)) {
                        (Src::Lanes(c), Src::Lanes(a), Src::Lanes(b)) => {
                            for k in 0..n {
                                let (x, y) = (*a.add(k), *b.add(k));
                                *dp.add(k) = if *c.add(k) != 0.0 { x } else { y };
                            }
                        }
                        (Src::Lanes(c), Src::Splat(x), Src::Lanes(b)) => {
                            for k in 0..n {
                                let y = *b.add(k);
                                *dp.add(k) = if *c.add(k) != 0.0 { x } else { y };
                            }
                        }
                        (Src::Lanes(c), Src::Lanes(a), Src::Splat(y)) => {
                            for k in 0..n {
                                let x = *a.add(k);
                                *dp.add(k) = if *c.add(k) != 0.0 { x } else { y };
                            }
                        }
                        (c, a, b) => {
                            let at = |s: Src, k: usize| match s {
                                Src::Lanes(q) => *q.add(k),
                                Src::Splat(v) => v,
                            };
                            for k in 0..n {
                                *dp.add(k) = if at(c, k) != 0.0 { at(a, k) } else { at(b, k) };
                            }
                        }
                    }
                }
                VInstr::Tune {
                    tuning,
                    d,
                    pitch,
                    setting,
                    a4,
                } => {
                    let dp = p.add(usize::from(d));
                    let at = |s: Src, k: usize| match s {
                        Src::Lanes(q) => *q.add(k),
                        Src::Splat(v) => v,
                    };
                    let (pi, se, a) = (src(pitch), src(setting), src(a4));
                    for k in 0..n {
                        *dp.add(k) = tuning.frequency(at(pi, k), at(se, k), at(a, k));
                    }
                }
            }
        }
    }
}

/// Run one instruction the general way; a jump's target if it jumps.
fn step(instr: &Instr, code: &Code, run: &mut Run) -> Option<usize> {
    let rate = run.rate;
    let regs = &mut run.regs[..];
    match *instr {
        Instr::Op2 { op, dst, a, b } => {
            regs[dst as usize] = op.apply(val(regs, a), val(regs, b));
        }
        Instr::Op1 { op, dst, x } => {
            regs[dst as usize] = op.apply(val(regs, x), rate);
        }
        Instr::Copy { dst, src } => regs[dst as usize] = val(regs, src),
        Instr::Tune {
            tuning,
            dst,
            pitch,
            setting,
            a4,
        } => {
            regs[dst as usize] =
                tuning.frequency(val(regs, pitch), val(regs, setting), val(regs, a4));
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
                return Some(target as usize);
            }
        }
        Instr::Jump { target } => return Some(target as usize),
        Instr::Vector { block } => run_vector(&code.vectors[block as usize], regs, rate),
        Instr::InvokeSeq { call } => invoke(call, code, run),
        Instr::InvokeEvent { event, values } => {
            let kind = code.decls[usize::from(event.0)].kind;
            let values = values.map(|v| val(regs, v));
            let payload = Payload::from_values(kind, values);
            run.queue
                .push_back(Pending::To(event, payload, Sender::Host(0)));
        }
        Instr::Halt { seq, id } => {
            let id = id.map(|id| val(regs, id) as i32);
            let seq = usize::from(seq);
            for slot in &mut run.slots[seq] {
                if slot.active && id.is_none_or(|id| id == slot.id) {
                    halt(slot, seq, &mut run.queue);
                }
            }
        }
        Instr::LoadCapture {
            dst,
            seq,
            slot,
            index,
        } => {
            let slot = &run.slots[usize::from(seq)][usize::from(slot)];
            regs[dst as usize] = slot
                .captures
                .get(usize::from(index))
                .copied()
                .unwrap_or(0.0);
        }
        Instr::SetSlot {
            seq,
            slot,
            call,
            setting,
            value,
        } => {
            let v = val(regs, value);
            let slot = &mut run.slots[usize::from(seq)][usize::from(slot)];
            if slot.active && slot.call == call {
                slot.values[usize::from(setting)] = v;
            }
        }
    }
    None
}

/// `invoke` or `trigger`: start an instance, or leave a playing one alone.
fn invoke(call_index: u16, code: &Code, run: &mut Run) {
    let call = &code.calls[usize::from(call_index)];
    let seq = usize::from(call.seq);
    let table = &code.seqs[seq];
    let regs = &run.regs;
    let id = match call.id {
        Some(id) => val(regs, id) as i32,
        None => {
            let id = run.fresh;
            run.fresh = run.fresh.wrapping_sub(1).min(-1);
            id
        }
    };
    let len = table.steps.len().max(1) as i64;
    let start = call
        .step
        .map(|s| ((val(regs, s) as i64 - 1).rem_euclid(len)) as usize);
    let repeat = call
        .repeat
        .map_or(table.repeat, |r| val(regs, r).max(1.0) as u32);
    let looping = call.looping.map_or(table.looping, |l| val(regs, l) != 0.0);
    let mut values = table.settings;
    for (value, source) in values.iter_mut().zip(call.settings) {
        match source {
            Source::Default => {}
            Source::Now(op) | Source::PerSlot { initial: op } => *value = val(regs, op),
            Source::Follow(r) => *value = regs[r as usize],
        }
    }
    let captured: [f32; 16] = {
        let mut c = [0.0; 16];
        for (i, &op) in call.captures.iter().take(16).enumerate() {
            c[i] = val(regs, op);
        }
        c
    };
    run.regs[call.dst as usize] = id as f32;
    // A sequence that would not move never starts: at 0bpm (or below), its
    // first notes would sound and then hold forever.
    if values[TEMPO].is_nan() || values[TEMPO] <= 0.0 {
        return;
    }

    let slots = &mut run.slots[seq];
    let existing = slots
        .iter()
        .position(|s| s.active && s.id == id && call.id.is_some());
    let k = match existing {
        Some(_) if start.is_none() => return,
        Some(k) => {
            halt(&mut slots[k], seq, &mut run.queue);
            k
        }
        None => match slots.iter().position(|s| !s.active) {
            Some(k) => k,
            None => {
                let k = (0..slots.len())
                    .min_by_key(|&k| slots[k].started)
                    .unwrap_or(0);
                halt(&mut slots[k], seq, &mut run.queue);
                k
            }
        },
    };
    let slot = &mut slots[k];
    let step = start.unwrap_or(0);
    slot.active = true;
    slot.id = id;
    slot.call = call_index;
    slot.started = run.now;
    slot.pos = step as f64 * table.step_beats;
    slot.next = step;
    slot.chord = 0;
    slot.repeats = repeat;
    slot.looping = looping;
    slot.values = values;
    slot.sources = call.settings;
    for (i, c) in slot.captures.iter_mut().enumerate() {
        *c = captured.get(i).copied().unwrap_or(0.0);
    }
    slot.held.clear();
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
        self.run.rate = ctx.sample_rate;
        self.ensure_started();
        let sequences = !self.code.seqs.is_empty();
        for i in 0..ctx.frames {
            if sequences {
                self.fire_sequences();
            }
            for (k, &r) in self.code.input_regs.iter().enumerate() {
                self.run.regs[r as usize] = inputs.get(k).at(i);
            }
            exec(&self.tick, &self.code.instrs, &self.code, &mut self.run);
            if !self.post.is_empty() {
                exec(&self.post, &self.code.post, &self.code, &mut self.run);
            }
            for (c, &o) in self.code.output.iter().enumerate() {
                out.set(c, i, val(&self.run.regs, o));
            }
            if self.claims {
                self.track_voices();
            }
            if !self.code.seqs.is_empty() {
                self.advance_sequences();
            }
            self.run.now += 1;
        }
    }

    fn program_size(&self) -> Option<(usize, usize)> {
        Some((
            self.code.instrs.len() + self.code.post.len(),
            self.code.regs,
        ))
    }

    fn reset(&mut self) {
        let run = &mut self.run;
        run.regs.fill(0.0);
        for &(r, v) in &self.code.state_init {
            run.regs[r as usize] = v;
        }
        for slot in run.slots.iter_mut().flatten() {
            slot.active = false;
            slot.held.clear();
        }
        for v in run.voices.iter_mut().flatten() {
            v.state = VoiceState::Free;
        }
        run.queue.clear();
        run.draining = false;
        run.started = false;
        run.now = 0;
        run.fresh = -1;
        run.picks.fill((0, None));
        run.delivery = 0;
    }

    fn handle_event(
        &mut self,
        event: EventId,
        payload: &Payload,
        from: Sender,
        sample_rate: f32,
    ) -> bool {
        self.run.rate = sample_rate;
        self.ensure_started();
        let handled = self
            .code
            .events
            .iter()
            .any(|h| h.handles == Handles::Event(event));
        self.deliver(Pending::To(event, *payload, from));
        handled
    }
}
