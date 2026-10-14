//! Running the copies of a voice pool together.
//!
//! The copies of a lifted rill, or of `[x; N]`, are the same code on
//! different registers. When they sit back to back in the tick's code and
//! line up instruction for instruction, they are replaced by one
//! [`VectorBlock`]: each instruction then works on all copies (lanes) at
//! once, which is one dispatch instead of `N` and lets the compiler use SIMD.
//!
//! On the way:
//!
//! - **Branches become selects.** Copies can take different branches, so
//!   each copy's code is made straight-line first: both sides run, and
//!   writes that must not happen on the side not taken go through a
//!   `Select` on that side's condition. A temporary is computed without a
//!   select when everything that reads it can only run after it, since
//!   whatever it feeds is guarded in turn. A register every path writes,
//!   and nothing on the way reads (the result of an `if` with an `else`),
//!   needs no guard on its first write. The tick's other `if`s are made
//!   straight-line the same way ([`straighten_branches`]).
//! - **Registers are renumbered.** A register that differs per copy joins a
//!   lane group: one register per copy, in a row. Everything in the program
//!   that refers to those registers is renumbered to match. A constant that
//!   differs per copy becomes a lane group set when the program loads.
//! - **Noise runs per lane.** Each copy's `noise()` has its own generator;
//!   lane `k` takes the next number from the `k`th copy's, so they must be
//!   evenly spaced (a copy with three generators has them three apart).
//!   Only noise that runs on every path can join: on a side not taken it
//!   would still move its generator on.
//!
//! A pool is left as it is when anything does not fit: other side effects
//! in a copy (delays, events), copies that differ in shape, or registers
//! that would need two places.

use std::collections::HashMap;

use super::opt::{Usage, reads, usage, writes};
use super::vm::{Code, Grouping, Instr, Operand, Reg, Source, VArg, VInstr, VectorBlock};
use crate::ops::{Op1, Op2};

/// Replace every pool whose copies can run together with a vector block.
pub fn vectorize(code: &mut Code) {
    let usage = usage(code);
    // What happened to each pool, as far as it is known yet.
    let mut outcome: Vec<Option<Outcome>> = vec![None; code.lane_ranges.len()];
    // Pools whose copies are back to back in the tick's code.
    let mut regions: Vec<(usize, u32, u32)> = Vec::new();
    for (p, r) in code.lane_ranges.iter().enumerate() {
        let Some(r) = r else { continue };
        if r.len() < 2 || r[0].0 >= r[r.len() - 1].1 {
            continue;
        }
        if r.windows(2).all(|w| w[0].1 == w[1].0) {
            regions.push((p, r[0].0, r[r.len() - 1].1));
        } else {
            outcome[p] = Some(Outcome::Left(Why::Apart));
        }
    }
    // The most lanes first, then the largest: of an outer pool and the
    // pools inside it, the one with more copies gains the most.
    regions.sort_by_key(|&(p, s, e)| {
        let lanes = code.lane_ranges[p].as_ref().map_or(0, Vec::len);
        (std::cmp::Reverse(lanes), std::cmp::Reverse(e - s))
    });
    // Where the pools that run together were.
    let mut grouped: Vec<(u32, u32)> = Vec::new();
    // Old register -> new register, for every register in a lane group.
    let mut renames: HashMap<Reg, Reg> = HashMap::new();
    // Regions already replaced by a single instruction: where they were and
    // how many instructions went.
    let mut replaced: Vec<(u32, u32)> = Vec::new();
    // In order of priority, since a register can only go in one lane group.
    // A pool inside one that runs together already does so; one inside a
    // pool that cannot gets its own try (the voices of an echo cannot, as
    // they read delay lines, but the taps inside each voice can).
    for (pool, s0, e0) in regions {
        if let Some(&(s, e)) = grouped.iter().find(|&&(s, e)| !(e0 <= s || s0 >= e)) {
            // Inside a pool that runs together, it does too; around one (a
            // pool with fewer copies than those inside it, tried later),
            // it is left as it is.
            outcome[pool] = Some(if s <= s0 && e0 <= e {
                Outcome::Inside
            } else {
                Outcome::Left(Why::Holds)
            });
            continue;
        }
        let shift: u32 = replaced
            .iter()
            .filter(|&&(at, _)| at < s0)
            .map(|&(_, removed)| removed)
            .sum();
        let (s, e) = (s0 - shift, e0 - shift);
        let ranges: Vec<(u32, u32)> = code.lane_ranges[pool]
            .clone()
            .expect("chosen from these")
            .into_iter()
            .map(|(a, b)| (a - shift, b - shift))
            .collect();
        let mut next = code.regs as u32;
        let (block, new_names, consts) =
            match vectorize_pool(code, &usage, &ranges, s, e, &renames, &mut next) {
                Ok(done) => done,
                Err(why) => {
                    outcome[pool] = Some(Outcome::Left(why));
                    continue;
                }
            };
        outcome[pool] = Some(Outcome::Grouped);
        grouped.push((s0, e0));
        code.regs = next as usize;
        renames.extend(new_names);
        code.state_init.extend(consts);
        let index = code.vectors.len() as u32;
        code.vectors.push(block);
        replace_region(&mut code.instrs, s, e, Instr::Vector { block: index });
        replaced.push((s0, e0 - s0 - 1));
    }
    code.grouping = outcome
        .iter()
        .enumerate()
        .filter_map(|(p, o)| {
            let o = o.as_ref()?;
            let copies = code.lane_ranges[p].as_ref().map_or(0, Vec::len);
            let why = match o {
                Outcome::Grouped | Outcome::Inside => None,
                Outcome::Left(why) => Some(why.to_string()),
            };
            Some(Grouping {
                name: code.lane_names.get(p).cloned().unwrap_or_default(),
                copies,
                why,
            })
        })
        .collect();
    // The copy ranges no longer match the code.
    code.lane_ranges.fill(None);
    if !renames.is_empty() {
        rename(code, &renames);
    }
}

