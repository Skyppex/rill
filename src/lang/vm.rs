//! Bytecode for compiled rill bodies, and the node that runs it.
//!
//! A compiled instance is a flat list of register instructions run once per
//! tick. `state` lives in registers that keep their value between ticks.
//! Jumps only go forward, so every tick finishes in at most `instrs.len()`
//! steps: there are no loops at run time.

use std::collections::VecDeque;

use crate::event::{EventDecl, EventId, EventKind, Payload, Sender};
use crate::node::{Context, Inputs, Node, Outputs, Signal};
use crate::ops::{Op1, Op2};

/// A register's number.
pub type Reg = u32;

/// An instruction operand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Operand {
    Reg(Reg),
    Const(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Instr {
    Op2 {
        op: Op2,
        dst: Reg,
        a: Operand,
        b: Operand,
    },
    Op1 {
        op: Op1,
        dst: Reg,
        x: Operand,
    },
    Copy {
        dst: Reg,
        src: Operand,
    },
    /// `dst = tuning(pitch, setting, a4)`; see [`Tuning::frequency`].
    Tune {
        tuning: Tuning,
        dst: Reg,
        pitch: Operand,
        setting: Operand,
        a4: Operand,
    },
    /// `dst = if cond { a } else { b }`
    Select {
        dst: Reg,
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
    /// Set value `index` of the next event sent, in
    /// [`EventKind::fields`](crate::event::EventKind::fields) order.
    EventArg {
        index: u8,
        value: Operand,
    },
    /// Send the declared event `event`, with the values its
    /// [`Instr::EventArg`]s set.
    InvokeEvent {
        event: EventId,
    },
    /// Stop instance `id` of sequence `seq`, or all of them.
    Halt {
        seq: u16,
        id: Option<Operand>,
    },
    /// `dst` = captured value `index` of instance slot `slot` of `seq`.
    LoadCapture {
        dst: Reg,
        seq: u16,
        slot: u16,
        index: u8,
    },
    /// `dst` = `info` of the instance of `seq` whose id is `id`, as it is
    /// at this sample (see [`seq_read`]).
    SeqRead {
        dst: Reg,
        seq: u16,
        info: SeqInfo,
        id: Operand,
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
    /// `dst` = the next number in [-1, 1) from noise generator `source`.
    Noise {
        dst: Reg,
        source: u16,
    },
    /// `dst` = what was written to delay line `line` `time` samples ago,
    /// interpolated between samples. Reads come before the tick's write. A
    /// time longer than the line is noted in ceiling `want`.
    DelayRead {
        dst: Reg,
        line: u16,
        time: Operand,
        want: u16,
    },
    /// `dst` = channel `channel` of audio file `audio` (see
    /// [`Code::audios`]) at frame `at` plus `part` of one from its start,
    /// read between frames (see [`read_audio`]). The two are added in `f64`,
    /// so a position far into a long file stays exact.
    AudioRead {
        dst: Reg,
        audio: u16,
        channel: u16,
        at: Operand,
        part: Operand,
    },
    /// Note in ceiling `slot` if `x` goes past `ceiling` (see
    /// [`Code::ceilings`]).
    Want {
        slot: u16,
        x: Operand,
        ceiling: f32,
    },
    /// Write this tick's input to delay line `line`, after its reads.
    DelayWrite {
        line: u16,
        x: Operand,
    },
    /// `dst = init + r[base] + r[base + stride] + ...`, `n` registers, added
    /// one at a time in that order: what a chain of adds over a lane group
    /// (`sum` of a pool's copies) becomes. See [`super::opt::sums`].
    Sum {
        dst: Reg,
        init: Operand,
        base: Reg,
        n: u32,
        stride: u32,
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
    Lanes(Reg),
    /// One register shared by every copy.
    Scalar(Reg),
    Const(f32),
}

/// An instruction over every lane of a [`VectorBlock`]. `d` is the first
/// register of the lane group it writes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VInstr {
    Op2 {
        op: Op2,
        d: Reg,
        a: VArg,
        b: VArg,
    },
    Op1 {
        op: Op1,
        d: Reg,
        x: VArg,
    },
    Copy {
        d: Reg,
        s: VArg,
    },
    /// `d = if cond { a } else { b }`, per lane.
    Select {
        d: Reg,
        cond: VArg,
        a: VArg,
        b: VArg,
    },
    Tune {
        tuning: Tuning,
        d: Reg,
        pitch: VArg,
        setting: VArg,
        a4: VArg,
    },
    /// Lane `k` takes the next number from noise generator
    /// `source + k * stride`: a copy with several generators has them in a
    /// row, so the same generator of each copy is `stride` apart.
    Noise {
        d: Reg,
        source: u16,
        stride: u16,
    },
    /// Lane `k` reads delay line `line + k * stride` as [`Instr::DelayRead`]
    /// does, noting a time past the line in ceiling `want + k * want_stride`.
    /// A stride of 0 has every lane read the same line (the taps of one
    /// multi-tap delay).
    DelayRead {
        d: Reg,
        line: u16,
        stride: u16,
        time: VArg,
        want: u16,
        want_stride: u16,
    },
    /// Lane `k` reads channel `channel` of audio file `audio` at lane `k`
    /// of `at`, as [`Instr::AudioRead`] does.
    AudioRead {
        d: Reg,
        audio: u16,
        channel: u16,
        at: VArg,
        part: VArg,
    },
    /// Lane `k` reads `info` of the instance of `seq` whose id is lane `k`
    /// of `id`, as [`Instr::SeqRead`] does.
    SeqRead {
        d: Reg,
        seq: u16,
        info: SeqInfo,
        id: VArg,
    },
    /// Lane `k` writes delay line `line + k * stride` as
    /// [`Instr::DelayWrite`] does; every lane has its own line.
    DelayWrite {
        line: u16,
        stride: u16,
        x: VArg,
    },
}

/// The prelude's tunings, as run by [`Instr::Tune`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tuning {
    Equal,
    Just,
    Pythagorean,
    Meantone,
}

impl Tuning {
    /// The tuning behind a prelude function name.
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
        d: Reg,
        a: Reg,
        b: Reg,
    },
    AddRC {
        d: Reg,
        a: Reg,
        c: f32,
    },
    SubRR {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    SubRC {
        d: Reg,
        a: Reg,
        c: f32,
    },
    SubCR {
        d: Reg,
        c: f32,
        b: Reg,
    },
    MulRR {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    MulRC {
        d: Reg,
        a: Reg,
        c: f32,
    },
    DivRR {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    DivCR {
        d: Reg,
        c: f32,
        b: Reg,
    },
    LtRC {
        d: Reg,
        a: Reg,
        c: f32,
    },
    GtRC {
        d: Reg,
        a: Reg,
        c: f32,
    },
    Wrap {
        d: Reg,
        x: Reg,
    },
    CopyR {
        d: Reg,
        s: Reg,
    },
    CopyC {
        d: Reg,
        c: f32,
    },
    Op2RR {
        op: Op2,
        d: Reg,
        a: Reg,
        b: Reg,
    },
    Op2RC {
        op: Op2,
        d: Reg,
        a: Reg,
        c: f32,
    },
    Op2CR {
        op: Op2,
        d: Reg,
        c: f32,
        b: Reg,
    },
    Op1 {
        op: Op1,
        d: Reg,
        x: Reg,
    },
    /// Skip ahead unless register `cond` is true.
    JumpUnless {
        cond: Reg,
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
    let reg = |r: Reg| {
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
    pub input_regs: Vec<Reg>,
    /// What each output channel holds at the end of a tick.
    pub output: Vec<Operand>,
    /// Registers that are `state`, with their initial values.
    pub state_init: Vec<(Reg, f32)>,
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
    /// A name for each entry of [`Code::lane_ranges`], for reports: a
    /// pool's state key, or where a `for` loop or a lifted operator is.
    pub lane_names: Vec<String>,
    /// Copies of pools that run together, by [`Instr::Vector`].
    pub vectors: Vec<VectorBlock>,
    /// Whether each pool (or loop) with copies to run together does, and
    /// why not if not. Filled by the vectoriser; empty without it.
    pub grouping: Vec<Grouping>,
    /// Which parts of the tick can run over a stretch of samples at once
    /// (see [`super::stages`]).
    pub stages: super::stages::Stages,
    /// Per noise generator ([`Instr::Noise`]), its seed.
    pub noises: Vec<u64>,
    /// Per delay line ([`Instr::DelayRead`]), its length in samples,
    /// allocated when the program is built.
    pub delays: Vec<u32>,
    /// Per audio file the program reads (see [`Instr::AudioRead`]), its
    /// samples at the host rate, one buffer per channel.
    pub audios: Vec<std::sync::Arc<[Box<[f32]>]>>,
    /// What every piece of state is called, so a new build of the program
    /// can take it over (see [`StateKeys`]).
    pub keys: StateKeys,
    /// The places sized when the program is built (`ceiling()` calls and
    /// delay lines), by state key, with the size they were given: in the
    /// value's own units, seconds for a delay line. A value past one is
    /// noted, so a new build can make it bigger.
    pub ceilings: Vec<(String, f32)>,
}

/// Stable names for a program's state, so a rebuilt program can carry on
/// where the running one is. A key is a path of names: the `let` (or
/// `state`, assignment, `return` or handler) each call is in, then the
/// callee and its count among the unnamed calls of that callee in that
/// statement, as in `main#1/lead/saw#1/return/phasor#1.phase`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StateKeys {
    /// `state` registers.
    pub regs: Vec<(String, u32)>,
    /// Delay lines, by index into [`Code::delays`].
    pub delays: Vec<(String, u32)>,
    /// Noise generators, by index into [`Code::noises`].
    pub noises: Vec<(String, u32)>,
    /// Voice pools, by index into [`Code::pools`].
    pub pools: Vec<(String, u32)>,
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
    /// Runs in the copy holding the note, which takes the new pitch: a
    /// `note_change`.
    Held,
}

/// One `on` handler: the code to run when what it handles happens.
#[derive(Clone, Debug, PartialEq)]
pub struct EventCode {
    pub handles: Handles,
    /// Registers that receive the payload's values, in
    /// [`EventKind::fields`](crate::event::EventKind::fields) order.
    pub payload: Vec<Reg>,
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
    /// Defaults, in [`TEMPO`]... [`RELEASE`] order (a tempo in beats per
    /// second).
    pub settings: [f32; SETTINGS],
    pub repeat: u32,
    pub looping: bool,
    pub instances: u16,
    /// The top of the meter: beats in a bar.
    pub beats_per_bar: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeqStep {
    /// Empty for a rest.
    pub pitches: Vec<f32>,
    /// The step's own velocity, pressure, timbre and release, where it
    /// gives them (in [`VELOCITY`]... [`RELEASE`] order, from index 0).
    pub values: [Option<f32>; 4],
    /// `~` on its own: the notes held into it go on through it.
    pub held: bool,
    /// `C2~`: its notes go on into the next step.
    pub hold: bool,
}

/// Index of each followed setting, in [`SeqTable::settings`] and
/// [`InvokeCall::settings`].
pub const TEMPO: usize = 0;
pub const GATE: usize = 1;
pub const VELOCITY: usize = 2;
pub const PRESSURE: usize = 3;
pub const TIMBRE: usize = 4;
pub const RELEASE: usize = 5;
/// How many settings a sequence has that can follow streams.
pub const SETTINGS: usize = 6;

/// What [`Instr::SeqRead`] reads of a playing instance: `seq[id].field`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqInfo {
    Tempo,
    StepTime,
    BeatTime,
    Step,
    Pass,
    Progress,
    Playing,
}

impl SeqInfo {
    /// The field named `name`.
    pub fn from_name(name: &str) -> Option<SeqInfo> {
        Some(match name {
            "tempo" => SeqInfo::Tempo,
            "step_time" => SeqInfo::StepTime,
            "beat_time" => SeqInfo::BeatTime,
            "step" => SeqInfo::Step,
            "pass" => SeqInfo::Pass,
            "progress" => SeqInfo::Progress,
            "playing" => SeqInfo::Playing,
            _ => return None,
        })
    }
}

/// What a step or beat lasts while a sequence holds still (a tempo of 0 or
/// below): long enough to mean "not moving", and finite.
const STILL: f32 = 3600.0;

/// [`Instr::SeqRead`]: `info` of the instance of `table` with id `id`, among
/// `slots`. `at` is the sample of the segment being run, if one is: a
/// segment moves its sequences on before its tick runs, so they kept their
/// position and tempo for each sample in [`Slot::trail`].
fn seq_read(table: &SeqTable, slots: &[Slot], at: Option<usize>, info: SeqInfo, id: f32) -> f32 {
    let lasts = |beats: f64, tempo: f32| {
        if tempo > 0.0 {
            (beats / f64::from(tempo)) as f32
        } else {
            STILL
        }
    };
    let id = id as i32;
    let slot = (id != 0)
        .then(|| slots.iter().find(|s| s.active && s.id == id))
        .flatten();
    let Some(slot) = slot else {
        // Not playing: the sequence's own tempo, so a release tail does not
        // jump.
        let tempo = table.settings[TEMPO];
        return match info {
            SeqInfo::Tempo => tempo,
            SeqInfo::StepTime => lasts(table.step_beats, tempo),
            SeqInfo::BeatTime => lasts(1.0, tempo),
            SeqInfo::Step | SeqInfo::Pass | SeqInfo::Progress | SeqInfo::Playing => 0.0,
        };
    };
    let (pos, tempo) = match at {
        Some(j) if !slot.trail.is_empty() => slot.trail[j],
        _ => (slot.pos, slot.values[TEMPO]),
    };
    match info {
        SeqInfo::Tempo => tempo,
        SeqInfo::StepTime => lasts(table.step_beats, tempo),
        SeqInfo::BeatTime => lasts(1.0, tempo),
        SeqInfo::Step => (slot.current + 1) as f32,
        SeqInfo::Pass => slot.pass as f32,
        SeqInfo::Progress => {
            let into = pos - slot.current as f64 * table.step_beats;
            ((into / table.step_beats) as f32).clamp(0.0, 1.0 - f32::EPSILON / 2.0)
        }
        SeqInfo::Playing => 1.0,
    }
}

/// Where a playing sequence takes a setting from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Source {
    /// The sequence's own default.
    #[default]
    Default,
    /// Read once, when invoked.
    Now(Operand),
    /// A register, read every tick while playing.
    Follow(Reg),
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
    pub dst: Reg,
    /// In [`TEMPO`]... [`RELEASE`] order.
    pub settings: [Source; SETTINGS],
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
            Instr::EventArg { index, value } => format!("event arg {index} = {}", op(value)),
            Instr::InvokeEvent { event } => format!("invoke event {}", event.0),
            Instr::Halt { seq, id } => {
                format!("halt seq {seq} {}", id.as_ref().map_or("all".into(), op))
            }
            Instr::LoadCapture {
                dst,
                seq,
                slot,
                index,
            } => format!("r{dst} = capture {index} of seq {seq} slot {slot}"),
            Instr::SeqRead { dst, seq, info, id } => {
                format!("r{dst} = {info:?} of seq {seq} instance {}", op(id))
            }
            Instr::AudioRead {
                dst,
                audio,
                channel,
                at,
                part,
            } => format!(
                "r{dst} = audio {audio} channel {channel} at {} + {}",
                op(at),
                op(part)
            ),
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
            Instr::Noise { dst, source } => format!("r{dst} = noise {source}"),
            Instr::DelayRead {
                dst, line, time, ..
            } => format!("r{dst} = delay {line} read {}", op(time)),
            Instr::Want { slot, x, ceiling } => format!("want {slot} {} past {ceiling}", op(x)),
            Instr::DelayWrite { line, x } => format!("delay {line} write {}", op(x)),
            Instr::Sum {
                dst,
                init,
                base,
                n,
                stride,
            } => format!("r{dst} = {} + sum r{base}, {n} by {stride}", op(init)),
        };
        let _ = writeln!(s, "{pc:4}: {line}");
    }
}

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
    values: [f32; SETTINGS],
    sources: [Source; SETTINGS],
    captures: Vec<f32>,
    /// Notes sounding.
    held: Vec<Held>,
    /// The pass it is on, from 1.
    pass: u32,
    /// The step it is on (the last one started), from 0.
    current: usize,
    /// The next beat to mark, counted from the start of the pass.
    beat: u32,
    /// What has been sent for the bar, beat or step about to happen.
    bar_sent: bool,
    step_sent: bool,
    rest_sent: bool,
    /// `finished` has been sent; `end` comes next.
    finishing: bool,
    /// The `note_change` that follows the `note_on` just sent.
    change: Option<Payload>,
    /// The notes carried into the current step are settled: tied, slid or
    /// ended.
    settled: bool,
    /// Which of the current step's pitches carried notes took (by place in
    /// the chord), so they do not start again.
    taken: u64,
    /// Notes went on through the current step (so it is not a rest).
    went_on: bool,
    /// Its position and tempo at each sample of the segment being run, as
    /// its tick sees them; empty unless something reads them (see
    /// [`seq_read`]).
    trail: Box<[(f64, f32)]>,
}

/// A note a sequence instance is sounding.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Held {
    pitch: f32,
    /// The position its note-off is due: never, while it goes on into the
    /// next step (see [`Held::carried`]).
    off: f64,
    /// The release its note-off has.
    release: f32,
    pressure: f32,
    timbre: f32,
    /// Where it is in its step's chord, for pairing with the next step's.
    index: usize,
    /// It goes on into the next step, which ties, slides or ends it.
    carried: bool,
    /// A `note_change` is due, from this pitch.
    changed_from: Option<f32>,
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
    /// Something a sequence did: every declaration that accepts it.
    Seq(Sender, Payload),
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
    /// Per sequence, a bit per [`EventKind`] that some handler hears from
    /// it; the others are not delivered at all.
    wanted: Vec<u16>,
    /// Per noise generator, its state.
    noise: Vec<u64>,
    /// Per delay line, its samples and where the next write goes.
    delays: Vec<(Box<[f32]>, usize)>,
    /// Per ceiling, the most a value has asked of it, as `f32` bits; read
    /// by [`crate::live::Live`] on another thread.
    wants: std::sync::Arc<[std::sync::atomic::AtomicU32]>,
    /// The values of the next event a handler sends (see
    /// [`Instr::EventArg`]).
    event_args: [f32; crate::event::MAX_FIELDS],
    /// The sample of the segment whose tick is running, if one is (see
    /// [`seq_read`]).
    at: Option<usize>,
}

