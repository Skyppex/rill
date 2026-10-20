//! Clean-ups on compiled code, after it is generated.
//!
//! The compiler emits straightforward code: every result goes to a fresh
//! register, and is copied to where it belongs (a `state` register, the
//! result of an `if`). This pass removes what that leaves behind:
//!
//! - **Writing results in place:** `r3 = wrap r2; r1 = r3` becomes
//!   `r1 = wrap r2` when `r3` is used only by the copy and nothing in
//!   between touches `r1`.
//! - **Dead code:** instructions whose result is never read.
//!
//! Every list of instructions is looked at together (the tick, the code
//! after it, and the handlers), since they share registers.

use std::collections::HashMap;

use super::vm::{Code, Instr, Operand, Reg, Source, VArg, VInstr};
use crate::ops::Op2;

/// Rewrite `code` into an equivalent with fewer instructions.
pub fn optimize(code: &mut Code) {
    loop {
        let changed = coalesce(code) | remove_dead(code);
        if !changed {
            break;
        }
    }
}

/// The register an instruction writes as its result, for instructions that
/// do nothing else.
fn pure_dst(i: &Instr) -> Option<Reg> {
    match *i {
        Instr::Op1 { dst, .. }
        | Instr::Op2 { dst, .. }
        | Instr::Tune { dst, .. }
        | Instr::Copy { dst, .. }
        | Instr::SeqRead { dst, .. }
        | Instr::AudioRead { dst, .. }
        | Instr::Sum { dst, .. } => Some(dst),
        _ => None,
    }
}

fn pure_dst_mut(i: &mut Instr) -> Option<&mut Reg> {
    match i {
        Instr::Op1 { dst, .. }
        | Instr::Op2 { dst, .. }
        | Instr::Tune { dst, .. }
        | Instr::Copy { dst, .. }
        | Instr::SeqRead { dst, .. }
        | Instr::AudioRead { dst, .. }
        | Instr::Sum { dst, .. } => Some(dst),
        _ => None,
    }
}

/// Every register an instruction writes.
pub(crate) fn writes(i: &Instr, f: &mut impl FnMut(Reg)) {
    match *i {
        Instr::Op1 { dst, .. }
        | Instr::Op2 { dst, .. }
        | Instr::Tune { dst, .. }
        | Instr::Copy { dst, .. }
        | Instr::Select { dst, .. }
        | Instr::LoadCapture { dst, .. }
        | Instr::Noise { dst, .. }
        | Instr::DelayRead { dst, .. }
        | Instr::SeqRead { dst, .. }
        | Instr::AudioRead { dst, .. }
        | Instr::Sum { dst, .. } => f(dst),
        Instr::DelayWrite { .. }
        | Instr::Want { .. }
        | Instr::JumpUnless { .. }
        | Instr::Jump { .. }
        | Instr::InvokeSeq { .. }
        | Instr::InvokeEvent { .. }
        | Instr::EventArg { .. }
        | Instr::Halt { .. }
        | Instr::SetSlot { .. }
        | Instr::Vector { .. } => {}
    }
}

/// Every register an instruction reads.
pub(crate) fn reads(i: &Instr, f: &mut impl FnMut(Reg)) {
    let mut op = |o: Operand| {
        if let Operand::Reg(r) = o {
            f(r)
        }
    };
    match *i {
        Instr::Op1 { x, .. } => op(x),
        Instr::Op2 { a, b, .. } => {
            op(a);
            op(b);
        }
        Instr::Copy { src, .. } => op(src),
        Instr::Tune {
            pitch, setting, a4, ..
        } => {
            op(pitch);
            op(setting);
            op(a4);
        }
        Instr::Select { cond, a, b, .. } => {
            op(cond);
            op(a);
            op(b);
        }
        Instr::JumpUnless { cond, .. } => op(cond),
        Instr::EventArg { value, .. } => op(value),
        Instr::InvokeEvent { .. } => {}
        Instr::Halt { id, .. } => id.into_iter().for_each(op),
        Instr::SetSlot { value, .. } => op(value),
        Instr::DelayRead { time, .. } => op(time),
        Instr::SeqRead { id, .. } => op(id),
        Instr::AudioRead { at, part, .. } => {
            op(at);
            op(part);
        }
        Instr::DelayWrite { x, .. } => op(x),
        Instr::Jump { .. }
        | Instr::InvokeSeq { .. }
        | Instr::LoadCapture { .. }
        | Instr::Noise { .. }
        | Instr::Vector { .. } => {}
        Instr::Want { x, .. } => op(x),
        Instr::Sum {
            init,
            base,
            n,
            stride,
            ..
        } => {
            op(init);
            for k in 0..n {
                f(base + k * stride);
            }
        }
    }
}