/// Make every `if` in the tick straight-line code, where it can be: both
/// sides run, and the writes of the side not taken go nowhere (the same as
/// copies get before they run together). Code without jumps can run over
/// many samples at once (see [`super::stages`]); a region with jumps
/// cannot. A region with side effects on only some paths (noise, delay
/// lines, events) stays as it is.
pub fn straighten_branches(code: &mut Code) {
    if !code
        .instrs
        .iter()
        .any(|i| matches!(i, Instr::Jump { .. } | Instr::JumpUnless { .. }))
    {
        return;
    }
    let usage = usage(code);
    let list = std::mem::take(&mut code.instrs);
    let mut next = code.regs as u32;
    let mut out: Vec<Instr> = Vec::with_capacity(list.len());
    // Where each old position went, for the jumps that stay.
    let mut moved = vec![0u32; list.len() + 1];
    let mut i = 0;
    while i < list.len() {
        let (Instr::Jump { target } | Instr::JumpUnless { target, .. }) = list[i] else {
            moved[i] = out.len() as u32;
            out.push(list[i]);
            i += 1;
            continue;
        };
        // The region: from the jump to the furthest place it, or a jump
        // inside it, goes.
        let mut end = target as usize;
        let mut p = i;
        while p < end {
            if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = list[p] {
                end = end.max(target as usize);
            }
            p += 1;
        }
        let end = end.min(list.len());
        let mut tried = next;
        match straighten(&list[i..end], i as u32, &usage, &mut tried) {
            Ok(flat) => {
                next = tried;
                moved[i..end].fill(out.len() as u32);
                out.extend(flat);
            }
            Err(_) => {
                for (k, instr) in list[i..end].iter().enumerate() {
                    moved[i + k] = out.len() as u32;
                    out.push(*instr);
                }
            }
        }
        i = end;
    }
    moved[list.len()] = out.len() as u32;
    for instr in &mut out {
        if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = instr {
            *target = moved[*target as usize];
        }
    }
    if out.len() != list.len() || next as usize != code.regs {
        // The copy ranges no longer match the code.
        code.lane_ranges.fill(None);
    }
    code.instrs = out;
    code.regs = next as usize;
}

/// What became of a pool.
#[derive(Clone, Debug)]
enum Outcome {
    Grouped,
    /// Inside a pool that runs together, so it does too.
    Inside,
    Left(Why),
}