impl Run {
    /// Queue `payload` from sequence `seq`, if anything handles it.
    fn send(&mut self, seq: usize, payload: Payload) {
        if self.wanted[seq] & (1 << payload.kind() as u16) != 0 {
            self.queue
                .push_back(Pending::Seq(Sender::Seq(seq as u16), payload));
        }
    }
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
    /// How the tick runs over segments, unless it runs sample by sample.
    streams: Option<Streams>,
}

/// Most samples a segment runs at once: the length of every buffer.
const SEGMENT: usize = 64;

/// The tick in stages, run over a segment of samples at a time (see
/// [`super::stages`]).
///
/// A register that crosses stages, or that a streaming stage writes, has a
/// buffer of [`SEGMENT`] values after the registers, in [`Run::regs`]. A
/// streaming stage is a vector block whose lanes are the segment's samples.
/// A looping stage runs its instructions sample by sample on registers,
/// taking the buffered values it reads from their buffers first and
/// putting what it writes into them after.
struct Streams {
    stages: Vec<StageRun>,
    /// Every buffered register, with where its buffer starts.
    buffered: Vec<(Reg, u32)>,
    /// Node inputs the tick reads, by input, with their buffers.
    inputs: Vec<(usize, u32)>,
    /// Each output channel, and each pool copy's outputs (for voices
    /// falling silent).
    outputs: Vec<Out>,
    pools: Vec<Vec<Vec<Out>>>,
}

