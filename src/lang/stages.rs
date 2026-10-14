//! Which parts of the tick can run over a whole stretch of samples at once.
//!
//! The tick runs once per sample. Most of it does not need to: an
//! instruction can run over every sample of a stretch (a *segment*) in one
//! go, as long as nothing it depends on needs the previous sample to be
//! finished first. This module works out which instructions those are.
//!
//! Every instruction (or region of code with jumps inside, which runs as a
//! whole) is a node. Edges say what each read depends on:
//!
//! - **Same sample:** `b` reads what `a` wrote earlier in this tick. And a
//!   write may not overtake a read or write before it (it would change what
//!   that one sees).
//! - **Lagged:** `b` reads a value from an earlier sample. A register read
//!   before anything writes it in the tick sees the last sample's value
//!   (`state`); a delay line read sees what was written before.
//!
//! Nodes on a cycle need the previous sample done before the next: they
//! *loop*, sample by sample. So do nodes joined by a lagged edge (a value
//! carried from one sample to the next stays where it is, as now) and nodes
//! that loop by nature: vector blocks, noise, delay lines, and regions with
//! jumps. Everything else *streams*.
//!
//! The groups that result, in an order that respects every edge, make the
//! stages: consecutive streaming groups form one streaming stage, and
//! consecutive looping groups one looping stage. The program runs them
//! over segments of samples (see `Streams` in [`super::vm`]).

use std::collections::HashMap;

use super::opt::{reads, vector_dst, vector_regs, writes};
use super::vm::{Code, Instr, Reg, Source, VArg, VInstr};

/// How the tick runs in stages, or why it cannot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stages {
    /// Why the program runs sample by sample as a whole, if it must.
    pub fallback: Option<String>,
    pub stages: Vec<Stage>,
    /// Why instructions loop, with how many loop for each reason, most
    /// first.
    pub looping: Vec<(&'static str, usize)>,
}

/// A run of the tick's instructions of one kind.
#[derive(Clone, Debug, PartialEq)]
pub struct Stage {
    /// Whether every instruction runs over the whole segment in turn, or
    /// all of them once per sample.
    pub streams: bool,
    /// Positions in [`Code::instrs`], in order.
    pub instrs: Vec<u32>,
}

impl Stages {
    /// How many of the tick's instructions stream, and how many loop.
    pub fn counts(&self) -> (usize, usize) {
        let count = |streams: bool| {
            self.stages
                .iter()
                .filter(|s| s.streams == streams)
                .map(|s| s.instrs.len())
                .sum()
        };
        (count(true), count(false))
    }
}

/// One node: an instruction, or a region with jumps inside.
struct Node {
    /// Positions in the tick it covers.
    instrs: std::ops::Range<usize>,
    reads: Vec<Reg>,
    writes: Vec<Reg>,
    /// Delay lines it reads and writes.
    reads_lines: Vec<u32>,
    writes_lines: Vec<u32>,
    /// Loops whatever its edges say.
    loops: bool,
}