/// Why a pool's copies cannot run together.
#[derive(Clone, Debug)]
enum Why {
    /// Other code sits between its copies.
    Apart,
    /// Code outside jumps into the middle of it.
    JumpIn,
    /// It jumps out of its own code.
    JumpOut,
    /// The copies compiled to different instructions.
    Differ,
    /// An instruction that has no form for lanes.
    Unsupported(&'static str),
    /// `noise()` that does not run on every path.
    BranchNoise,
    /// The copies' noise generators are not evenly spaced.
    NoiseSpacing,
    /// A delay line used on only some paths.
    BranchDelay,
    /// The copies' delay lines (or their ceilings) are not evenly spaced,
    /// or several copies write one line.
    DelaySpacing,
    /// A register would need two places, or there are none left.
    Registers,
    /// More copies than a block can hold.
    TooMany,
    /// Pools inside it, with more copies, run together already.
    Holds,
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::Apart => write!(f, "other code sits between its copies"),
            Why::JumpIn => write!(f, "code outside it jumps into it"),
            Why::JumpOut => write!(f, "it jumps out of its own code"),
            Why::Differ => write!(
                f,
                "its copies compile to different code (a branch decided when \
                 building, or a different callee)"
            ),
            Why::Unsupported(what) => write!(f, "it {what}"),
            Why::BranchNoise => write!(f, "it calls noise() on only some paths"),
            Why::NoiseSpacing => write!(f, "its noise generators are not evenly spaced"),
            Why::BranchDelay => write!(f, "it uses a delay line on only some paths"),
            Why::DelaySpacing => write!(f, "its delay lines are not evenly spaced"),
            Why::Registers => write!(f, "a value would need two places in the lanes"),
            Why::TooMany => write!(f, "it has more than 65,535 copies"),
            Why::Holds => write!(f, "pools inside it, with more copies, run together instead"),
        }
    }
}

/// The step between evenly spaced `values` (0 if they are all the same), if
/// they are, in the order given.
fn spacing(values: &[u16]) -> Option<u16> {
    let step = values[1].checked_sub(values[0])?;
    values
        .iter()
        .enumerate()
        .all(|(k, &v)| usize::from(v) == usize::from(values[0]) + k * usize::from(step))
        .then_some(step)
}

/// What an instruction that cannot run in lanes does, for [`Why`].
fn what(i: &Instr) -> &'static str {
    match i {
        Instr::DelayRead { .. } => "reads a delay line",
        Instr::DelayWrite { .. } => "writes a delay line",
        Instr::Want { .. } => "checks a ceiling (a growing max)",
        Instr::InvokeSeq { .. } => "starts a sequence",
        Instr::Halt { .. } => "stops a sequence",
        Instr::InvokeEvent { .. } | Instr::EventArg { .. } => "sends an event",
        Instr::LoadCapture { .. } | Instr::SetSlot { .. } => "follows a playing sequence",
        Instr::SeqRead { .. } => "reads a playing sequence",
        Instr::AudioRead { .. } => "reads an audio file",
        Instr::Vector { .. } => "holds copies that already run together",
        Instr::Sum { .. } => "sums",
        Instr::Noise { .. } => "calls noise()",
        Instr::Jump { .. } | Instr::JumpUnless { .. } => "branches",
        Instr::Op1 { .. }
        | Instr::Op2 { .. }
        | Instr::Copy { .. }
        | Instr::Select { .. }
        | Instr::Tune { .. } => "computes",
    }
}

/// Lane values of one operand position, sorted out.
enum Lanes {
    Const(f32),
    Scalar(Reg),
    /// Different constants per lane.
    Consts(Vec<f32>),
    /// A different register per lane.
    Regs(Vec<Reg>),
}

fn classify(ops: &[Operand]) -> Option<Lanes> {
    match ops[0] {
        Operand::Const(c0) => {
            let mut cs = Vec::with_capacity(ops.len());
            for o in ops {
                let Operand::Const(c) = *o else { return None };
                cs.push(c);
            }
            Some(if cs.iter().all(|c| c.to_bits() == c0.to_bits()) {
                Lanes::Const(c0)
            } else {
                Lanes::Consts(cs)
            })
        }
        Operand::Reg(r0) => {
            let mut rs = Vec::with_capacity(ops.len());
            for o in ops {
                let Operand::Reg(r) = *o else { return None };
                rs.push(r);
            }
            if rs.iter().all(|&r| r == r0) {
                Some(Lanes::Scalar(r0))
            } else {
                Some(Lanes::Regs(rs))
            }
        }
    }
}

/// Building one block: where each register of the copies goes.
struct Groups<'a> {
    lanes: usize,
    /// From earlier pools.
    earlier: &'a HashMap<Reg, Reg>,
    /// From this pool.
    names: HashMap<Reg, Reg>,
    consts: Vec<(Reg, f32)>,
    next: &'a mut u32,
    /// Registers written somewhere in the region: they vary per lane.
    written: &'a std::collections::HashSet<Reg>,
}

