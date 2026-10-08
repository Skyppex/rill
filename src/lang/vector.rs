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
//!   whatever it feeds is guarded in turn.
//! - **Registers are renumbered.** A register that differs per copy joins a
//!   lane group: one register per copy, in a row. Everything in the program
//!   that refers to those registers is renumbered to match. A constant that
//!   differs per copy becomes a lane group set when the program loads.
//!
//! A pool is left as it is when anything does not fit: side effects in a
//! copy, copies that differ in shape, or registers that would need two
//! places.

use std::collections::HashMap;

use super::opt::{Usage, reads, usage, writes};
use super::vm::{Code, Instr, Operand, Source, VArg, VInstr, VectorBlock};
use crate::ops::{Op1, Op2};

/// Replace every pool whose copies can run together with a vector block.
pub fn vectorize(code: &mut Code) {
    let usage = usage(code);
    // Pools whose copies are back to back in the tick's code.
    let mut regions: Vec<(usize, u32, u32)> = code
        .lane_ranges
        .iter()
        .enumerate()
        .filter_map(|(p, r)| {
            let r = r.as_ref()?;
            let contiguous = r.windows(2).all(|w| w[0].1 == w[1].0);
            (r.len() >= 2 && contiguous && r[0].0 < r[r.len() - 1].1)
                .then(|| (p, r[0].0, r[r.len() - 1].1))
        })
        .collect();
    // The most lanes first, then the largest: of an outer pool and the
    // pools inside it, the one with more copies gains the most.
    regions.sort_by_key(|&(p, s, e)| {
        let lanes = code.lane_ranges[p].as_ref().map_or(0, Vec::len);
        (std::cmp::Reverse(lanes), std::cmp::Reverse(e - s))
    });
    let mut chosen: Vec<(usize, u32, u32)> = Vec::new();
    for r in regions {
        if chosen.iter().all(|c| r.2 <= c.1 || r.1 >= c.2) {
            chosen.push(r);
        }
    }
    // Old register -> new register, for every register in a lane group.
    let mut renames: HashMap<u16, u16> = HashMap::new();
    // Regions already replaced by a single instruction: where they were and
    // how many instructions went.
    let mut replaced: Vec<(u32, u32)> = Vec::new();
    // In order of priority, since a register can only go in one lane group.
    for (pool, s0, e0) in chosen {
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
        let Some((block, new_names, consts)) =
            vectorize_pool(code, &usage, &ranges, s, e, &renames, &mut next)
        else {
            continue;
        };
        if next > u32::from(u16::MAX) {
            continue;
        }
        code.regs = next as usize;
        renames.extend(new_names);
        code.state_init.extend(consts);
        let index = code.vectors.len() as u32;
        code.vectors.push(block);
        replace_region(&mut code.instrs, s, e, Instr::Vector { block: index });
        replaced.push((s0, e0 - s0 - 1));
    }
    // The copy ranges no longer match the code.
    code.lane_ranges.fill(None);
    if !renames.is_empty() {
        rename(code, &renames);
    }
}

/// Lane values of one operand position, sorted out.
enum Lanes {
    Const(f32),
    Scalar(u16),
    /// Different constants per lane.
    Consts(Vec<f32>),
    /// A different register per lane.
    Regs(Vec<u16>),
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
    earlier: &'a HashMap<u16, u16>,
    /// From this pool.
    names: HashMap<u16, u16>,
    consts: Vec<(u16, f32)>,
    next: &'a mut u32,
    /// Registers written somewhere in the region: they vary per lane.
    written: &'a std::collections::HashSet<u16>,
}