/// How each register is used across the whole program.
pub(crate) struct Usage {
    pub defs: Vec<u32>,
    pub uses: Vec<u32>,
    /// Registers that something outside the instruction lists refers to:
    /// state, inputs, payloads, outputs, sequence calls. They keep their
    /// identity.
    pub pinned: Vec<bool>,
}

fn lists(code: &Code) -> impl Iterator<Item = &Vec<Instr>> {
    [&code.instrs, &code.post]
        .into_iter()
        .chain(code.events.iter().map(|e| &e.instrs))
}

pub(crate) fn usage(code: &Code) -> Usage {
    let n = code.regs;
    let mut u = Usage {
        defs: vec![0; n],
        uses: vec![0; n],
        pinned: vec![false; n],
    };
    for list in lists(code) {
        for i in list {
            writes(i, &mut |r| u.defs[r as usize] += 1);
            reads(i, &mut |r| u.uses[r as usize] += 1);
        }
    }
    // What vector blocks read and write, lane by lane.
    for block in &code.vectors {
        let n = usize::from(block.lanes);
        for i in &block.instrs {
            vector_regs(i, &mut |a| match a {
                VArg::Lanes(b) => (0..n).for_each(|k| u.uses[b as usize + k] += 1),
                VArg::Scalar(r) => u.uses[r as usize] += 1,
                VArg::Const(_) => {}
            });
            if let Some(d) = vector_dst(i) {
                (0..n).for_each(|k| u.defs[d as usize + k] += 1);
            }
        }
    }
    let mut pin = |r: Reg| {
        u.pinned[r as usize] = true;
        u.uses[r as usize] += 1;
    };
    let pin_op = |o: Operand, pin: &mut dyn FnMut(Reg)| {
        if let Operand::Reg(r) = o {
            pin(r)
        }
    };
    for &(r, _) in &code.state_init {
        pin(r);
    }
    for &r in &code.input_regs {
        pin(r);
    }
    for &o in &code.output {
        pin_op(o, &mut pin);
    }
    for copy in code.pools.iter().flatten() {
        for &o in copy {
            pin_op(o, &mut pin);
        }
    }
    for e in &code.events {
        for &r in &e.payload {
            pin(r);
        }
    }
    for call in &code.calls {
        pin(call.dst);
        for &o in call
            .id
            .iter()
            .chain(&call.step)
            .chain(&call.repeat)
            .chain(&call.looping)
        {
            pin_op(o, &mut pin);
        }
        for &o in &call.captures {
            pin_op(o, &mut pin);
        }
        for s in call.settings {
            match s {
                Source::Default => {}
                Source::Now(o) | Source::PerSlot { initial: o } => pin_op(o, &mut pin),
                Source::Follow(r) => pin(r),
            }
        }
    }
    u
}