impl Groups<'_> {
    fn alloc(&mut self) -> Option<Reg> {
        let base = *self.next;
        *self.next = self.next.checked_add(self.lanes as u32)?;
        Some(base)
    }

    fn name(&self, r: Reg) -> Option<Reg> {
        self.names.get(&r).or_else(|| self.earlier.get(&r)).copied()
    }

    /// The lane group holding `regs`, one per lane, creating it if none of
    /// them has a place yet.
    fn group(&mut self, regs: &[Reg]) -> Option<Reg> {
        match self.name(regs[0]) {
            Some(base) => {
                for (k, &r) in regs.iter().enumerate() {
                    if self.name(r) != Some(base + k as Reg) {
                        return None;
                    }
                }
                Some(base)
            }
            None => {
                if regs.iter().any(|&r| self.name(r).is_some()) {
                    return None;
                }
                let mut seen = std::collections::HashSet::new();
                if !regs.iter().all(|r| seen.insert(*r)) {
                    return None;
                }
                let base = self.alloc()?;
                for (k, &r) in regs.iter().enumerate() {
                    self.names.insert(r, base + k as Reg);
                }
                Some(base)
            }
        }
    }

    fn arg(&mut self, ops: &[Operand]) -> Option<VArg> {
        Some(match classify(ops)? {
            Lanes::Const(c) => VArg::Const(c),
            Lanes::Scalar(r) => {
                // The same register in every lane, which no lane changes.
                if self.written.contains(&r) || self.name(r).is_some() {
                    return None;
                }
                VArg::Scalar(r)
            }
            Lanes::Consts(cs) => {
                let base = self.alloc()?;
                for (k, c) in cs.into_iter().enumerate() {
                    self.consts.push((base + k as Reg, c));
                }
                VArg::Lanes(base)
            }
            Lanes::Regs(rs) => VArg::Lanes(self.group(&rs)?),
        })
    }

    fn dst(&mut self, regs: &[Reg]) -> Option<Reg> {
        self.group(regs)
    }
}

type Block = (VectorBlock, HashMap<Reg, Reg>, Vec<(Reg, f32)>);