impl Groups<'_> {
    fn alloc(&mut self) -> Option<u16> {
        let base = u16::try_from(*self.next).ok()?;
        *self.next += self.lanes as u32;
        Some(base)
    }

    fn name(&self, r: u16) -> Option<u16> {
        self.names.get(&r).or_else(|| self.earlier.get(&r)).copied()
    }

    /// The lane group holding `regs`, one per lane, creating it if none of
    /// them has a place yet.
    fn group(&mut self, regs: &[u16]) -> Option<u16> {
        match self.name(regs[0]) {
            Some(base) => {
                for (k, &r) in regs.iter().enumerate() {
                    if self.name(r) != Some(base + k as u16) {
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
                    self.names.insert(r, base + k as u16);
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
                    self.consts.push((base + k as u16, c));
                }
                VArg::Lanes(base)
            }
            Lanes::Regs(rs) => VArg::Lanes(self.group(&rs)?),
        })
    }

    fn dst(&mut self, regs: &[u16]) -> Option<u16> {
        self.group(regs)
    }
}

type Block = (VectorBlock, HashMap<u16, u16>, Vec<(u16, f32)>);

fn vectorize_pool(
    code: &Code,
    usage: &Usage,
    ranges: &[(u32, u32)],
    s: u32,
    e: u32,
    earlier: &HashMap<u16, u16>,
    next: &mut u32,
) -> Option<Block> {
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
            return None;
        }
    }
    // Each copy as straight-line code.
    let mut copies: Vec<Vec<Instr>> = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges {
        copies.push(straighten(&list[a as usize..b as usize], a, usage, next)?);
    }
    let len = copies[0].len();
    if copies.iter().any(|c| c.len() != len) {
        return None;
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
        let dsts = |f: &dyn Fn(&Instr) -> u16| at.iter().map(|i| f(i)).collect::<Vec<_>>();
        let v = match *at[0] {
            Instr::Op2 { op, .. } => {
                if !at
                    .iter()
                    .all(|i| matches!(i, Instr::Op2 { op: o, .. } if *o == op))
                {
                    return None;
                }
                let a = g.arg(&ops(&|i| match *i {
                    Instr::Op2 { a, .. } => a,
                    _ => unreachable!(),
                }))?;
                let b = g.arg(&ops(&|i| match *i {
                    Instr::Op2 { b, .. } => b,
                    _ => unreachable!(),
                }))?;
                let d = g.dst(&dsts(&|i| match *i {
                    Instr::Op2 { dst, .. } => dst,
                    _ => unreachable!(),
                }))?;
                VInstr::Op2 { op, d, a, b }
            }
            Instr::Op1 { op, .. } => {
                if !at
                    .iter()
                    .all(|i| matches!(i, Instr::Op1 { op: o, .. } if *o == op))
                {
                    return None;
                }
                let x = g.arg(&ops(&|i| match *i {
                    Instr::Op1 { x, .. } => x,
                    _ => unreachable!(),
                }))?;
                let d = g.dst(&dsts(&|i| match *i {
                    Instr::Op1 { dst, .. } => dst,
                    _ => unreachable!(),
                }))?;
                VInstr::Op1 { op, d, x }
            }
            Instr::Copy { .. } => {
                if !at.iter().all(|i| matches!(i, Instr::Copy { .. })) {
                    return None;
                }
                let s = g.arg(&ops(&|i| match *i {
                    Instr::Copy { src, .. } => src,
                    _ => unreachable!(),
                }))?;
                let d = g.dst(&dsts(&|i| match *i {
                    Instr::Copy { dst, .. } => dst,
                    _ => unreachable!(),
                }))?;
                VInstr::Copy { d, s }
            }
            Instr::Select { .. } => {
                if !at.iter().all(|i| matches!(i, Instr::Select { .. })) {
                    return None;
                }
                let part = |n: usize| {
                    ops(&move |i| match *i {
                        Instr::Select { cond, a, b, .. } => [cond, a, b][n],
                        _ => unreachable!(),
                    })
                };
                let cond = g.arg(&part(0))?;
                let a = g.arg(&part(1))?;
                let b = g.arg(&part(2))?;
                let d = g.dst(&dsts(&|i| match *i {
                    Instr::Select { dst, .. } => dst,
                    _ => unreachable!(),
                }))?;
                VInstr::Select { d, cond, a, b }
            }
            Instr::Tune { tuning, .. } => {
                if !at
                    .iter()
                    .all(|i| matches!(i, Instr::Tune { tuning: t, .. } if *t == tuning))
                {
                    return None;
                }
                let part = |n: usize| {
                    ops(&move |i| match *i {
                        Instr::Tune {
                            pitch, setting, a4, ..
                        } => [pitch, setting, a4][n],
                        _ => unreachable!(),
                    })
                };
                let pitch = g.arg(&part(0))?;
                let setting = g.arg(&part(1))?;
                let a4 = g.arg(&part(2))?;
                let d = g.dst(&dsts(&|i| match *i {
                    Instr::Tune { dst, .. } => dst,
                    _ => unreachable!(),
                }))?;
                VInstr::Tune {
                    tuning,
                    d,
                    pitch,
                    setting,
                    a4,
                }
            }
            _ => return None,
        };
        instrs.push(v);
    }
    let lanes = u16::try_from(copies.len()).ok()?;
    Some((VectorBlock { lanes, instrs }, g.names, g.consts))
}