enum StageRun {
    Stream(VectorBlock),
    Loop {
        fast: Vec<Fast>,
        instrs: Vec<Instr>,
        /// Registers taken from their buffers before each sample, and put
        /// into them after.
        loads: Vec<(Reg, u32)>,
        stores: Vec<(Reg, u32)>,
    },
}

/// Where an output's values are over a segment.
#[derive(Clone, Copy)]
enum Out {
    Buffer(u32),
    /// The same all through it.
    Value(Operand),
}

impl Streams {
    /// How `code`'s tick runs in stages, if it can (and gains from it), and
    /// the registers it needs in all.
    fn new(code: &Code, claims: bool) -> Option<(Streams, usize)> {
        use std::collections::{HashMap, HashSet};
        let stages = &code.stages;
        if stages.fallback.is_some() || !stages.stages.iter().any(|s| s.streams) {
            return None;
        }
        let list = &code.instrs;
        let inputs: HashSet<Reg> = code.input_regs.iter().copied().collect();
        // What each stage reads and writes.
        let mut reads: Vec<HashSet<Reg>> = Vec::new();
        let mut writes: Vec<HashSet<Reg>> = Vec::new();
        let mut writers: HashMap<Reg, Vec<usize>> = HashMap::new();
        for (k, stage) in stages.stages.iter().enumerate() {
            let (mut r, mut w) = (HashSet::new(), HashSet::new());
            for &p in &stage.instrs {
                let (rs, ws) = super::stages::touches(code, &list[p as usize]);
                r.extend(rs);
                w.extend(ws);
            }
            for &x in &w {
                writers.entry(x).or_default().push(k);
            }
            reads.push(r);
            writes.push(w);
        }
        // Inputs are set before the tick; one written in it would be both.
        if inputs.iter().any(|r| writers.contains_key(r)) {
            return None;
        }
        let mut buffer: HashSet<Reg> = HashSet::new();
        for (k, stage) in stages.stages.iter().enumerate() {
            if stage.streams {
                buffer.extend(&writes[k]);
            }
            for &r in &reads[k] {
                let elsewhere = writers.get(&r).is_some_and(|ws| ws.iter().any(|&w| w != k));
                if elsewhere || inputs.contains(&r) {
                    buffer.insert(r);
                }
            }
        }
        let seen = |o: &Operand| match *o {
            Operand::Reg(r) if writers.contains_key(&r) || inputs.contains(&r) => Some(r),
            _ => None,
        };
        // What a pool's copies output only matters for knowing when a
        // claimed voice falls silent.
        let pools: &[Vec<Vec<Operand>>] = if claims { &code.pools } else { &[] };
        buffer.extend(
            code.output
                .iter()
                .chain(pools.iter().flatten().flatten())
                .filter_map(seen),
        );
        let mut buffered: Vec<Reg> = buffer.into_iter().collect();
        buffered.sort_unstable();
        let at: HashMap<Reg, u32> = buffered
            .iter()
            .enumerate()
            .map(|(k, &r)| (r, (code.regs + k * SEGMENT) as u32))
            .collect();
        let total = code.regs + buffered.len() * SEGMENT;
        let arg = |o: Operand| match o {
            Operand::Const(c) => VArg::Const(c),
            Operand::Reg(r) => at.get(&r).map_or(VArg::Scalar(r), |&b| VArg::Lanes(b)),
        };
        let mut runs = Vec::new();
        for (k, stage) in stages.stages.iter().enumerate() {
            if stage.streams {
                let mut instrs = Vec::new();
                for &p in &stage.instrs {
                    let i = list[p as usize];
                    let mut d = 0;
                    super::opt::writes(&i, &mut |r| d = at[&r]);
                    let v = match i {
                        Instr::Op2 { op, a, b, .. } => VInstr::Op2 {
                            op,
                            d,
                            a: arg(a),
                            b: arg(b),
                        },
                        Instr::Op1 { op, x, .. } => VInstr::Op1 { op, d, x: arg(x) },
                        Instr::Copy { src, .. } => VInstr::Copy { d, s: arg(src) },
                        Instr::Select { cond, a, b, .. } => VInstr::Select {
                            d,
                            cond: arg(cond),
                            a: arg(a),
                            b: arg(b),
                        },
                        Instr::Tune {
                            tuning,
                            pitch,
                            setting,
                            a4,
                            ..
                        } => VInstr::Tune {
                            tuning,
                            d,
                            pitch: arg(pitch),
                            setting: arg(setting),
                            a4: arg(a4),
                        },
                        Instr::Sum {
                            dst,
                            init,
                            base,
                            n,
                            stride,
                        } => {
                            // One add at a time, in the same order.
                            let parts: Vec<Reg> = (0..n).map(|j| base + j * stride).collect();
                            if parts.contains(&dst) {
                                return None;
                            }
                            let mut acc = arg(init);
                            for r in parts {
                                instrs.push(VInstr::Op2 {
                                    op: Op2::Add,
                                    d,
                                    a: acc,
                                    b: arg(Operand::Reg(r)),
                                });
                                acc = VArg::Lanes(d);
                            }
                            VInstr::Copy { d, s: acc }
                        }
                        _ => return None,
                    };
                    instrs.push(v);
                }
                let block = VectorBlock {
                    lanes: SEGMENT as u16,
                    instrs,
                };
                check_vector(&block, total, 0, 0, 0);
                runs.push(StageRun::Stream(block));
            } else {
                // Its own code, with jumps moved to match.
                let at_new = |t: u32| stage.instrs.partition_point(|&p| p < t) as u32;
                let instrs: Vec<Instr> = stage
                    .instrs
                    .iter()
                    .map(|&p| {
                        let mut i = list[p as usize];
                        if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = &mut i {
                            *target = at_new(*target);
                        }
                        i
                    })
                    .collect();
                let pick = |set: &HashSet<Reg>, keep: &dyn Fn(Reg) -> bool| {
                    let mut v: Vec<(Reg, u32)> = set
                        .iter()
                        .filter(|&&r| keep(r))
                        .filter_map(|r| Some((*r, *at.get(r)?)))
                        .collect();
                    v.sort_unstable();
                    v
                };
                let loads = pick(&reads[k], &|r| {
                    inputs.contains(&r)
                        || writers.get(&r).is_some_and(|ws| ws.iter().any(|&w| w != k))
                });
                let stores = pick(&writes[k], &|_| true);
                runs.push(StageRun::Loop {
                    fast: lower(&instrs, code.regs),
                    instrs,
                    loads,
                    stores,
                });
            }
        }
        let out = |o: &Operand| match *o {
            Operand::Reg(r) => at.get(&r).map_or(Out::Value(*o), |&b| Out::Buffer(b)),
            Operand::Const(_) => Out::Value(*o),
        };
        let streams = Streams {
            stages: runs,
            buffered: buffered.iter().map(|r| (*r, at[r])).collect(),
            inputs: code
                .input_regs
                .iter()
                .enumerate()
                .filter_map(|(k, r)| Some((k, *at.get(r)?)))
                .collect(),
            outputs: code.output.iter().map(out).collect(),
            pools: pools
                .iter()
                .map(|p| p.iter().map(|c| c.iter().map(out).collect()).collect())
                .collect(),
        };
        Some((streams, total))
    }
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
        let check = |r: Reg| assert!((r as usize) < code.regs, "register r{r} out of range");
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
            check_vector(
                block,
                code.regs,
                code.noises.len(),
                code.delays.len(),
                code.ceilings.len(),
            );
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
        // Whether the tick reads where instances of sequence `seq` are.
        let reads = |seq: usize| {
            let seq = seq as u16;
            code.instrs
                .iter()
                .any(|i| matches!(*i, Instr::SeqRead { seq: s, .. } if s == seq))
                || code.vectors.iter().flat_map(|v| &v.instrs).any(
                    |i| matches!(*i, VInstr::SeqRead { seq: s, .. } if s == seq),
                )
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
                        trail: vec![(0.0, 0.0); if reads(j) { SEGMENT } else { 0 }].into(),
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
        // Every handler can queue a few events, and stopping every instance
        // of a sequence at once queues their note-offs, `halted` and `end`;
        // this is a generous bound, so the queue does not grow while playing.
        let stops: usize = code
            .seqs
            .iter()
            .map(|t| {
                let chord = t.steps.iter().map(|s| s.pitches.len()).max().unwrap_or(0);
                usize::from(t.instances) * (2 * chord + 4)
            })
            .sum();
        let queue = 64 + 8 * code.events.len() + 4 * code.seqs.len() + stops;
        let wanted = (0..code.seqs.len())
            .map(|j| {
                let from = Sender::Seq(j as u16);
                let mut bits = 0u16;
                for (i, d) in code.decls.iter().enumerate() {
                    let handled = code
                        .events
                        .iter()
                        .any(|h| h.handles == Handles::Event(EventId(i as u16)));
                    for kind in EventKind::SEQ {
                        if handled && d.accepts(from, 0, kind) {
                            bits |= 1 << kind as u16;
                        }
                    }
                }
                bits
            })
            .collect();
        let (streams, regs) = match Streams::new(&code, claims) {
            Some((s, total)) => (Some(s), total),
            None => (None, code.regs),
        };
        let mut p = Program {
            run: Run {
                regs: vec![0.0; regs].into_boxed_slice(),
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
                wanted,
                noise: code.noises.clone(),
                event_args: [0.0; crate::event::MAX_FIELDS],
                at: None,
                wants: (0..code.ceilings.len())
                    .map(|_| std::sync::atomic::AtomicU32::new(0))
                    .collect(),
                delays: code
                    .delays
                    .iter()
                    .map(|&n| (vec![0.0; n.max(3) as usize].into_boxed_slice(), 0))
                    .collect(),
            },
            code,
            tick,
            post,
            handlers,
            claims,
            streams,
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
                Pending::Seq(from, payload) => {
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
                (Mode::Held, Some((pool, copy)), Some(payload)) => {
                    self.change(pool, from, &payload) == Some(copy)
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

    /// The copy of `pool` holding the note `payload` changes, which then
    /// holds it at its new pitch. Of two holding the same note, the one
    /// that took it last: the one a `note_on` just claimed.
    fn change(&mut self, pool: u16, from: Sender, payload: &Payload) -> Option<u16> {
        let p = usize::from(pool);
        let (stamp, pick) = self.run.picks[p];
        if stamp == self.run.delivery {
            return pick;
        }
        let (held, instance) = payload.note()?;
        let Payload::NoteChange { pitch, .. } = *payload else {
            return None;
        };
        let voices = &mut self.run.voices[p];
        let copy = voices
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v.state == VoiceState::Holding
                    && v.from == from
                    && v.pitch == held
                    && v.instance == instance
            })
            .max_by_key(|(_, v)| v.since)
            .map(|(i, _)| i);
        if let Some(c) = copy {
            voices[c].pitch = pitch;
        }
        let pick = copy.map(|c| c as u16);
        self.run.picks[p] = (self.run.delivery, pick);
        pick
    }

    /// Everything every playing sequence does now. Handlers can start
    /// instances, which then make their first step in the same sample, so
    /// this goes round again until nothing more happens (a few times at
    /// most; the checker rejects anything that could go on forever).
    fn fire_sequences(&mut self) {
        for _ in 0..16 {
            let mut any = false;
            for j in 0..self.code.seqs.len() {
                for k in 0..self.run.slots[j].len() {
                    while let Some(p) = next_event(&self.code.seqs[j], &mut self.run.slots[j][k]) {
                        any = true;
                        self.run.send(j, p);
                        self.deliver_queued();
                    }
                }
            }
            if !any {
                break;
            }
        }
    }

    /// Run what is queued, unless a delivery is already running.
    fn deliver_queued(&mut self) {
        if let Some(p) = self.run.queue.pop_front() {
            self.deliver(p);
        }
    }

    /// Move every playing sequence on by one tick.
    fn advance_sequences(&mut self) {
        let rate = f64::from(self.run.rate);
        for slots in &mut self.run.slots {
            for slot in slots.iter_mut().filter(|s| s.active) {
                for s in 0..SETTINGS {
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
                let quiet = outputs.iter().all(|&o| val(regs, o).abs() < SILENCE);
                note_quiet(&mut self.run.voices[p][c], quiet);
            }
        }
    }

    /// The tick over `ctx.frames` samples, in segments: each from where a
    /// sequence does something to the next, [`SEGMENT`] samples at most.
    /// Handlers only run where a segment starts, so every stage sees what
    /// it would sample by sample.
    fn process_segments(&mut self, ctx: &Context, inputs: &Inputs, out: &mut Outputs) {
        let sequences = !self.code.seqs.is_empty();
        let mut i = 0;
        while i < ctx.frames {
            if sequences {
                self.fire_sequences();
            }
            let mut n = 0;
            loop {
                // Sequences move on by what the inputs say; what the tick
                // computes they cannot follow here (see `stages::fallback`).
                if sequences {
                    self.advance_sequences_at(inputs, i, n);
                }
                n += 1;
                if n == SEGMENT || i + n == ctx.frames || (sequences && self.sequences_due()) {
                    break;
                }
            }
            self.run_segment(inputs, out, i, n);
            i += n;
        }
    }

    /// [`Program::advance_sequences`] for sample `j` of the segment
    /// starting at sample `i`, with the values it follows taken from the
    /// inputs. Where the tick reads them, each instance first notes where
    /// it is, which is what the tick sees at that sample.
    fn advance_sequences_at(&mut self, inputs: &Inputs, i: usize, j: usize) {
        let rate = f64::from(self.run.rate);
        let (code, run) = (&self.code, &mut self.run);
        let i = i + j;
        for slots in &mut run.slots {
            for slot in slots.iter_mut().filter(|s| s.active) {
                if let Some(t) = slot.trail.get_mut(j) {
                    *t = (slot.pos, slot.values[TEMPO]);
                }
                for s in 0..SETTINGS {
                    if let Source::Follow(r) = slot.sources[s] {
                        slot.values[s] = match code.input_regs.iter().position(|&x| x == r) {
                            Some(k) => inputs.get(k).at(i),
                            None => run.regs[r as usize],
                        };
                    }
                }
                slot.pos += f64::from(slot.values[TEMPO].max(0.0)) / rate;
            }
        }
    }

    /// Whether any playing sequence could do something now. Once
    /// [`Program::fire_sequences`] has run, an instance does nothing more
    /// until its position reaches a note-off, the next beat or step, or
    /// the end of its pass (see [`next_event`]).
    fn sequences_due(&self) -> bool {
        for (table, slots) in self.code.seqs.iter().zip(&self.run.slots) {
            let len = table.steps.len();
            let total = len as f64 * table.step_beats;
            for slot in slots.iter().filter(|s| s.active) {
                let pos = slot.pos + EPS;
                let beat_at = f64::from(slot.beat);
                let beats_left = beat_at < total - EPS;
                let steps_left = slot.next < len;
                if slot.held.iter().any(|h| h.off <= pos)
                    || (beats_left && beat_at <= pos)
                    || (steps_left && slot.next as f64 * table.step_beats <= pos)
                    || (!beats_left && !steps_left && total <= pos)
                {
                    return true;
                }
            }
        }
        false
    }

    /// Run the tick's stages over samples `i..i + n` of this block.
    fn run_segment(&mut self, inputs: &Inputs, out: &mut Outputs, i: usize, n: usize) {
        let Program {
            code,
            run,
            streams,
            claims,
            ..
        } = self;
        let streams = streams.as_ref().expect("runs in stages");
        for &(k, b) in &streams.inputs {
            let dst = &mut run.regs[b as usize..b as usize + n];
            match inputs.get(k) {
                Signal::Const(v) => dst.fill(v),
                Signal::Buffer(src) => dst.copy_from_slice(&src[i..i + n]),
            }
        }
        for stage in &streams.stages {
            match stage {
                StageRun::Stream(block) => {
                    let state = LaneState {
                        noise: &mut run.noise,
                        delays: &mut run.delays,
                        wants: &run.wants,
                        rate: run.rate,
                        seqs: &code.seqs,
                        slots: &run.slots,
                        at: None,
                        audios: &code.audios,
                    };
                    run_lanes(block, n, &mut run.regs, state);
                }
                StageRun::Loop {
                    fast,
                    instrs,
                    loads,
                    stores,
                } => {
                    for j in 0..n {
                        for &(r, b) in loads {
                            run.regs[r as usize] = run.regs[b as usize + j];
                        }
                        run.at = Some(j);
                        exec(fast, instrs, code, run);
                        for &(r, b) in stores {
                            run.regs[b as usize + j] = run.regs[r as usize];
                        }
                    }
                    run.at = None;
                }
            }
        }
        let regs = &run.regs;
        let at = |o: Out, j: usize| match o {
            Out::Buffer(b) => regs[b as usize + j],
            Out::Value(o) => val(regs, o),
        };
        for (c, &o) in streams.outputs.iter().enumerate() {
            for j in 0..n {
                out.set(c, i + j, at(o, j));
            }
        }
        if *claims {
            for j in 0..n {
                for (p, copies) in streams.pools.iter().enumerate() {
                    for (c, outputs) in copies.iter().enumerate() {
                        let quiet = outputs.iter().all(|&o| at(o, j).abs() < SILENCE);
                        note_quiet(&mut run.voices[p][c], quiet);
                    }
                }
            }
        }
        // Each register as the segment's last sample left it, for handlers
        // and the next segment.
        for &(r, b) in &streams.buffered {
            run.regs[r as usize] = run.regs[b as usize + n - 1];
        }
        run.now += n as u64;
    }
}

/// Count a releasing voice's silent samples; it is free after its tail.
fn note_quiet(v: &mut Voice, quiet: bool) {
    if v.state != VoiceState::Releasing {
        return;
    }
    v.silent = if quiet { v.silent + 1 } else { 0 };
    if v.silent >= v.tail {
        v.state = VoiceState::Free;
    }
}

fn val(regs: &[f32], o: Operand) -> f32 {
    match o {
        Operand::Reg(r) => regs[r as usize],
        Operand::Const(c) => c,
    }
}

/// The next thing `slot` (an instance of a sequence with `table`) does
/// now, moving it on; `None` when nothing more is due this tick. At the same
/// moment, in order: note-offs; finishing (`finished`, `end`) or going round
/// (`repeated`); `bar`; `beat`; `step`; `rest`; slides and changes of held
/// notes; note-ons, each followed by its `note_change`.
fn next_event(table: &SeqTable, slot: &mut Slot) -> Option<Payload> {
    let sb = table.step_beats;
    let len = table.steps.len();
    let total = len as f64 * sb;
    loop {
        if !slot.active {
            return None;
        }
        // A note's starting values come right after it starts.
        if let Some(change) = slot.change.take() {
            return Some(change);
        }
        let (id, pos, pass) = (slot.id, slot.pos, slot.pass as i32);
        let seq = |kind, values| Some(Payload::Seq { kind, values });
        if let Some(h) = slot.held.iter().position(|h| h.off <= pos + EPS) {
            let h = slot.held.swap_remove(h);
            return Some(Payload::NoteOff {
                pitch: h.pitch,
                release: h.release,
                instance: id,
            });
        }
        let beat_at = f64::from(slot.beat);
        let beats_left = beat_at < total - EPS;
        let step_at = slot.next as f64 * sb;
        let steps_left = slot.next < len;
        if !beats_left && !steps_left {
            // The pass is over once its last step is.
            if pos + EPS < total {
                return None;
            }
            if slot.looping || slot.repeats > 1 {
                if !slot.looping {
                    slot.repeats -= 1;
                }
                slot.pass += 1;
                slot.pos -= total;
                for h in &mut slot.held {
                    h.off -= total;
                }
                slot.next = 0;
                slot.chord = 0;
                slot.beat = 0;
                return seq(EventKind::Repeated, [id, slot.pass as i32, 0, 0]);
            }
            if let Some(h) = slot.held.pop() {
                return Some(Payload::NoteOff {
                    pitch: h.pitch,
                    release: h.release,
                    instance: id,
                });
            }
            let step = slot.current as i32 + 1;
            if !slot.finishing {
                slot.finishing = true;
                return seq(EventKind::Finished, [id, step, 0, 0]);
            }
            slot.active = false;
            return seq(EventKind::End, [id, step, 0, 0]);
        }
        // The notes carried into a step that is due: tied or slid to the
        // pitch in the same place of its chord (or all going on, for `~`),
        // or ended now. Their note-offs come before anything else.
        if steps_left && step_at <= pos + EPS && !slot.settled {
            slot.settled = true;
            let step = &table.steps[slot.next];
            let (carries, end) = step_end(table, slot, slot.next);
            for h in slot.held.iter_mut().filter(|h| h.carried) {
                let to = if step.held {
                    Some(h.pitch)
                } else {
                    step.pitches.get(h.index).copied()
                };
                let Some(to) = to else {
                    h.carried = false;
                    h.off = step_at;
                    continue;
                };
                if !step.held && h.index < 64 {
                    slot.taken |= 1 << h.index;
                }
                slot.went_on = true;
                let pressure = step.values[PRESSURE - VELOCITY].unwrap_or(h.pressure);
                let timbre = step.values[TIMBRE - VELOCITY].unwrap_or(h.timbre);
                if to != h.pitch || pressure != h.pressure || timbre != h.timbre {
                    h.changed_from = Some(h.pitch);
                }
                h.pitch = to;
                h.pressure = pressure;
                h.timbre = timbre;
                h.release = step.values[RELEASE - VELOCITY].unwrap_or(h.release);
                h.carried = carries;
                h.off = end;
            }
            continue;
        }
        // A beat comes before a step starting at the same moment.
        if beats_left && (!steps_left || beat_at <= step_at + EPS) {
            if beat_at > pos + EPS {
                return None;
            }
            let per_bar = table.beats_per_bar.max(1);
            let (bar, beat) = (
                (slot.beat / per_bar + 1) as i32,
                (slot.beat % per_bar + 1) as i32,
            );
            if beat == 1 && !slot.bar_sent {
                slot.bar_sent = true;
                return seq(EventKind::Bar, [id, bar, pass, 0]);
            }
            slot.bar_sent = false;
            slot.beat += 1;
            return seq(EventKind::Beat, [id, beat, bar, pass]);
        }
        if step_at > pos + EPS {
            return None;
        }
        let step = &table.steps[slot.next];
        let number = slot.next as i32 + 1;
        let (carries, end) = step_end(table, slot, slot.next);
        if !slot.step_sent {
            slot.step_sent = true;
            slot.current = slot.next;
            return seq(EventKind::Step, [id, number, pass, 0]);
        }
        if step.pitches.is_empty() && !slot.went_on && !slot.rest_sent {
            slot.rest_sent = true;
            return seq(EventKind::Rest, [id, number, pass, 0]);
        }
        // Slides and changes of held notes, before new notes start.
        if let Some(h) = slot.held.iter_mut().find(|h| h.changed_from.is_some()) {
            let from = h.changed_from.take().expect("found");
            return Some(Payload::NoteChange {
                pitch: h.pitch,
                pressure: h.pressure,
                timbre: h.timbre,
                slide: true,
                instance: id,
                from,
            });
        }
        // Pitches carried notes took do not start again.
        while slot.chord < step.pitches.len()
            && slot.chord < 64
            && slot.taken & (1 << slot.chord) != 0
        {
            slot.chord += 1;
        }
        if let Some(&pitch) = step.pitches.get(slot.chord) {
            let index = slot.chord;
            slot.chord += 1;
            // The step's own values, else the instance's.
            let value = |k: usize| step.values[k - VELOCITY].unwrap_or(slot.values[k]);
            slot.held.push(Held {
                pitch,
                off: end,
                release: value(RELEASE),
                pressure: value(PRESSURE),
                timbre: value(TIMBRE),
                index,
                carried: carries,
                changed_from: None,
            });
            slot.change = Some(Payload::NoteChange {
                pitch,
                pressure: value(PRESSURE),
                timbre: value(TIMBRE),
                slide: false,
                instance: id,
                from: pitch,
            });
            return Some(Payload::NoteOn {
                pitch,
                velocity: value(VELOCITY),
                instance: id,
            });
        }
        slot.next += 1;
        slot.chord = 0;
        slot.step_sent = false;
        slot.rest_sent = false;
        slot.settled = false;
        slot.taken = 0;
        slot.went_on = false;
    }
}

/// Whether the notes of step `n` go on into the next step (it ends in `~`,
/// or the next is `~`), and so the position they end at: never, or the
/// step's gate.
fn step_end(table: &SeqTable, slot: &Slot, n: usize) -> (bool, f64) {
    let steps = &table.steps;
    let round = slot.looping || slot.repeats > 1;
    let next = match steps.get(n + 1) {
        Some(s) => Some(s),
        None if round => steps.first(),
        None => None,
    };
    let carries = next.is_some_and(|s| steps[n].hold || s.held);
    let gate = f64::from(slot.values[GATE].clamp(1e-6, 1.0));
    let end = if carries {
        f64::INFINITY
    } else {
        (n as f64 + gate) * table.step_beats
    };
    (carries, end)
}

/// Stop `slot` (an instance of sequence `seq`) for `reason` (`halted` or
/// `replaced`): a note-off for every note it holds, then `reason`, then
/// `end`.
fn stop(run: &mut Run, seq: usize, k: usize, reason: EventKind) {
    let slot = &mut run.slots[seq][k];
    let (id, step) = (slot.id, slot.current as i32 + 1);
    slot.active = false;
    let held = slot.held.len();
    for h in 0..held {
        let Held { pitch, release, .. } = run.slots[seq][k].held[h];
        run.send(
            seq,
            Payload::NoteOff {
                pitch,
                release,
                instance: id,
            },
        );
    }
    run.slots[seq][k].held.clear();
    run.send(
        seq,
        Payload::Seq {
            kind: reason,
            values: [id, step, 0, 0],
        },
    );
    run.send(
        seq,
        Payload::Seq {
            kind: EventKind::End,
            values: [id, step, 0, 0],
        },
    );
}

/// Run `fast` (lowered from `instrs`) to the end.
fn exec(fast: &[Fast], instrs: &[Instr], code: &Code, run: &mut Run) {
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
            Fast::Op1 { op, d, x } => set!(d, op.apply(r!(x))),
            Fast::JumpUnless { cond, target } => {
                if r!(cond) == 0.0 {
                    pc = target as usize;
                }
            }
            Fast::Jump { target } => pc = target as usize,
            Fast::Vector { block } => {
                let state = LaneState {
                    noise: &mut run.noise,
                    delays: &mut run.delays,
                    wants: &run.wants,
                    rate: run.rate,
                    seqs: &code.seqs,
                    slots: &run.slots,
                    at: run.at,
                    audios: &code.audios,
                };
                run_vector(&code.vectors[block as usize], regs, state);
            }
            Fast::Slow { index } => {
                if let Some(target) = step(&instrs[index as usize], code, run) {
                    pc = target;
                }
            }
        }
    }
}

/// Check that every register a vector block uses exists.
fn check_vector(block: &VectorBlock, regs: usize, noises: usize, delays: usize, wants: usize) {
    let n = usize::from(block.lanes);
    assert!(n > 0, "a vector block needs lanes");
    let arg = |a: VArg| match a {
        VArg::Lanes(b) => assert!((b as usize) + n <= regs, "lane group r{b} out of range"),
        VArg::Scalar(r) => assert!((r as usize) < regs, "register r{r} out of range"),
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
            VInstr::Noise { d, source, stride } => {
                assert!(stride > 0, "noise generators need a stride");
                assert!(
                    usize::from(source) + (n - 1) * usize::from(stride) < noises,
                    "noise generators {source}.. out of range"
                );
                d
            }
            VInstr::DelayRead {
                d,
                line,
                stride,
                time,
                want,
                want_stride,
            } => {
                arg(time);
                let last =
                    |first: u16, stride: u16| usize::from(first) + (n - 1) * usize::from(stride);
                assert!(
                    last(line, stride) < delays,
                    "delay lines {line}.. out of range"
                );
                assert!(
                    last(want, want_stride) < wants,
                    "ceilings {want}.. out of range"
                );
                d
            }
            VInstr::SeqRead { d, id, .. } => {
                arg(id);
                d
            }
            VInstr::AudioRead { d, at, part, .. } => {
                arg(at);
                arg(part);
                d
            }
            VInstr::DelayWrite { line, stride, x } => {
                arg(x);
                assert!(stride > 0, "lanes cannot write one delay line");
                assert!(
                    usize::from(line) + (n - 1) * usize::from(stride) < delays,
                    "delay lines {line}.. out of range"
                );
                continue;
            }
        };
        arg(VArg::Lanes(d));
    }
}