/// Chains of adds over evenly spaced registers become one [`Instr::Sum`]:
/// `t1 = init + x0; t2 = t1 + x1; ...; d = tn + xn`, where `x0..=xn` are
/// `stride` apart, as `sum` over the copies of a pool is once they run
/// together (their results are a lane group, one apart). One instruction
/// instead of one per copy; the adds happen in the same order, so the
/// result is the same to the bit.
///
/// Each `t` must be read only by the next add, and the chain must run
/// straight through, with nothing in between writing the registers it adds
/// (the sum reads them all at the end).
pub(crate) fn sums(code: &mut Code) {
    let u = usage(code);
    let lists = std::iter::once(&mut code.instrs)
        .chain(std::iter::once(&mut code.post))
        .chain(code.events.iter_mut().map(|e| &mut e.instrs));
    for list in lists {
        let targets = jump_targets(list);
        // Where each single-use register is written by an add.
        let mut add_at: HashMap<Reg, usize> = HashMap::new();
        for (p, i) in list.iter().enumerate() {
            if let Instr::Op2 {
                op: Op2::Add, dst, ..
            } = *i
            {
                let d = dst as usize;
                if u.defs[d] == 1 && u.uses[d] == 1 && !u.pinned[d] {
                    add_at.insert(dst, p);
                }
            }
        }
        let mut remove = vec![false; list.len()];
        // From the end, so the longest chain is found from its last add.
        for t in (0..list.len()).rev() {
            if remove[t] {
                continue;
            }
            let Instr::Op2 {
                op: Op2::Add,
                dst,
                a: Operand::Reg(mut acc),
                b: Operand::Reg(last),
            } = list[t]
            else {
                continue;
            };
            // Walk back along the chain while each add brings in the
            // register `stride` before the one after it.
            let mut leaves = vec![last];
            let mut links = Vec::new();
            let mut stride = None;
            let mut init = Operand::Reg(acc);
            while let Some(&q) = add_at.get(&acc) {
                let Instr::Op2 {
                    a,
                    b: Operand::Reg(x),
                    ..
                } = list[q]
                else {
                    break;
                };
                let prev = *leaves.last().expect("one at least");
                let s = prev.checked_sub(x).filter(|&s| s > 0);
                if s.is_none() || stride.is_some_and(|st| Some(st) != s) || q >= t {
                    break;
                }
                stride = s;
                leaves.push(x);
                links.push(q);
                init = a;
                match a {
                    Operand::Reg(r) => acc = r,
                    Operand::Const(_) => break,
                }
            }
            let Some(stride) = stride else { continue };
            // A sum of fewer than four is not worth it.
            if leaves.len() < 4 {
                continue;
            }
            let first = *links.last().expect("links for every leaf but one");
            // Straight through from the first add to the last, nothing
            // writing what is added, and nothing that writes registers it
            // does not say (vector blocks).
            let mut leaf_set: std::collections::HashSet<Reg> = leaves.iter().copied().collect();
            if let Operand::Reg(r) = init {
                leaf_set.insert(r);
            }
            let clear = (first..t).all(|p| {
                let mut writes_leaf = false;
                writes(&list[p], &mut |r| writes_leaf |= leaf_set.contains(&r));
                !targets[p + 1]
                    && !writes_leaf
                    && !matches!(
                        list[p],
                        Instr::Jump { .. } | Instr::JumpUnless { .. } | Instr::Vector { .. }
                    )
            });
            if !clear {
                continue;
            }
            let base = *leaves.last().expect("leaves");
            list[t] = Instr::Sum {
                dst,
                init,
                base,
                n: leaves.len() as u32,
                stride,
            };
            for q in links {
                remove[q] = true;
            }
        }
        if remove.iter().any(|&r| r) {
            retain(list, &remove);
        }
    }
}

/// The operands a vector instruction reads.
pub(crate) fn vector_regs(i: &VInstr, f: &mut impl FnMut(VArg)) {
    match *i {
        VInstr::Op2 { a, b, .. } => {
            f(a);
            f(b);
        }
        VInstr::Op1 { x, .. } => f(x),
        VInstr::Copy { s, .. } => f(s),
        VInstr::Select { cond, a, b, .. } => {
            f(cond);
            f(a);
            f(b);
        }
        VInstr::Tune {
            pitch, setting, a4, ..
        } => {
            f(pitch);
            f(setting);
            f(a4);
        }
        VInstr::DelayRead { time, .. } => f(time),
        VInstr::SeqRead { id, .. } => f(id),
        VInstr::AudioRead { at, part, .. } => {
            f(at);
            f(part);
        }
        VInstr::DelayWrite { x, .. } => f(x),
        VInstr::Noise { .. } => {}
    }
}