/// Work out the stages of `code`'s tick.
pub fn analyze(code: &Code) -> Stages {
    if let Some(why) = fallback(code) {
        return Stages {
            fallback: Some(why),
            ..Default::default()
        };
    }
    let list = &code.instrs;
    if list.is_empty() {
        return Stages::default();
    }
    let nodes = nodes(code);
    let n = nodes.len();

    // Registers written by vector blocks: what reads them is tied to the
    // block, which runs once per sample.
    let mut lane_written = std::collections::HashSet::new();
    for node in &nodes {
        if node.instrs.len() == 1
            && let Instr::Vector { .. } = list[node.instrs.start]
        {
            lane_written.extend(node.writes.iter().copied());
        }
    }

    // Edges: same-sample ones directed, lagged ones both ways (what they
    // join stays together).
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut lagged_self = vec![false; n];
    let mut writers: HashMap<Reg, Vec<usize>> = HashMap::new();
    for (k, node) in nodes.iter().enumerate() {
        for &r in &node.writes {
            writers.entry(r).or_default().push(k);
        }
    }
    for (k, node) in nodes.iter().enumerate() {
        for &r in &node.reads {
            let Some(ws) = writers.get(&r) else {
                // Never written in the tick: the same over a segment.
                continue;
            };
            let before = ws.iter().rev().find(|&&w| w < k).copied();
            let last = *ws.last().expect("a writer");
            match before {
                Some(w) => {
                    edges[w].push(k);
                    // A conditional write may not happen, leaving a value
                    // from an earlier sample.
                    if nodes[w].instrs.len() > 1 {
                        lag(&mut edges, &mut lagged_self, last, k);
                    }
                }
                None => lag(&mut edges, &mut lagged_self, last, k),
            }
            // The next write after this read waits for it.
            if let Some(&w) = ws.iter().find(|&&w| w > k) {
                edges[k].push(w);
            }
        }
        // Writes stay in order.
        for &r in &node.writes {
            if let Some(&w) = writers[&r].iter().find(|&&w| w > k) {
                edges[k].push(w);
            }
        }
    }
    // Delay lines: a read sees what earlier samples wrote.
    for (w, node) in nodes.iter().enumerate() {
        for line in &node.writes_lines {
            for (k, other) in nodes.iter().enumerate() {
                if other.reads_lines.contains(line) {
                    lag(&mut edges, &mut lagged_self, w, k);
                }
            }
        }
    }

    let group = sccs(&edges);
    let groups = group.iter().copied().max().map_or(0, |g| g + 1);
    let mut size = vec![0usize; groups];
    let mut loops = vec![false; groups];
    for k in 0..n {
        let g = group[k];
        size[g] += 1;
        let node = &nodes[k];
        loops[g] |=
            node.loops || lagged_self[k] || node.reads.iter().any(|r| lane_written.contains(r));
    }
    for g in 0..groups {
        loops[g] |= size[g] > 1;
    }
    // Why each looping instruction loops, for reports.
    let mut looping: HashMap<&'static str, usize> = HashMap::new();
    for (k, node) in nodes.iter().enumerate() {
        if !loops[group[k]] {
            continue;
        }
        let why = if node.instrs.len() > 1 {
            "in an `if`"
        } else if node.loops {
            match list[node.instrs.start] {
                Instr::Vector { .. } => "vector blocks",
                Instr::Noise { .. } => "noise",
                Instr::DelayRead { .. } | Instr::DelayWrite { .. } => "delay lines",
                _ => "doing something besides maths",
            }
        } else if lagged_self[k] {
            "reading their own last value"
        } else if node.reads.iter().any(|r| lane_written.contains(r)) {
            "reading vector blocks"
        } else {
            "in a cycle"
        };
        *looping.entry(why).or_default() += node.instrs.len();
    }
    let mut looping: Vec<(&'static str, usize)> = looping.into_iter().collect();
    looping.sort_by_key(|&(why, count)| (std::cmp::Reverse(count), why));

    // The groups in an order that respects every edge, keeping to one kind
    // as long as possible so stages are few.
    let mut next: Vec<std::collections::BTreeSet<usize>> = vec![Default::default(); groups];
    let mut waiting = vec![0usize; groups];
    for (a, out) in edges.iter().enumerate() {
        for &b in out {
            let (ga, gb) = (group[a], group[b]);
            if ga != gb && next[ga].insert(gb) {
                waiting[gb] += 1;
            }
        }
    }
    // Groups by where they start, so ties go in the tick's order.
    let mut start = vec![usize::MAX; groups];
    for k in 0..n {
        start[group[k]] = start[group[k]].min(k);
    }
    let mut ready: Vec<usize> = (0..groups).filter(|&g| waiting[g] == 0).collect();
    let mut order = Vec::with_capacity(groups);
    let mut kind = None;
    while !ready.is_empty() {
        let pick = ready
            .iter()
            .enumerate()
            .min_by_key(|&(_, &g)| (kind.is_some_and(|k| k != loops[g]), start[g]))
            .map(|(i, _)| i)
            .expect("not empty");
        let g = ready.swap_remove(pick);
        kind = Some(loops[g]);
        order.push(g);
        for &h in &next[g] {
            waiting[h] -= 1;
            if waiting[h] == 0 {
                ready.push(h);
            }
        }
    }
    debug_assert_eq!(order.len(), groups, "groups form no cycle");

    let mut members: Vec<Vec<usize>> = vec![Vec::new(); groups];
    for k in 0..n {
        members[group[k]].push(k);
    }
    let mut stages: Vec<Stage> = Vec::new();
    for g in order {
        let streams = !loops[g];
        let instrs = members[g]
            .iter()
            .flat_map(|&k| nodes[k].instrs.clone())
            .map(|i| i as u32);
        match stages.last_mut() {
            Some(s) if s.streams == streams => s.instrs.extend(instrs),
            _ => stages.push(Stage {
                streams,
                instrs: instrs.collect(),
            }),
        }
    }
    for s in &mut stages {
        s.instrs.sort_unstable();
    }
    Stages {
        fallback: None,
        stages,
        looping,
    }
}

/// Join `a` and `b` by a lagged edge.
fn lag(edges: &mut [Vec<usize>], lagged_self: &mut [bool], a: usize, b: usize) {
    if a == b {
        lagged_self[a] = true;
    } else {
        edges[a].push(b);
        edges[b].push(a);
    }
}

/// Why `code` has to run sample by sample as a whole, if it does.
fn fallback(code: &Code) -> Option<String> {
    if code.instrs.iter().any(|i| {
        matches!(
            i,
            Instr::InvokeSeq { .. }
                | Instr::InvokeEvent { .. }
                | Instr::EventArg { .. }
                | Instr::Halt { .. }
                | Instr::SetSlot { .. }
        )
    }) {
        return Some(
            "its tick starts, stops or sends something, which changes what the next \
             sample sees"
                .into(),
        );
    }
    let inputs: std::collections::HashSet<Reg> = code.input_regs.iter().copied().collect();
    let follows_computed = code.calls.iter().any(|c| {
        c.settings
            .iter()
            .any(|s| matches!(s, Source::Follow(r) if !inputs.contains(r)))
    });
    if !code.post.is_empty() || follows_computed {
        return Some(
            "a sequence follows a value it computes, so when its steps fall is only known \
             sample by sample"
                .into(),
        );
    }
    None
}

/// The tick's nodes: single instructions, and regions with jumps inside
/// (from a jump to the furthest place it or the jumps inside go).
fn nodes(code: &Code) -> Vec<Node> {
    let list = &code.instrs;
    let mut nodes = Vec::new();
    let mut i = 0;
    while i < list.len() {
        let mut end = i + 1;
        if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = list[i] {
            end = target as usize;
            let mut p = i;
            while p < end {
                if let Instr::Jump { target } | Instr::JumpUnless { target, .. } = list[p] {
                    end = end.max(target as usize);
                }
                p += 1;
            }
            end = end.min(list.len()).max(i + 1);
        }
        let mut node = Node {
            instrs: i..end,
            reads: Vec::new(),
            writes: Vec::new(),
            reads_lines: Vec::new(),
            writes_lines: Vec::new(),
            loops: end - i > 1,
        };
        for instr in &list[i..end] {
            visit(code, instr, &mut node);
        }
        node.reads.sort_unstable();
        node.reads.dedup();
        node.writes.sort_unstable();
        node.writes.dedup();
        nodes.push(node);
        i = end;
    }
    nodes
}

/// Add what `instr` reads and writes to `node`.
fn visit(code: &Code, instr: &Instr, node: &mut Node) {
    reads(instr, &mut |r| node.reads.push(r));
    writes(instr, &mut |r| node.writes.push(r));
    match *instr {
        // What a streaming stage can run; the rest loops.
        Instr::Op1 { .. }
        | Instr::Op2 { .. }
        | Instr::Copy { .. }
        | Instr::Select { .. }
        | Instr::Tune { .. }
        | Instr::Sum { .. } => {}
        Instr::Noise { .. } => node.loops = true,
        Instr::DelayRead { line, .. } => {
            node.loops = true;
            node.reads_lines.push(u32::from(line));
        }
        Instr::DelayWrite { line, .. } => {
            node.loops = true;
            node.writes_lines.push(u32::from(line));
        }
        Instr::Vector { block } => {
            node.loops = true;
            let block = &code.vectors[block as usize];
            let n = usize::from(block.lanes);
            for v in &block.instrs {
                vector_regs(v, &mut |a| match a {
                    VArg::Lanes(b) => node.reads.extend((0..n).map(|k| b + k as Reg)),
                    VArg::Scalar(r) => node.reads.push(r),
                    VArg::Const(_) => {}
                });
                if let Some(d) = vector_dst(v) {
                    node.writes.extend((0..n).map(|k| d + k as Reg));
                }
                match *v {
                    VInstr::DelayRead { line, stride, .. } => node
                        .reads_lines
                        .extend((0..n).map(|k| u32::from(line) + (k * usize::from(stride)) as u32)),
                    VInstr::DelayWrite { line, stride, .. } => node
                        .writes_lines
                        .extend((0..n).map(|k| u32::from(line) + (k * usize::from(stride)) as u32)),
                    _ => {}
                }
            }
        }
        _ => node.loops = true,
    }
}

/// The registers `instr` reads and writes, with every lane of a vector
/// block's.
pub(crate) fn touches(code: &Code, instr: &Instr) -> (Vec<Reg>, Vec<Reg>) {
    let mut node = Node {
        instrs: 0..0,
        reads: Vec::new(),
        writes: Vec::new(),
        reads_lines: Vec::new(),
        writes_lines: Vec::new(),
        loops: false,
    };
    visit(code, instr, &mut node);
    (node.reads, node.writes)
}

/// Strongly connected components (Tarjan's, without recursion): the group
/// of each node.
fn sccs(edges: &[Vec<usize>]) -> Vec<usize> {
    let n = edges.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut group = vec![usize::MAX; n];
    let mut groups = 0;
    let mut next = 0;
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // (node, next edge to look at)
        let mut work = vec![(root, 0usize)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut e)) = work.last_mut() {
            if let Some(&w) = edges[v].get(*e) {
                *e += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    low[parent] = low[parent].min(low[v]);
                }
                if low[v] == index[v] {
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        group[w] = groups;
                        if w == v {
                            break;
                        }
                    }
                    groups += 1;
                }
            }
        }
    }
    group
}