/// What a vector block's lanes keep besides registers: noise generators,
/// delay lines, and the ceilings they ask to grow.
struct LaneState<'a> {
    noise: &'a mut [u64],
    delays: &'a mut [(Box<[f32]>, usize)],
    wants: &'a [std::sync::atomic::AtomicU32],
    rate: f32,
    /// The sequences and their instances, for [`VInstr::SeqRead`], and the
    /// sample of the segment being run (see [`seq_read`]).
    seqs: &'a [SeqTable],
    slots: &'a [Vec<Slot>],
    at: Option<usize>,
    /// The audio files, for [`VInstr::AudioRead`].
    audios: &'a [std::sync::Arc<[Box<[f32]>]>],
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
///
/// The loops over lanes are small functions of their own, one per operation
/// and shape of operands, kept out of this one: each is compiled on its own
/// terms (SIMD, unrolling), however this function changes around them.
fn run_vector(block: &VectorBlock, regs: &mut [f32], state: LaneState) {
    run_lanes(block, usize::from(block.lanes), regs, state);
}

/// Run every instruction of `block` over its first `n` lanes (at most
/// `block.lanes`).
fn run_lanes(block: &VectorBlock, n: usize, regs: &mut [f32], state: LaneState) {
    debug_assert!(n <= usize::from(block.lanes));
    let p = regs.as_mut_ptr();
    // SAFETY: `check_vector` made sure every lane group and register is
    // inside `regs`. Lane groups never partly overlap: a write goes to the
    // same lane it reads, or to a different group.
    unsafe {
        let src = |a: VArg| match a {
            VArg::Lanes(b) => Src::Lanes(p.add(b as usize)),
            VArg::Scalar(r) => Src::Splat(*p.add(r as usize)),
            VArg::Const(c) => Src::Splat(c),
        };
        let truth = |b: bool| if b { 1.0f32 } else { 0.0 };
        for i in &block.instrs {
            match *i {
                VInstr::Op2 { op, d, a, b } => {
                    let (d, a, b) = (p.add(d as usize), src(a), src(b));
                    match op {
                        Op2::Add => lanes2(d, a, b, n, |x, y| x + y),
                        Op2::Sub => lanes2(d, a, b, n, |x, y| x - y),
                        Op2::Mul => lanes2(d, a, b, n, |x, y| x * y),
                        Op2::Div => lanes2(d, a, b, n, |x, y| x / y),
                        Op2::Min => lanes2(d, a, b, n, f32::min),
                        Op2::Max => lanes2(d, a, b, n, f32::max),
                        Op2::Lt => lanes2(d, a, b, n, |x, y| truth(x < y)),
                        Op2::Le => lanes2(d, a, b, n, |x, y| truth(x <= y)),
                        Op2::Gt => lanes2(d, a, b, n, |x, y| truth(x > y)),
                        Op2::Ge => lanes2(d, a, b, n, |x, y| truth(x >= y)),
                        Op2::Eq => lanes2(d, a, b, n, |x, y| truth(x == y)),
                        Op2::Ne => lanes2(d, a, b, n, |x, y| truth(x != y)),
                        Op2::And => lanes2(d, a, b, n, |x, y| truth(x != 0.0 && y != 0.0)),
                        Op2::Or => lanes2(d, a, b, n, |x, y| truth(x != 0.0 || y != 0.0)),
                        op => lanes2(d, a, b, n, |x, y| op.apply(x, y)),
                    }
                }
                VInstr::Op1 { op, d, x } => {
                    let (d, x) = (p.add(d as usize), src(x));
                    match op {
                        Op1::Wrap => lanes1(d, x, n, |v| v - crate::ops::floor(v)),
                        Op1::Floor => lanes1(d, x, n, crate::ops::floor),
                        Op1::Neg => lanes1(d, x, n, |v| -v),
                        Op1::Abs => lanes1(d, x, n, f32::abs),
                        Op1::Not => lanes1(d, x, n, |v| truth(v == 0.0)),
                        op => lanes1(d, x, n, |v| op.apply(v)),
                    }
                }
                VInstr::Copy { d, s } => lanes1(p.add(d as usize), src(s), n, |v| v),
                VInstr::Select { d, cond, a, b } => {
                    select(p.add(d as usize), src(cond), src(a), src(b), n);
                }
                VInstr::Tune {
                    tuning,
                    d,
                    pitch,
                    setting,
                    a4,
                } => {
                    let dp = p.add(d as usize);
                    let (pi, se, a) = (src(pitch), src(setting), src(a4));
                    for k in 0..n {
                        *dp.add(k) = tuning.frequency(pi.at(k), se.at(k), a.at(k));
                    }
                }
                VInstr::Noise { d, source, stride } => {
                    let states = &mut state.noise[usize::from(source)..];
                    noise_lanes(p.add(d as usize), states, usize::from(stride), n);
                }
                VInstr::DelayRead {
                    d,
                    line,
                    stride,
                    time,
                    want,
                    want_stride,
                } => delay_reads(
                    p.add(d as usize),
                    src(time),
                    &state.delays[usize::from(line)..],
                    usize::from(stride),
                    &state.wants[usize::from(want)..],
                    usize::from(want_stride),
                    state.rate,
                    n,
                ),
                VInstr::AudioRead {
                    d,
                    audio,
                    channel,
                    at,
                    part,
                } => {
                    let (dp, at, part) = (p.add(d as usize), src(at), src(part));
                    let data = &state.audios[usize::from(audio)][usize::from(channel)];
                    for k in 0..n {
                        *dp.add(k) = read_audio(data, at.at(k), part.at(k));
                    }
                }
                VInstr::SeqRead { d, seq, info, id } => {
                    let (dp, id) = (p.add(d as usize), src(id));
                    let seq = usize::from(seq);
                    let (table, slots) = (&state.seqs[seq], &state.slots[seq]);
                    for k in 0..n {
                        *dp.add(k) = seq_read(table, slots, state.at, info, id.at(k));
                    }
                }
                VInstr::DelayWrite { line, stride, x } => delay_writes(
                    src(x),
                    &mut state.delays[usize::from(line)..],
                    usize::from(stride),
                    n,
                ),
            }
        }
    }
}