/// The lane group a vector instruction writes.
pub(crate) fn vector_dst(i: &VInstr) -> Option<Reg> {
    match *i {
        VInstr::Op2 { d, .. }
        | VInstr::Op1 { d, .. }
        | VInstr::Copy { d, .. }
        | VInstr::Select { d, .. }
        | VInstr::Tune { d, .. }
        | VInstr::Noise { d, .. }
        | VInstr::DelayRead { d, .. }
        | VInstr::SeqRead { d, .. }
        | VInstr::AudioRead { d, .. } => Some(d),
        VInstr::DelayWrite { .. } => None,
    }
}

/// What `a op b` leaves as it is: `x + 0`, `x * 1` and the like, as
/// `Compiler::op2` folds them outside pools.
fn identity(op: Op2, a: Option<f32>, b: Option<f32>) -> Option<bool> {
    match (op, a, b) {
        (Op2::Add | Op2::Sub, _, Some(0.0)) => Some(true),
        (Op2::Mul | Op2::Div | Op2::Pow, _, Some(1.0)) => Some(true),
        (Op2::Add, Some(0.0), _) | (Op2::Mul, Some(1.0), _) => Some(false),
        _ => None,
    }
}

/// After grouping, fold what the copies of pools kept so they stayed alike:
/// selects on a constant become copies of the side chosen, `x + 0` and the
/// like become copies of `x`, and operations on constants their result. In
/// vector blocks too, where the constant turned out the same in every lane.
/// [`optimize`] then removes the copies.
pub(crate) fn simplify(code: &mut Code) {
    let konst = |o: Operand| match o {
        Operand::Const(c) => Some(c),
        Operand::Reg(_) => None,
    };
    let lists = std::iter::once(&mut code.instrs)
        .chain(std::iter::once(&mut code.post))
        .chain(code.events.iter_mut().map(|e| &mut e.instrs));
    for list in lists {
        for i in list.iter_mut() {
            *i = match *i {
                Instr::Select { dst, cond, a, b } => match konst(cond) {
                    Some(c) => Instr::Copy {
                        dst,
                        src: if c != 0.0 { a } else { b },
                    },
                    None => continue,
                },
                Instr::Op2 { op, dst, a, b } => match (konst(a), konst(b)) {
                    (Some(x), Some(y)) => Instr::Copy {
                        dst,
                        src: Operand::Const(op.apply(x, y)),
                    },
                    (x, y) => match identity(op, x, y) {
                        Some(keep_a) => Instr::Copy {
                            dst,
                            src: if keep_a { a } else { b },
                        },
                        None => continue,
                    },
                },
                Instr::Op1 {
                    op,
                    dst,
                    x: Operand::Const(x),
                } => Instr::Copy {
                    dst,
                    src: Operand::Const(op.apply(x)),
                },
                _ => continue,
            };
        }
    }
    let konst = |a: VArg| match a {
        VArg::Const(c) => Some(c),
        _ => None,
    };
    for block in &mut code.vectors {
        for i in &mut block.instrs {
            *i = match *i {
                VInstr::Select { d, cond, a, b } => match konst(cond) {
                    Some(c) => VInstr::Copy {
                        d,
                        s: if c != 0.0 { a } else { b },
                    },
                    None => continue,
                },
                VInstr::Op2 { op, d, a, b } => match identity(op, konst(a), konst(b)) {
                    Some(keep_a) => VInstr::Copy {
                        d,
                        s: if keep_a { a } else { b },
                    },
                    None => continue,
                },
                _ => continue,
            };
        }
    }
    forward_copies(code);
}