fn vectorize_pool(
    code: &Code,
    usage: &Usage,
    ranges: &[(u32, u32)],
    s: u32,
    e: u32,
    earlier: &HashMap<Reg, Reg>,
    next: &mut u32,
) -> Result<Block, Why> {
    let list = &code.instrs;
    // Nothing outside may jump into the middle of the region.
    for (i, instr) in list.iter().enumerate() {
        let i = i as u32;
        if (s..e).contains(&i) {
            continue;
        }
        if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = *instr
            && target > s
            && target < e
        {
            return Err(Why::JumpIn);
        }
    }
    // Each copy as straight-line code.
    let mut copies: Vec<Vec<Instr>> = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges {
        copies.push(straighten(&list[a as usize..b as usize], a, usage, next)?);
    }
    let len = copies[0].len();
    if copies.iter().any(|c| c.len() != len) {
        return Err(Why::Differ);
    }
    let mut written = std::collections::HashSet::new();
    for c in &copies {
        for i in c {
            writes(i, &mut |r| {
                written.insert(r);
            });
        }
    }
    let mut g = Groups {
        lanes: copies.len(),
        earlier,
        names: HashMap::new(),
        consts: Vec::new(),
        next,
        written: &written,
    };
    let mut instrs = Vec::with_capacity(len);
    for p in 0..len {
        let at: Vec<&Instr> = copies.iter().map(|c| &c[p]).collect();
        let ops = |f: &dyn Fn(&Instr) -> Operand| at.iter().map(|i| f(i)).collect::<Vec<_>>();
        let dsts = |f: &dyn Fn(&Instr) -> Reg| at.iter().map(|i| f(i)).collect::<Vec<_>>();
        let v = match *at[0] {
            Instr::Op2 { op, .. } => {
                if !at
                    .iter()
                    .all(|i| matches!(i, Instr::Op2 { op: o, .. } if *o == op))
                {
                    return Err(Why::Differ);
                }
                let a = g
                    .arg(&ops(&|i| match *i {
                        Instr::Op2 { a, .. } => a,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                let b = g
                    .arg(&ops(&|i| match *i {
                        Instr::Op2 { b, .. } => b,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::Op2 { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::Op2 { op, d, a, b }
            }
            Instr::Op1 { op, .. } => {
                if !at
                    .iter()
                    .all(|i| matches!(i, Instr::Op1 { op: o, .. } if *o == op))
                {
                    return Err(Why::Differ);
                }
                let x = g
                    .arg(&ops(&|i| match *i {
                        Instr::Op1 { x, .. } => x,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::Op1 { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::Op1 { op, d, x }
            }
            Instr::Copy { .. } => {
                if !at.iter().all(|i| matches!(i, Instr::Copy { .. })) {
                    return Err(Why::Differ);
                }
                let s = g
                    .arg(&ops(&|i| match *i {
                        Instr::Copy { src, .. } => src,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::Copy { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::Copy { d, s }
            }
            Instr::Select { .. } => {
                if !at.iter().all(|i| matches!(i, Instr::Select { .. })) {
                    return Err(Why::Differ);
                }
                let part = |n: usize| {
                    ops(&move |i| match *i {
                        Instr::Select { cond, a, b, .. } => [cond, a, b][n],
                        _ => unreachable!(),
                    })
                };
                let cond = g.arg(&part(0)).ok_or(Why::Registers)?;
                let a = g.arg(&part(1)).ok_or(Why::Registers)?;
                let b = g.arg(&part(2)).ok_or(Why::Registers)?;
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::Select { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::Select { d, cond, a, b }
            }
            Instr::Tune { tuning, .. } => {
                if !at
                    .iter()
                    .all(|i| matches!(i, Instr::Tune { tuning: t, .. } if *t == tuning))
                {
                    return Err(Why::Differ);
                }
                let part = |n: usize| {
                    ops(&move |i| match *i {
                        Instr::Tune {
                            pitch, setting, a4, ..
                        } => [pitch, setting, a4][n],
                        _ => unreachable!(),
                    })
                };
                let pitch = g.arg(&part(0)).ok_or(Why::Registers)?;
                let setting = g.arg(&part(1)).ok_or(Why::Registers)?;
                let a4 = g.arg(&part(2)).ok_or(Why::Registers)?;
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::Tune { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::Tune {
                    tuning,
                    d,
                    pitch,
                    setting,
                    a4,
                }
            }
            Instr::Noise { source, .. } => {
                // Lane `k` takes generator `source + k * stride`: the copies'
                // generators must be evenly spaced, in the order of the
                // copies.
                let sources: Vec<usize> = at
                    .iter()
                    .map(|i| match **i {
                        Instr::Noise { source, .. } => Some(usize::from(source)),
                        _ => None,
                    })
                    .collect::<Option<_>>()
                    .ok_or(Why::Differ)?;
                let stride = sources[1]
                    .checked_sub(sources[0])
                    .filter(|&s| s > 0)
                    .ok_or(Why::NoiseSpacing)?;
                if sources
                    .iter()
                    .enumerate()
                    .any(|(k, &s)| s != sources[0] + k * stride)
                {
                    return Err(Why::NoiseSpacing);
                }
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::Noise { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::Noise {
                    d,
                    source,
                    stride: u16::try_from(stride).map_err(|_| Why::NoiseSpacing)?,
                }
            }
            Instr::DelayRead { .. } => {
                let parts: Vec<(u16, u16)> = at
                    .iter()
                    .map(|i| match **i {
                        Instr::DelayRead { line, want, .. } => Some((line, want)),
                        _ => None,
                    })
                    .collect::<Option<_>>()
                    .ok_or(Why::Differ)?;
                let (lines, wants): (Vec<u16>, Vec<u16>) = parts.into_iter().unzip();
                let stride = spacing(&lines).ok_or(Why::DelaySpacing)?;
                let want_stride = spacing(&wants).ok_or(Why::DelaySpacing)?;
                let time = g
                    .arg(&ops(&|i| match *i {
                        Instr::DelayRead { time, .. } => time,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::DelayRead { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::DelayRead {
                    d,
                    line: lines[0],
                    stride,
                    time,
                    want: wants[0],
                    want_stride,
                }
            }
            Instr::AudioRead { audio, channel, .. } => {
                if !at.iter().all(|i| {
                    matches!(**i, Instr::AudioRead { audio: a, channel: c, .. } if a == audio && c == channel)
                }) {
                    return Err(Why::Differ);
                }
                let at_arg = g
                    .arg(&ops(&|i| match *i {
                        Instr::AudioRead { at, .. } => at,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                let part = g
                    .arg(&ops(&|i| match *i {
                        Instr::AudioRead { part, .. } => part,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::AudioRead { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::AudioRead {
                    d,
                    audio,
                    channel,
                    at: at_arg,
                    part,
                }
            }
            Instr::SeqRead { seq, info, .. } => {
                if !at.iter().all(
                    |i| matches!(**i, Instr::SeqRead { seq: s, info: f, .. } if s == seq && f == info),
                ) {
                    return Err(Why::Differ);
                }
                let id = g
                    .arg(&ops(&|i| match *i {
                        Instr::SeqRead { id, .. } => id,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                let d = g
                    .dst(&dsts(&|i| match *i {
                        Instr::SeqRead { dst, .. } => dst,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::SeqRead { d, seq, info, id }
            }
            Instr::DelayWrite { .. } => {
                let lines: Vec<u16> = at
                    .iter()
                    .map(|i| match **i {
                        Instr::DelayWrite { line, .. } => Some(line),
                        _ => None,
                    })
                    .collect::<Option<_>>()
                    .ok_or(Why::Differ)?;
                // Every lane must write a line of its own.
                let stride = spacing(&lines)
                    .filter(|&s| s > 0)
                    .ok_or(Why::DelaySpacing)?;
                let x = g
                    .arg(&ops(&|i| match *i {
                        Instr::DelayWrite { x, .. } => x,
                        _ => unreachable!(),
                    }))
                    .ok_or(Why::Registers)?;
                VInstr::DelayWrite {
                    line: lines[0],
                    stride,
                    x,
                }
            }
            other => return Err(Why::Unsupported(what(&other))),
        };
        instrs.push(v);
    }
    let lanes = u16::try_from(copies.len()).map_err(|_| Why::TooMany)?;
    Ok((VectorBlock { lanes, instrs }, g.names, g.consts))
}

/// `copy` (which starts at `offset` in the tick's code) as straight-line
/// code with the same effect, or `None` if it has side effects.
fn straighten(
    copy: &[Instr],
    offset: u32,
    usage: &Usage,
    next: &mut u32,
) -> Result<Vec<Instr>, Why> {
    let len = copy.len();
    let mut targets = vec![Vec::new(); len + 1];
    for (i, instr) in copy.iter().enumerate() {
        match *instr {
            Instr::Jump { target } | Instr::JumpUnless { target, .. } => {
                let t = target.checked_sub(offset).ok_or(Why::JumpOut)? as usize;
                if t > len {
                    return Err(Why::JumpOut);
                }
                targets[t].push(i);
            }
            Instr::Op1 { .. }
            | Instr::Op2 { .. }
            | Instr::Copy { .. }
            | Instr::Select { .. }
            | Instr::Tune { .. }
            | Instr::Noise { .. }
            | Instr::DelayRead { .. }
            | Instr::DelayWrite { .. }
            | Instr::SeqRead { .. }
            | Instr::AudioRead { .. } => {}
            other => return Err(Why::Unsupported(what(&other))),
        }
    }
    let jumps = copy
        .iter()
        .any(|i| matches!(i, Instr::Jump { .. } | Instr::JumpUnless { .. }));
    if !jumps {
        return Ok(copy.to_vec());
    }
    let target_of = |i: &Instr| match *i {
        Instr::Jump { target } | Instr::JumpUnless { target, .. } => (target - offset) as usize,
        _ => unreachable!(),
    };
    // Where each instruction can go next.
    let succ = |i: usize| -> Vec<usize> {
        match copy[i] {
            Instr::Jump { .. } => vec![target_of(&copy[i])],
            Instr::JumpUnless { .. } => vec![i + 1, target_of(&copy[i])],
            _ => vec![i + 1],
        }
    };
    // Paths from the start to each position, counted.
    let mut from = vec![0u128; len + 1];
    from[0] = 1;
    for i in 0..len {
        for j in succ(i) {
            from[j] = from[j].saturating_add(from[i]);
        }
    }
    // Paths from `start` to each later position.
    let paths_from = |start: usize| {
        let mut c = vec![0u128; len + 1];
        c[start] = 1;
        for i in start..len {
            if c[i] == 0 {
                continue;
            }
            for j in succ(i) {
                c[j] = c[j].saturating_add(c[i]);
            }
        }
        c
    };
    let total = from[len];
    let to_end = |i: usize| paths_from(i)[len];
    let saturated = |x: u128| x == u128::MAX;
    // Every path passes through `i`.
    let always: Vec<bool> = (0..len)
        .map(|i| {
            let through = from[i].saturating_mul(to_end(i));
            !saturated(through) && !saturated(total) && through == total
        })
        .collect();
    // `i` comes before `j` on every path that reaches `j`.
    let dominates = |i: usize, j: usize| {
        let via = from[i].saturating_mul(paths_from(i)[j]);
        !saturated(via) && via == from[j]
    };
    // Reads of each register inside the copy, by position.
    let mut local_reads: HashMap<Reg, Vec<usize>> = HashMap::new();
    for (i, instr) in copy.iter().enumerate() {
        reads(instr, &mut |r| local_reads.entry(r).or_default().push(i));
    }
    // Registers every path writes and nothing here reads (the result of an
    // `if` with an `else`): their value from before is never seen, so the
    // first write needs no guard. Paths that skip it write over it later.
    let mut first_free: HashMap<Reg, usize> = HashMap::new();
    let mut written: HashMap<Reg, Vec<usize>> = HashMap::new();
    for (i, instr) in copy.iter().enumerate() {
        writes(instr, &mut |r| written.entry(r).or_default().push(i));
    }
    for (&d, at) in &written {
        if local_reads.contains_key(&d) {
            continue;
        }
        // Paths to the end that write `d` nowhere.
        let mut c = vec![0u128; len + 1];
        c[0] = 1;
        for i in 0..len {
            if c[i] == 0 || at.contains(&i) {
                continue;
            }
            for j in succ(i) {
                c[j] = c[j].saturating_add(c[i]);
            }
        }
        if c[len] == 0 {
            first_free.insert(d, at[0]);
        }
    }
    let fresh = |next: &mut u32| -> Option<Reg> {
        let r = *next;
        *next = next.checked_add(1)?;
        Some(r)
    };

    let one = Operand::Const(1.0);
    let mut out: Vec<Instr> = Vec::with_capacity(len * 2);
    let mut incoming: Vec<Vec<Operand>> = vec![Vec::new(); len + 1];
    let mut cur: Option<Operand> = Some(one);
    for i in 0..len {
        // This position runs when any way into it does.
        let pred = if always[i] {
            Some(one)
        } else {
            let mut ways: Vec<Operand> = cur.into_iter().collect();
            ways.extend(incoming[i].iter().copied());
            let mut acc: Option<Operand> = None;
            for w in ways {
                acc = Some(match acc {
                    None => w,
                    Some(a) => {
                        let d = fresh(next).ok_or(Why::Registers)?;
                        out.push(Instr::Op2 {
                            op: Op2::Or,
                            dst: d,
                            a,
                            b: w,
                        });
                        Operand::Reg(d)
                    }
                });
            }
            acc
        };
        let Some(pred) = pred else {
            // Unreachable.
            cur = None;
            continue;
        };
        let and = |p: Operand, x: Operand, out: &mut Vec<Instr>, next: &mut u32| {
            if p == one {
                return Some(x);
            }
            let d = fresh(next)?;
            out.push(Instr::Op2 {
                op: Op2::And,
                dst: d,
                a: p,
                b: x,
            });
            Some(Operand::Reg(d))
        };
        match copy[i] {
            Instr::JumpUnless { cond, .. } => {
                let t = target_of(&copy[i]);
                let not = fresh(next).ok_or(Why::Registers)?;
                out.push(Instr::Op1 {
                    op: Op1::Not,
                    dst: not,
                    x: cond,
                });
                let taken = and(pred, Operand::Reg(not), &mut out, next).ok_or(Why::Registers)?;
                incoming[t].push(taken);
                cur = Some(and(pred, cond, &mut out, next).ok_or(Why::Registers)?);
            }
            Instr::Jump { .. } => {
                incoming[target_of(&copy[i])].push(pred);
                cur = None;
            }
            // Noise run on a path not taken would still move its generator
            // on, so only noise that always runs can go in a block.
            Instr::Noise { .. } if pred != one => return Err(Why::BranchNoise),
            // A write on a side not taken would change the line; a read
            // there could ask for a ceiling to grow for nothing.
            Instr::DelayRead { .. } | Instr::DelayWrite { .. } if pred != one => {
                return Err(Why::BranchDelay);
            }
            // Writes nothing to a register: as it is.
            Instr::DelayWrite { .. } => {
                out.push(copy[i]);
                cur = Some(pred);
            }
            instr => {
                let mut d: Reg = 0;
                writes(&instr, &mut |r| d = r);
                let di = d as usize;
                // Computing it on every path is harmless when it is a
                // temporary read only where it was written first.
                let local = local_reads.get(&d).map_or(0, Vec::len);
                let temporary = usage.defs[di] == 1
                    && !usage.pinned[di]
                    && usage.uses[di] as usize == local
                    && local_reads
                        .get(&d)
                        .is_none_or(|js| js.iter().all(|&j| j > i && dominates(i, j)));
                if pred == one || temporary || first_free.get(&d) == Some(&i) {
                    out.push(instr);
                } else {
                    let t = fresh(next).ok_or(Why::Registers)?;
                    let mut guarded = instr;
                    match &mut guarded {
                        Instr::Op1 { dst, .. }
                        | Instr::Op2 { dst, .. }
                        | Instr::Copy { dst, .. }
                        | Instr::Select { dst, .. }
                        | Instr::Tune { dst, .. } => *dst = t,
                        _ => unreachable!(),
                    }
                    out.push(guarded);
                    out.push(Instr::Select {
                        dst: d,
                        cond: pred,
                        a: Operand::Reg(t),
                        b: Operand::Reg(d),
                    });
                }
                cur = Some(pred);
            }
        }
    }
    Ok(out)
}

/// Put `instr` in place of `list[s..e]`, moving jumps after it along.
fn replace_region(list: &mut Vec<Instr>, s: u32, e: u32, instr: Instr) {
    let removed = e - s - 1;
    list.splice(s as usize..e as usize, std::iter::once(instr));
    for i in list.iter_mut() {
        if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = i
            && *target >= e
        {
            *target -= removed;
        }
    }
}

/// Renumber registers everywhere in `code` by `map`.
fn rename(code: &mut Code, map: &HashMap<Reg, Reg>) {
    let r = |x: &mut Reg| {
        if let Some(&n) = map.get(x) {
            *x = n;
        }
    };
    let op = |o: &mut Operand| {
        if let Operand::Reg(x) = o
            && let Some(&n) = map.get(x)
        {
            *x = n;
        }
    };
    let instr = |i: &mut Instr| match i {
        Instr::Op2 { dst, a, b, .. } => {
            r(dst);
            op(a);
            op(b);
        }
        Instr::Op1 { dst, x, .. } => {
            r(dst);
            op(x);
        }
        Instr::Copy { dst, src } => {
            r(dst);
            op(src);
        }
        Instr::Tune {
            dst,
            pitch,
            setting,
            a4,
            ..
        } => {
            r(dst);
            op(pitch);
            op(setting);
            op(a4);
        }
        Instr::Select { dst, cond, a, b } => {
            r(dst);
            op(cond);
            op(a);
            op(b);
        }
        Instr::JumpUnless { cond, .. } => op(cond),
        Instr::EventArg { value, .. } => op(value),
        Instr::InvokeEvent { .. } => {}
        Instr::Halt { id, .. } => {
            if let Some(id) = id {
                op(id);
            }
        }
        Instr::LoadCapture { dst, .. } => r(dst),
        Instr::SetSlot { value, .. } => op(value),
        Instr::Noise { dst, .. } => r(dst),
        Instr::DelayRead { dst, time, .. } => {
            r(dst);
            op(time);
        }
        Instr::SeqRead { dst, id, .. } => {
            r(dst);
            op(id);
        }
        Instr::AudioRead { dst, at, part, .. } => {
            r(dst);
            op(at);
            op(part);
        }
        Instr::DelayWrite { x, .. } | Instr::Want { x, .. } => op(x),
        // Made after renaming (see `opt::sums`), so its registers are
        // already the new ones.
        Instr::Sum { .. } => {}
        Instr::Jump { .. } | Instr::InvokeSeq { .. } | Instr::Vector { .. } => {}
    };
    for i in code
        .instrs
        .iter_mut()
        .chain(code.post.iter_mut())
        .chain(code.events.iter_mut().flat_map(|e| e.instrs.iter_mut()))
    {
        instr(i);
    }
    for e in &mut code.events {
        e.payload.iter_mut().for_each(r);
    }
    code.input_regs.iter_mut().for_each(r);
    code.output.iter_mut().for_each(op);
    for (x, _) in &mut code.state_init {
        r(x);
    }
    for (_, x) in &mut code.keys.regs {
        r(x);
    }
    for o in code.pools.iter_mut().flatten().flatten() {
        op(o);
    }
    for call in &mut code.calls {
        r(&mut call.dst);
        for o in call
            .id
            .iter_mut()
            .chain(call.step.iter_mut())
            .chain(call.repeat.iter_mut())
            .chain(call.looping.iter_mut())
            .chain(call.captures.iter_mut())
        {
            op(o);
        }
        for s in &mut call.settings {
            match s {
                Source::Default => {}
                Source::Now(o) | Source::PerSlot { initial: o } => op(o),
                Source::Follow(x) => r(x),
            }
        }
    }
    // Lane groups already have their new numbers; only shared registers in
    // vector blocks can still be old ones.
    for block in &mut code.vectors {
        for i in &mut block.instrs {
            let args: Vec<&mut VArg> = match i {
                VInstr::Op2 { a, b, .. } => vec![a, b],
                VInstr::Op1 { x, .. } => vec![x],
                VInstr::Copy { s, .. } => vec![s],
                VInstr::Select { cond, a, b, .. } => vec![cond, a, b],
                VInstr::Tune {
                    pitch, setting, a4, ..
                } => vec![pitch, setting, a4],
                VInstr::Noise { .. } => vec![],
                VInstr::DelayRead { time, .. } => vec![time],
                VInstr::SeqRead { id, .. } => vec![id],
                VInstr::AudioRead { at, part, .. } => vec![at, part],
                VInstr::DelayWrite { x, .. } => vec![x],
            };
            for a in args {
                if let VArg::Scalar(x) = a {
                    r(x);
                }
            }
        }
    }
}
