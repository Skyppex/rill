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

use super::vm::{Code, Instr, Operand, Source};

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
fn pure_dst(i: &Instr) -> Option<u16> {
    match *i {
        Instr::Op1 { dst, .. }
        | Instr::Op2 { dst, .. }
        | Instr::Tune { dst, .. }
        | Instr::Copy { dst, .. } => Some(dst),
        _ => None,
    }
}

fn pure_dst_mut(i: &mut Instr) -> Option<&mut u16> {
    match i {
        Instr::Op1 { dst, .. }
        | Instr::Op2 { dst, .. }
        | Instr::Tune { dst, .. }
        | Instr::Copy { dst, .. } => Some(dst),
        _ => None,
    }
}

/// Every register an instruction writes.
fn writes(i: &Instr, f: &mut impl FnMut(u16)) {
    match *i {
        Instr::Op1 { dst, .. }
        | Instr::Op2 { dst, .. }
        | Instr::Tune { dst, .. }
        | Instr::Copy { dst, .. }
        | Instr::Select { dst, .. }
        | Instr::LoadCapture { dst, .. } => f(dst),
        Instr::JumpUnless { .. }
        | Instr::Jump { .. }
        | Instr::InvokeSeq { .. }
        | Instr::InvokeEvent { .. }
        | Instr::Halt { .. }
        | Instr::SetSlot { .. } => {}
    }
}

/// Every register an instruction reads.
fn reads(i: &Instr, f: &mut impl FnMut(u16)) {
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
        Instr::InvokeEvent { values, .. } => values.into_iter().for_each(op),
        Instr::Halt { id, .. } => id.into_iter().for_each(op),
        Instr::SetSlot { value, .. } => op(value),
        Instr::Jump { .. } | Instr::InvokeSeq { .. } | Instr::LoadCapture { .. } => {}
    }
}

/// How each register is used across the whole program.
struct Usage {
    defs: Vec<u32>,
    uses: Vec<u32>,
    /// Registers that something outside the instruction lists refers to:
    /// state, inputs, payloads, outputs, sequence calls. They keep their
    /// identity.
    pinned: Vec<bool>,
}

fn lists(code: &Code) -> impl Iterator<Item = &Vec<Instr>> {
    [&code.instrs, &code.post]
        .into_iter()
        .chain(code.events.iter().map(|e| &e.instrs))
}

fn usage(code: &Code) -> Usage {
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
    let mut pin = |r: u16| {
        u.pinned[r as usize] = true;
        u.uses[r as usize] += 1;
    };
    let pin_op = |o: Operand, pin: &mut dyn FnMut(u16)| {
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
    let lists = std::iter::once(&mut code.instrs)
        .chain(std::iter::once(&mut code.post))
        .chain(code.events.iter_mut().map(|e| &mut e.instrs));
    for list in lists {
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
                if touches
                    || targets[p + 1]
                    || matches!(i, Instr::Jump { .. } | Instr::JumpUnless { .. })
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
            retain(list, &remove);
        }
    }
    changed
}

/// Remove instructions whose only effect is a result nobody reads.
fn remove_dead(code: &mut Code) -> bool {
    let u = usage(code);
    let mut changed = false;
    let lists = std::iter::once(&mut code.instrs)
        .chain(std::iter::once(&mut code.post))
        .chain(code.events.iter_mut().map(|e| &mut e.instrs));
    for list in lists {
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
            retain(list, &remove);
            changed = true;
        }
    }
    changed
}

/// Drop the instructions marked in `remove`, moving jumps to the
/// instruction that now follows their old target.
fn retain(list: &mut Vec<Instr>, remove: &[bool]) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Op1, Op2};

    fn r(n: u16) -> Operand {
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