/// In vector blocks, `d = s` goes away, as a copy in a block is a loop over
/// every lane, unlike a register that is simply read where it is:
///
/// - when `d` is a temporary of the block (nothing but this copy writes it,
///   nothing outside the block, nor state or outputs, reads it, and the
///   block does not write `s` again after the copy), later instructions
///   read `s` instead;
/// - when `s` is a temporary that only this copy reads, what computed it
///   writes `d` instead, if nothing between touches `d`.
fn forward_copies(code: &mut Code) {
    let u = usage(code);
    for block in &mut code.vectors {
        let n = usize::from(block.lanes);
        let mut i = 0;
        while i < block.instrs.len() {
            let VInstr::Copy { d, s } = block.instrs[i] else {
                i += 1;
                continue;
            };
            let later = &block.instrs[i + 1..];
            // Reads of `d` after the copy, per lane.
            let mut reads = 0u32;
            for j in later {
                vector_regs(j, &mut |a| reads += u32::from(a == VArg::Lanes(d)));
            }
            let lanes = (0..n).map(|k| d as usize + k);
            let temporary = lanes
                .clone()
                .all(|r| u.defs[r] == 1 && !u.pinned[r] && u.uses[r] == reads);
            let source_kept = match s {
                VArg::Lanes(b) => later.iter().all(|j| vector_dst(j) != Some(b)),
                VArg::Scalar(_) | VArg::Const(_) => true,
            };
            if s == VArg::Lanes(d) {
                i += 1;
                continue;
            }
            if !temporary || !source_kept {
                // The other way around: `s` is a temporary that only this copy
                // reads, so what computed it can write `d` instead.
                if let VArg::Lanes(b) = s
                    && let Some(j) = (0..i)
                        .rev()
                        .find(|&j| vector_dst(&block.instrs[j]) == Some(b))
                    && (0..n).all(|k| {
                        let r = b as usize + k;
                        u.defs[r] == 1 && u.uses[r] == 1 && !u.pinned[r]
                    })
                    && block.instrs[j + 1..i].iter().all(|x| {
                        let mut touches = vector_dst(x) == Some(d);
                        vector_regs(x, &mut |a| touches |= a == VArg::Lanes(d));
                        !touches
                    })
                {
                    set_vector_dst(&mut block.instrs[j], d);
                    block.instrs.remove(i);
                    continue;
                }
                i += 1;
                continue;
            }
            for j in &mut block.instrs[i + 1..] {
                vector_args_mut(j, &mut |a| {
                    if *a == VArg::Lanes(d) {
                        *a = s;
                    }
                });
            }
            block.instrs.remove(i);
        }
    }
}

/// Make a vector instruction write lane group `d`.
fn set_vector_dst(i: &mut VInstr, to: Reg) {
    match i {
        VInstr::Op2 { d, .. }
        | VInstr::Op1 { d, .. }
        | VInstr::Copy { d, .. }
        | VInstr::Select { d, .. }
        | VInstr::Tune { d, .. }
        | VInstr::Noise { d, .. }
        | VInstr::DelayRead { d, .. }
        | VInstr::SeqRead { d, .. }
        | VInstr::AudioRead { d, .. } => *d = to,
        VInstr::DelayWrite { .. } => {}
    }
}

/// The operands of a vector instruction, to change.
fn vector_args_mut(i: &mut VInstr, f: &mut impl FnMut(&mut VArg)) {
    match i {
        VInstr::Op2 { a, b, .. } => {
            f(a);
            f(b);
        }
        VInstr::Op1 { x, .. } => f(x),
        VInstr::Copy { s, .. } => f(s),
        VInstr::Select { cond, a, b, .. } => {
            f(cond);
            f(a);
            f(b);
        }
        VInstr::Tune {
            pitch, setting, a4, ..
        } => {
            f(pitch);
            f(setting);
            f(a4);
        }
        VInstr::DelayRead { time, .. } => f(time),
        VInstr::SeqRead { id, .. } => f(id),
        VInstr::AudioRead { at, part, .. } => {
            f(at);
            f(part);
        }
        VInstr::DelayWrite { x, .. } => f(x),
        VInstr::Noise { .. } => {}
    }
}

/// Positions some jump in `list` lands on.
fn jump_targets(list: &[Instr]) -> Vec<bool> {
    let mut targets = vec![false; list.len() + 1];
    for i in list {
        if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = *i {
            targets[target as usize] = true;
        }
    }
    targets
}