impl Src {
    /// Lane `k`'s value.
    ///
    /// # Safety
    ///
    /// For `Lanes`, lane `k` must be inside its group.
    #[inline(always)]
    unsafe fn at(self, k: usize) -> f32 {
        match self {
            // SAFETY: as the caller promises.
            Src::Lanes(q) => unsafe { *q.add(k) },
            Src::Splat(v) => v,
        }
    }
}

/// `d = f(x)` over `n` lanes. Each shape of operand has a loop of its own,
/// with one operation in its body, so the compiler can turn it into SIMD.
///
/// # Safety
///
/// `d` and a `Lanes` operand must hold `n` lanes, and may only be the same
/// group or not overlap at all.
#[inline(never)]
unsafe fn lanes1(d: *mut f32, x: Src, n: usize, f: impl Fn(f32) -> f32) {
    // SAFETY: as the caller promises.
    unsafe {
        match x {
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
    }
}

/// `d = f(a, b)` over `n` lanes; see [`lanes1`].
///
/// # Safety
///
/// As for [`lanes1`].
#[inline(never)]
unsafe fn lanes2(d: *mut f32, a: Src, b: Src, n: usize, f: impl Fn(f32, f32) -> f32) {
    // SAFETY: as the caller promises.
    unsafe {
        match (a, b) {
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
    }
}

/// Lane `k` of `d` = the next number from generator `states[k * stride]`,
/// for `n` lanes.
///
/// # Safety
///
/// `d` must hold `n` lanes.
#[inline(never)]
unsafe fn noise_lanes(d: *mut f32, states: &mut [u64], stride: usize, n: usize) {
    for (k, state) in states.iter_mut().step_by(stride).take(n).enumerate() {
        // SAFETY: as the caller promises.
        unsafe { *d.add(k) = next_noise(state) };
    }
}

/// Lane `k` of `d` = what `lines[k * stride]` held `time` (lane `k`) ago,
/// as [`Instr::DelayRead`]: a time past the line is noted in
/// `wants[k * want_stride]`.
///
/// # Safety
///
/// `d` and a `Lanes` time must hold `n` lanes.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn delay_reads(
    d: *mut f32,
    time: Src,
    lines: &[(Box<[f32]>, usize)],
    stride: usize,
    wants: &[std::sync::atomic::AtomicU32],
    want_stride: usize,
    rate: f32,
    n: usize,
) {
    for k in 0..n {
        // SAFETY: as the caller promises.
        let t = unsafe { time.at(k) };
        let (data, at) = &lines[k * stride];
        if t > (data.len() - 2) as f32 {
            note_want(wants, (k * want_stride) as u16, t / rate);
        }
        // SAFETY: as the caller promises.
        unsafe { *d.add(k) = read_delay(data, *at, t) };
    }
}

/// Lane `k`'s value of `x` written to `lines[k * stride]`, as
/// [`Instr::DelayWrite`].
///
/// # Safety
///
/// A `Lanes` value must hold `n` lanes.
#[inline(never)]
unsafe fn delay_writes(x: Src, lines: &mut [(Box<[f32]>, usize)], stride: usize, n: usize) {
    for k in 0..n {
        let (data, at) = &mut lines[k * stride];
        // SAFETY: as the caller promises.
        data[*at] = unsafe { x.at(k) };
        *at = if *at + 1 == data.len() { 0 } else { *at + 1 };
    }
}

/// `d = if cond { a } else { b }` over `n` lanes; see [`lanes1`].
///
/// # Safety
///
/// As for [`lanes1`].
#[inline(never)]
unsafe fn select(d: *mut f32, cond: Src, a: Src, b: Src, n: usize) {
    // SAFETY: as the caller promises.
    unsafe {
        match (cond, a, b) {
            (Src::Lanes(c), Src::Lanes(a), Src::Lanes(b)) => {
                for k in 0..n {
                    let (x, y) = (*a.add(k), *b.add(k));
                    *d.add(k) = if *c.add(k) != 0.0 { x } else { y };
                }
            }
            (Src::Lanes(c), Src::Splat(x), Src::Lanes(b)) => {
                for k in 0..n {
                    let y = *b.add(k);
                    *d.add(k) = if *c.add(k) != 0.0 { x } else { y };
                }
            }
            (Src::Lanes(c), Src::Lanes(a), Src::Splat(y)) => {
                for k in 0..n {
                    let x = *a.add(k);
                    *d.add(k) = if *c.add(k) != 0.0 { x } else { y };
                }
            }
            (c, a, b) => {
                for k in 0..n {
                    *d.add(k) = if c.at(k) != 0.0 { a.at(k) } else { b.at(k) };
                }
            }
        }
    }
}

/// Note that ceiling `slot` was asked for `value`, if that is more than
/// before. Only the audio thread writes, so a load and a store will do.
fn note_want(wants: &[std::sync::atomic::AtomicU32], slot: u16, value: f32) {
    use std::sync::atomic::Ordering;
    if let Some(w) = wants.get(usize::from(slot))
        && value > f32::from_bits(w.load(Ordering::Relaxed))
    {
        w.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// The next noise sample, in [-1, 1): SplitMix64.
fn next_noise(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    // The top 24 bits, exactly representable as an `f32`.
    (z >> 40) as f32 / (1u64 << 23) as f32 - 1.0
}

/// What was written `t` samples before the next write at `at`, linearly
/// interpolated. `t` is held between 1 and the line's length less one.
fn read_delay(data: &[f32], at: usize, t: f32) -> f32 {
    let len = data.len();
    let t = if t.is_finite() {
        t.clamp(1.0, (len - 2) as f32)
    } else {
        1.0
    };
    // A time a hair off a whole number of samples (as `10ms * RATE` can be
    // in floating point) reads that sample exactly.
    let t = if (t - t.round()).abs() < 1e-3 {
        t.round()
    } else {
        t
    };
    let whole = t.floor();
    let frac = t - whole;
    let back = whole as usize;
    let index = |back: usize| (at + len - back) % len;
    let a = data[index(back)];
    let b = data[index(back + 1)];
    a + (b - a) * frac
}

/// `data` at frame `at` plus `part` of one, between frames on a cubic
/// through the four nearest; silence before the start and from the end on.
/// A position a hair off a whole frame (as `t * RATE` can be in floating
/// point) reads that frame exactly.
fn read_audio(data: &[f32], at: f32, part: f32) -> f32 {
    let pos = f64::from(at) + f64::from(part);
    let len = data.len();
    if !(pos >= 0.0 && pos < len as f64) {
        return 0.0;
    }
    let near = pos.round();
    if (pos - near).abs() < 1e-3 {
        return data.get(near as usize).copied().unwrap_or(0.0);
    }
    let i = pos.floor() as usize;
    let t = (pos - pos.floor()) as f32;
    let x = |k: isize| {
        let k = i as isize + k;
        if k < 0 { 0.0 } else { data.get(k as usize).copied().unwrap_or(0.0) }
    };
    let (y0, y1, y2, y3) = (x(-1), x(0), x(1), x(2));
    // Catmull-Rom: through y1 at 0 and y2 at 1.
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

/// Run one instruction the general way; a jump's target if it jumps.
fn step(instr: &Instr, code: &Code, run: &mut Run) -> Option<usize> {
    let regs = &mut run.regs[..];
    match *instr {
        Instr::Op2 { op, dst, a, b } => {
            regs[dst as usize] = op.apply(val(regs, a), val(regs, b));
        }
        Instr::Op1 { op, dst, x } => {
            regs[dst as usize] = op.apply(val(regs, x));
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
        Instr::Vector { block } => {
            let state = LaneState {
                noise: &mut run.noise,
                delays: &mut run.delays,
                wants: &run.wants,
                rate: run.rate,
                seqs: &code.seqs,
                slots: &run.slots,
                at: run.at,
                audios: &code.audios,
            };
            run_vector(&code.vectors[block as usize], regs, state);
        }
        Instr::InvokeSeq { call } => invoke(call, code, run),
        Instr::EventArg { index, value } => {
            run.event_args[usize::from(index)] = val(regs, value);
        }
        Instr::InvokeEvent { event } => {
            let kind = code.decls[usize::from(event.0)].kind;
            let payload = Payload::from_values(kind, run.event_args);
            run.queue
                .push_back(Pending::To(event, payload, Sender::Host(0)));
        }
        Instr::Halt { seq, id } => {
            let id = id.map(|id| val(regs, id) as i32);
            let seq = usize::from(seq);
            for k in 0..run.slots[seq].len() {
                let slot = &run.slots[seq][k];
                if slot.active && id.is_none_or(|id| id == slot.id) {
                    stop(run, seq, k, EventKind::Halted);
                }
            }
        }
        Instr::AudioRead {
            dst,
            audio,
            channel,
            at,
            part,
        } => {
            let data = &code.audios[usize::from(audio)][usize::from(channel)];
            regs[dst as usize] = read_audio(data, val(regs, at), val(regs, part));
        }
        Instr::SeqRead { dst, seq, info, id } => {
            let seq = usize::from(seq);
            regs[dst as usize] =
                seq_read(&code.seqs[seq], &run.slots[seq], run.at, info, val(regs, id));
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
        Instr::Noise { dst, source } => {
            regs[dst as usize] = next_noise(&mut run.noise[usize::from(source)]);
        }
        Instr::DelayRead {
            dst,
            line,
            time,
            want,
        } => {
            let t = val(regs, time);
            let (data, at) = &run.delays[usize::from(line)];
            if t > (data.len() - 2) as f32 {
                note_want(&run.wants, want, t / run.rate);
            }
            regs[dst as usize] = read_delay(data, *at, t);
        }
        Instr::Want { slot, x, ceiling } => {
            let x = val(regs, x);
            if x > ceiling {
                note_want(&run.wants, slot, x);
            }
        }
        Instr::Sum {
            dst,
            init,
            base,
            n,
            stride,
        } => {
            let mut acc = val(regs, init);
            let (base, stride) = (base as usize, stride as usize);
            for k in 0..n as usize {
                acc += regs[base + k * stride];
            }
            regs[dst as usize] = acc;
        }
        Instr::DelayWrite { line, x } => {
            let x = val(regs, x);
            let (data, at) = &mut run.delays[usize::from(line)];
            data[*at] = x;
            *at = if *at + 1 == data.len() { 0 } else { *at + 1 };
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

    let slots = &run.slots[seq];
    let existing = slots
        .iter()
        .position(|s| s.active && s.id == id && call.id.is_some());
    let k = match existing {
        Some(_) if start.is_none() => return,
        Some(k) => {
            stop(run, seq, k, EventKind::Halted);
            k
        }
        None => match slots.iter().position(|s| !s.active) {
            Some(k) => k,
            None => {
                // Never one started in this sample: replacing those could
                // go round forever, each start replacing the last.
                let now = run.now;
                let Some(k) = (0..slots.len())
                    .filter(|&k| slots[k].started < now)
                    .min_by_key(|&k| slots[k].started)
                else {
                    return;
                };
                stop(run, seq, k, EventKind::Replaced);
                k
            }
        },
    };
    let slot = &mut run.slots[seq][k];
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
    slot.pass = 1;
    slot.current = step;
    // After `trigger N`, the first beat is the next one in the pattern.
    slot.beat = (slot.pos - EPS).ceil().max(0.0) as u32;
    slot.bar_sent = false;
    slot.step_sent = false;
    slot.rest_sent = false;
    slot.finishing = false;
    slot.change = None;
    slot.settled = false;
    slot.taken = 0;
    slot.went_on = false;
    run.send(
        seq,
        Payload::Seq {
            kind: EventKind::Start,
            values: [id, step as i32 + 1, 0, 0],
        },
    );
}

impl Node for Program {
    fn name(&self) -> &'static str {
        "rill"
    }

    fn program(&self) -> Option<&Program> {
        Some(self)
    }

    fn program_mut(&mut self) -> Option<&mut Program> {
        Some(self)
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
        if self.streams.is_some() {
            self.process_segments(ctx, inputs, out);
            return;
        }
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

    fn program_size(&self) -> Option<ProgramSize> {
        let code = &self.code;
        let instrs = code.instrs.iter().chain(&code.post);
        let operations = instrs
            .map(|i| match *i {
                Instr::Vector { block } => {
                    let block = &code.vectors[block as usize];
                    block.instrs.len() * usize::from(block.lanes)
                }
                Instr::Sum { n, .. } => n as usize,
                _ => 1,
            })
            .sum();
        Some(ProgramSize {
            instructions: code.instrs.len() + code.post.len(),
            operations,
            registers: code.regs,
        })
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
        run.noise.copy_from_slice(&self.code.noises);
        for (line, at) in &mut run.delays {
            line.fill(0.0);
            *at = 0;
        }
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

// ---- taking over from another build -----------------------------------------

/// Whether a pool's copies (or a loop's iterations) run together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grouping {
    /// What it is: a pool's state key, or where a loop is.
    pub name: String,
    pub copies: usize,
    /// Why its copies run one at a time, or `None` if they run together.
    pub why: Option<String>,
}

/// How big a program is, per sample: what the interpreter dispatches, and
/// the work that does (an instruction per lane for a vector block, an add
/// per value for a sum).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgramSize {
    pub instructions: usize,
    pub operations: usize,
    pub registers: usize,
}

/// The places a program sized when it was built, with their sizes, and a
/// counter each for what it has asked of them while playing.
pub type Ceilings = (
    Vec<(String, f32)>,
    std::sync::Arc<[std::sync::atomic::AtomicU32]>,
);

/// What a running program has, by name: its [`StateKeys`] and its
/// sequences. Taken from the running program, to plan how a new build takes
/// over from it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keys {
    pub state: StateKeys,
    /// Sequence names, by index.
    pub seqs: Vec<String>,
}

/// How a new build takes over the state of the running one: for each piece
/// of state both have, where it was and where it is now. Worked out off the
/// audio thread; [`Program::take_over`] then only copies.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Takeover {
    /// `(old, new)` registers, delay lines, noise generators, voice pools
    /// and sequences.
    pub regs: Vec<(u32, u32)>,
    pub delays: Vec<(u32, u32)>,
    pub noises: Vec<(u32, u32)>,
    pub pools: Vec<(u32, u32)>,
    pub seqs: Vec<(u32, u32)>,
    /// Pieces of state carried over, and new ones starting fresh.
    pub carried: usize,
    pub fresh: usize,
}

/// A kind of state, so keys of different kinds never pair up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Kind {
    Reg,
    Delay,
    Noise,
    Pool,
}

impl Takeover {
    /// Pair `new`'s state with `old`'s: first by key; then, for keys left
    /// on both sides, a level renamed (`lead` to `melody`, or a `state`
    /// renamed) pairs when everything below it has the same shape and no
    /// other candidate does.
    pub fn plan(old: &Keys, new: &Keys) -> Takeover {
        let list = |k: &StateKeys| -> Vec<(Kind, String, u32)> {
            let mut out = Vec::new();
            for (kind, items) in [
                (Kind::Reg, &k.regs),
                (Kind::Delay, &k.delays),
                (Kind::Noise, &k.noises),
                (Kind::Pool, &k.pools),
            ] {
                out.extend(items.iter().map(|(key, i)| (kind, key.clone(), *i)));
            }
            out
        };
        let (olds, news) = (list(&old.state), list(&new.state));
        let mut pairs: Vec<(Kind, u32, u32)> = Vec::new();
        let old_by: std::collections::HashMap<(Kind, &str), u32> = olds
            .iter()
            .map(|(k, key, i)| ((*k, key.as_str()), *i))
            .collect();
        let mut old_left: Vec<&(Kind, String, u32)> = Vec::new();
        let mut new_left: Vec<&(Kind, String, u32)> = Vec::new();
        let new_keys: std::collections::HashSet<(Kind, &str)> =
            news.iter().map(|(k, key, _)| (*k, key.as_str())).collect();
        for n in &news {
            match old_by.get(&(n.0, n.1.as_str())) {
                Some(&o) => pairs.push((n.0, o, n.2)),
                None => new_left.push(n),
            }
        }
        for o in &olds {
            if !new_keys.contains(&(o.0, o.1.as_str())) {
                old_left.push(o);
            }
        }
        pairs.extend(renamed(&old_left, &new_left));

        let mut t = Takeover::default();
        for (kind, o, n) in pairs {
            let list = match kind {
                Kind::Reg => &mut t.regs,
                Kind::Delay => &mut t.delays,
                Kind::Noise => &mut t.noises,
                Kind::Pool => &mut t.pools,
            };
            list.push((o, n));
        }
        for (j, name) in new.seqs.iter().enumerate() {
            if let Some(i) = old.seqs.iter().position(|o| o == name) {
                t.seqs.push((i as u32, j as u32));
            }
        }
        t.carried = t.regs.len() + t.delays.len() + t.noises.len() + t.pools.len();
        t.fresh = news.len() - t.carried;
        t
    }
}

/// Pairs for keys renamed at one level: an old and a new prefix that differ
/// only in their last name, where everything under each has the same kinds
/// and the same rest of the key, and each is the other's only candidate.
fn renamed(old: &[&(Kind, String, u32)], new: &[&(Kind, String, u32)]) -> Vec<(Kind, u32, u32)> {
    use std::collections::{BTreeMap, HashMap, HashSet};
    // A key's levels: `a/b/c.x` is `a`, `a/b`, `a/b/c.x`.
    let prefixes = |key: &str| -> Vec<usize> {
        let mut ends: Vec<usize> = key.match_indices('/').map(|(i, _)| i).collect();
        ends.push(key.len());
        ends
    };
    // Every prefix with what lies under it: (kind, rest of key, index).
    type Under = Vec<(Kind, String, u32)>;
    let subtrees = |keys: &[&(Kind, String, u32)]| -> BTreeMap<String, Under> {
        let mut map: BTreeMap<String, Under> = BTreeMap::new();
        for (kind, key, i) in keys.iter().map(|k| (k.0, &k.1, k.2)) {
            for end in prefixes(key) {
                map.entry(key[..end].to_owned()).or_default().push((
                    kind,
                    key[end..].to_owned(),
                    i,
                ));
            }
        }
        for under in map.values_mut() {
            under.sort();
        }
        map
    };
    let (olds, news) = (subtrees(old), subtrees(new));
    let shape =
        |u: &Under| -> Vec<(Kind, String)> { u.iter().map(|(k, r, _)| (*k, r.clone())).collect() };
    let parent = |p: &str| p.rfind('/').map_or("", |i| &p[..i]).to_owned();
    // Candidates, shallowest first, so a renamed `let` pairs as a whole.
    let mut used_old: HashSet<(Kind, u32)> = HashSet::new();
    let mut used_new: HashSet<(Kind, u32)> = HashSet::new();
    let mut pairs = Vec::new();
    let mut by_depth: BTreeMap<usize, Vec<&String>> = BTreeMap::new();
    for p in olds.keys() {
        by_depth.entry(p.matches('/').count()).or_default().push(p);
    }
    for (_, level) in by_depth {
        let mut candidates: HashMap<&String, Vec<&String>> = HashMap::new();
        let mut back: HashMap<&String, usize> = HashMap::new();
        for p in level {
            if news.contains_key(p) {
                continue;
            }
            let ps = shape(&olds[p]);
            for (q, under) in &news {
                if olds.contains_key(q)
                    || parent(p) != parent(q)
                    || q.matches('/').count() != p.matches('/').count()
                {
                    continue;
                }
                if shape(under) == ps {
                    candidates.entry(p).or_default().push(q);
                    *back.entry(q).or_default() += 1;
                }
            }
        }
        for (p, qs) in candidates {
            let [q] = qs.as_slice() else { continue };
            if back[q] != 1 {
                continue;
            }
            for (a, b) in olds[p].iter().zip(&news[*q]) {
                if !used_old.contains(&(a.0, a.2)) && !used_new.contains(&(b.0, b.2)) {
                    used_old.insert((a.0, a.2));
                    used_new.insert((b.0, b.2));
                    pairs.push((a.0, a.2, b.2));
                }
            }
        }
    }
    pairs
}

impl Program {
    /// The places sized when the program was built, with their sizes, and
    /// what they have been asked for while playing, in the same order.
    pub fn grouping(&self) -> &[Grouping] {
        &self.code.grouping
    }

    pub fn stages(&self) -> &super::stages::Stages {
        &self.code.stages
    }

    /// How many registers have a buffer for running in stages, and how
    /// many samples each holds; `None` when the tick runs sample by sample.
    pub fn stream_buffers(&self) -> Option<(usize, usize)> {
        self.streams.as_ref().map(|s| (s.buffered.len(), SEGMENT))
    }

    pub fn ceilings(&self) -> Ceilings {
        (self.code.ceilings.clone(), self.run.wants.clone())
    }

    /// The names of this program's state, for planning a takeover.
    pub fn keys(&self) -> Keys {
        Keys {
            state: self.code.keys.clone(),
            seqs: self.code.seqs.iter().map(|s| s.name.clone()).collect(),
        }
    }

    /// Carry `old`'s state over into this program, as `plan` says. Only
    /// copies: safe on the audio thread.
    pub fn take_over(&mut self, old: &Program, plan: &Takeover) {
        let (run, from) = (&mut self.run, &old.run);
        for &(o, n) in &plan.regs {
            if let (Some(v), Some(slot)) = (from.regs.get(o as usize), run.regs.get_mut(n as usize))
            {
                *slot = *v;
            }
        }
        for &(o, n) in &plan.delays {
            let (Some((src, at)), Some((dst, dst_at))) =
                (from.delays.get(o as usize), run.delays.get_mut(n as usize))
            else {
                continue;
            };
            // The most recent samples, as many as both hold, in order; the
            // next write goes at 0.
            let (sl, dl) = (src.len(), dst.len());
            dst.fill(0.0);
            for k in 1..sl.min(dl) {
                dst[dl - k] = src[(at + sl - k) % sl];
            }
            *dst_at = 0;
        }
        for &(o, n) in &plan.noises {
            if let (Some(v), Some(slot)) =
                (from.noise.get(o as usize), run.noise.get_mut(n as usize))
            {
                *slot = *v;
            }
        }
        for &(o, n) in &plan.pools {
            if let (Some(src), Some(dst)) =
                (from.voices.get(o as usize), run.voices.get_mut(n as usize))
            {
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = *s;
                }
            }
        }
        for &(o, n) in &plan.seqs {
            let (Some(src), Some(dst)) =
                (from.slots.get(o as usize), run.slots.get_mut(n as usize))
            else {
                continue;
            };
            for (d, s) in dst.iter_mut().zip(src) {
                copy_slot(d, s);
            }
        }
        run.now = from.now;
        run.started = from.started;
        run.fresh = from.fresh;
    }
}

/// `src` into `dst` without allocating: what does not fit `dst`'s buffers is
/// dropped.
fn copy_slot(dst: &mut Slot, src: &Slot) {
    dst.active = src.active;
    dst.id = src.id;
    // Calls are numbered per build; the instance keeps its settings as they
    // are, but no longer follows the call that started it.
    dst.call = u16::MAX;
    dst.started = src.started;
    dst.pos = src.pos;
    dst.next = src.next;
    dst.chord = src.chord;
    dst.repeats = src.repeats;
    dst.looping = src.looping;
    dst.values = src.values;
    dst.pass = src.pass;
    dst.current = src.current;
    dst.beat = src.beat;
    dst.bar_sent = src.bar_sent;
    dst.step_sent = src.step_sent;
    dst.rest_sent = src.rest_sent;
    dst.finishing = src.finishing;
    dst.change = src.change;
    dst.settled = src.settled;
    dst.taken = src.taken;
    dst.went_on = src.went_on;
    dst.captures.clear();
    let room = dst.captures.capacity();
    dst.captures.extend(src.captures.iter().take(room));
    dst.held.clear();
    let room = dst.held.capacity();
    dst.held.extend(src.held.iter().take(room));
}