/// `copy` (which starts at `offset` in the tick's code) as straight-line
/// code with the same effect, or `None` if it has side effects.
fn straighten(copy: &[Instr], offset: u32, usage: &Usage, next: &mut u32) -> Option<Vec<Instr>> {
    let len = copy.len();
    let mut targets = vec![Vec::new(); len + 1];
    for (i, instr) in copy.iter().enumerate() {
        match *instr {
            Instr::Jump { target } | Instr::JumpUnless { target, .. } => {
                let t = target.checked_sub(offset)? as usize;
                if t > len {
                    return None;
                }
                targets[t].push(i);
            }
            Instr::Op1 { .. }
            | Instr::Op2 { .. }
            | Instr::Copy { .. }
            | Instr::Select { .. }
            | Instr::Tune { .. } => {}
            _ => return None,
        }
    }
    let jumps = copy
        .iter()
        .any(|i| matches!(i, Instr::Jump { .. } | Instr::JumpUnless { .. }));
    if !jumps {
        return Some(copy.to_vec());
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
    let mut local_reads: HashMap<u16, Vec<usize>> = HashMap::new();
    for (i, instr) in copy.iter().enumerate() {
        reads(instr, &mut |r| local_reads.entry(r).or_default().push(i));
    }
    let fresh = |next: &mut u32| -> Option<u16> {
        let r = u16::try_from(*next).ok()?;
        *next += 1;
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
                        let d = fresh(next)?;
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
                let not = fresh(next)?;
                out.push(Instr::Op1 {
                    op: Op1::Not,
                    dst: not,
                    x: cond,
                });
                let taken = and(pred, Operand::Reg(not), &mut out, next)?;
                incoming[t].push(taken);
                cur = Some(and(pred, cond, &mut out, next)?);
            }
            Instr::Jump { .. } => {
                incoming[target_of(&copy[i])].push(pred);
                cur = None;
            }
            instr => {
                let mut d = 0u16;
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
                if pred == one || temporary {
                    out.push(instr);
                } else {
                    let t = fresh(next)?;
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
    Some(out)
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
fn rename(code: &mut Code, map: &HashMap<u16, u16>) {
    let r = |x: &mut u16| {
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
        Instr::InvokeEvent { values, .. } => values.iter_mut().for_each(op),
        Instr::Halt { id, .. } => {
            if let Some(id) = id {
                op(id);
            }
        }
        Instr::LoadCapture { dst, .. } => r(dst),
        Instr::SetSlot { value, .. } => op(value),
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
            };
            for a in args {
                if let VArg::Scalar(x) = a {
                    r(x);
                }
            }
        }
    }
}