/// `s = ...; ...; d = s` becomes `d = ...; ...` where that cannot change
/// anything: `s` is a temporary written and read exactly once, and the code
/// between runs straight through without touching `d`.
fn coalesce(code: &mut Code) -> bool {
    let u = usage(code);
    let mut changed = false;
    let ranges = &mut code.lane_ranges;
    let lists = std::iter::once(&mut code.instrs)
        .chain(std::iter::once(&mut code.post))
        .chain(code.events.iter_mut().map(|e| &mut e.instrs));
    for (index, list) in lists.enumerate() {
        let targets = jump_targets(list);
        let mut remove = vec![false; list.len()];
        for c in 0..list.len() {
            let Instr::Copy {
                dst: d,
                src: Operand::Reg(s),
            } = list[c]
            else {
                continue;
            };
            let si = s as usize;
            if s == d || u.defs[si] != 1 || u.uses[si] != 1 || u.pinned[si] {
                continue;
            }
            // The definition of `s`, before the copy and in a straight line
            // with it.
            let mut p = c;
            let mut ok = false;
            while p > 0 {
                p -= 1;
                if remove[p] {
                    continue;
                }
                let i = &list[p];
                if pure_dst(i) == Some(s) {
                    ok = true;
                    break;
                }
                let mut touches = false;
                writes(i, &mut |r| touches |= r == d);
                reads(i, &mut |r| touches |= r == d);
                // A vector block's registers are not in `reads`/`writes`.
                if touches
                    || targets[p + 1]
                    || matches!(
                        i,
                        Instr::Jump { .. } | Instr::JumpUnless { .. } | Instr::Vector { .. }
                    )
                {
                    break;
                }
            }
            // Nothing may jump to just before the copy either.
            if !ok || targets[c] {
                continue;
            }
            *pure_dst_mut(&mut list[p]).expect("defines s") = d;
            remove[c] = true;
            changed = true;
        }
        if remove.iter().any(|&r| r) {
            let moved = retain(list, &remove);
            if index == 0 {
                remap_ranges(ranges, &moved);
            }
        }
    }
    changed
}

/// Remove instructions whose only effect is a result nobody reads.
fn remove_dead(code: &mut Code) -> bool {
    let u = usage(code);
    let mut changed = false;
    let ranges = &mut code.lane_ranges;
    let lists = std::iter::once(&mut code.instrs)
        .chain(std::iter::once(&mut code.post))
        .chain(code.events.iter_mut().map(|e| &mut e.instrs));
    for (index, list) in lists.enumerate() {
        let remove: Vec<bool> = list
            .iter()
            .map(|i| match (i, pure_dst(i)) {
                // Copying a register onto itself does nothing.
                (
                    Instr::Copy {
                        dst,
                        src: Operand::Reg(s),
                    },
                    _,
                ) if dst == s => true,
                (_, Some(d)) => u.uses[d as usize] == 0 && !u.pinned[d as usize],
                _ => false,
            })
            .collect();
        if remove.iter().any(|&r| r) {
            let moved = retain(list, &remove);
            if index == 0 {
                remap_ranges(ranges, &moved);
            }
            changed = true;
        }
    }
    changed
}

/// Drop the instructions marked in `remove`, moving jumps to the
/// instruction that now follows their old target.
fn retain(list: &mut Vec<Instr>, remove: &[bool]) -> Vec<u32> {
    // The new position of each old position (and of the end).
    let mut new_pos = Vec::with_capacity(list.len() + 1);
    let mut kept = 0u32;
    for &r in remove {
        new_pos.push(kept);
        kept += u32::from(!r);
    }
    new_pos.push(kept);
    let mut out = Vec::with_capacity(kept as usize);
    for (i, instr) in list.iter().enumerate() {
        if remove[i] {
            continue;
        }
        let mut instr = *instr;
        if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = &mut instr {
            *target = new_pos[*target as usize];
        }
        out.push(instr);
    }
    *list = out;
    new_pos
}

/// Move the copy ranges of pools along after instructions of the tick were
/// removed.
fn remap_ranges(ranges: &mut [Option<Vec<(u32, u32)>>], new_pos: &[u32]) {
    for (s, e) in ranges.iter_mut().flatten().flatten() {
        *s = new_pos[*s as usize];
        *e = new_pos[*e as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Op1, Op2};

    fn r(n: Reg) -> Operand {
        Operand::Reg(n)
    }

    fn code(instrs: Vec<Instr>, regs: usize, output: Vec<Operand>) -> Code {
        Code {
            instrs,
            regs,
            output,
            state_init: vec![(1, 0.0)],
            ..Code::default()
        }
    }

    #[test]
    fn results_are_written_in_place() {
        // r2 = r1 + 1; r3 = wrap r2; r1 = r3  ->  r2 = r1 + 1; r1 = wrap r2
        let mut c = code(
            vec![
                Instr::Op2 {
                    op: Op2::Add,
                    dst: 2,
                    a: r(1),
                    b: Operand::Const(1.0),
                },
                Instr::Op1 {
                    op: Op1::Wrap,
                    dst: 3,
                    x: r(2),
                },
                Instr::Copy { dst: 1, src: r(3) },
            ],
            4,
            vec![r(1)],
        );
        optimize(&mut c);
        assert_eq!(c.instrs.len(), 2);
        assert_eq!(
            c.instrs[1],
            Instr::Op1 {
                op: Op1::Wrap,
                dst: 1,
                x: r(2)
            }
        );
    }

    #[test]
    fn not_when_the_destination_is_read_in_between() {
        // r3 = r1 * 2; out = r1 (old value); r1 = r3
        let mut c = code(
            vec![
                Instr::Op2 {
                    op: Op2::Mul,
                    dst: 3,
                    a: r(1),
                    b: Operand::Const(2.0),
                },
                Instr::Op2 {
                    op: Op2::Add,
                    dst: 4,
                    a: r(1),
                    b: Operand::Const(0.5),
                },
                Instr::Copy { dst: 1, src: r(3) },
            ],
            5,
            vec![r(4)],
        );
        let before = c.instrs.clone();
        optimize(&mut c);
        assert_eq!(c.instrs, before);
    }

    #[test]
    fn not_across_a_jump_target() {
        // unless r0 goto 2; r3 = r0 + 1; r1 = r3 (reached from the jump too)
        let mut c = code(
            vec![
                Instr::JumpUnless {
                    cond: r(0),
                    target: 2,
                },
                Instr::Op2 {
                    op: Op2::Add,
                    dst: 3,
                    a: r(0),
                    b: Operand::Const(1.0),
                },
                Instr::Copy { dst: 1, src: r(3) },
            ],
            4,
            vec![r(1)],
        );
        let before = c.instrs.clone();
        optimize(&mut c);
        assert_eq!(c.instrs, before);
    }

    #[test]
    fn dead_code_goes_and_jumps_follow() {
        // unless r0 goto 3; r5 = unused; r6 = unused; r2 = r0 * 3; out r2
        let mut c = code(
            vec![
                Instr::JumpUnless {
                    cond: r(0),
                    target: 3,
                },
                Instr::Op1 {
                    op: Op1::Sin,
                    dst: 5,
                    x: r(0),
                },
                Instr::Op1 {
                    op: Op1::Cos,
                    dst: 6,
                    x: r(5),
                },
                Instr::Op2 {
                    op: Op2::Mul,
                    dst: 2,
                    a: r(0),
                    b: Operand::Const(3.0),
                },
            ],
            7,
            vec![r(2)],
        );
        optimize(&mut c);
        assert_eq!(
            c.instrs,
            [
                Instr::JumpUnless {
                    cond: r(0),
                    target: 1
                },
                Instr::Op2 {
                    op: Op2::Mul,
                    dst: 2,
                    a: r(0),
                    b: Operand::Const(3.0)
                },
            ]
        );
    }

    #[test]
    fn side_effects_stay() {
        let mut c = code(
            vec![Instr::Halt { seq: 0, id: None }],
            2,
            vec![Operand::Const(0.0)],
        );
        optimize(&mut c);
        assert_eq!(c.instrs.len(), 1);
    }
}
