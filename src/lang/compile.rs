//! Compiling one rill (or fn) instance to bytecode.
//!
//! The entry rill becomes one [`Code`]. Everything it calls is inlined, so
//! every call site gets its own `state` registers (each call is its own
//! instance) and the result is one flat program. Arguments known at build
//! time are folded into the code instead of becoming inputs.
//!
//! Function values are resolved here too: a fn passed as an argument is
//! inlined where it is called, and one chosen while playing becomes a branch
//! between the candidates. Nothing is called at run time.

use std::collections::{HashMap, HashSet};

use super::ast::*;
use super::check::Checked;
use super::diag::{Diagnostic, Span};
use super::types::{Signature, Size, Type};
use super::vm::{
    Code, EventCode, Handles, Instr, InvokeCall, Mode, Operand, SeqStep, SeqTable, Source, Tuning,
};
use crate::event::{EventDecl as Declared, EventId, EventKind};
use crate::ops::{Op1, Op2};

/// A compile-time value: one operand per channel. Frames nest, so a frame's
/// elements are values themselves.
#[derive(Clone, Debug, PartialEq)]
pub enum CVal {
    Scalar(Operand),
    Frame(Vec<CVal>),
    Event(Vec<(String, Operand)>),
    Fn(FnVal),
}

/// A function value, known at build time.
#[derive(Clone, Debug, PartialEq)]
pub enum FnVal {
    /// A user fn or built-in, by name.
    Named(String),
    /// An anonymous fn (by expression id) and the scopes it captured.
    Lambda {
        id: u32,
        captured: Vec<HashMap<String, Binding>>,
    },
    /// One of two functions, picked by `cond` while playing.
    Choice {
        cond: Operand,
        then: Box<FnVal>,
        els: Box<FnVal>,
    },
}

impl CVal {
    /// The value of an expression that has none (`()`).
    pub fn unit() -> CVal {
        CVal::Frame(Vec::new())
    }

    /// A flat frame of scalars.
    pub fn flat_frame(ops: Vec<Operand>) -> CVal {
        CVal::Frame(ops.into_iter().map(CVal::Scalar).collect())
    }

    /// Every operand, channels of nested frames in order.
    pub fn operands(&self) -> Vec<Operand> {
        let mut out = Vec::new();
        self.collect_operands(&mut out);
        out
    }

    fn collect_operands(&self, out: &mut Vec<Operand>) {
        match self {
            CVal::Scalar(o) => out.push(*o),
            CVal::Frame(xs) => xs.iter().for_each(|x| x.collect_operands(out)),
            CVal::Event(fields) => out.extend(fields.iter().map(|(_, o)| *o)),
            CVal::Fn(_) => {}
        }
    }

    fn scalar(&self) -> Operand {
        match self {
            CVal::Scalar(o) => *o,
            CVal::Frame(xs) => xs.first().map_or(Operand::Const(0.0), CVal::scalar),
            CVal::Event(_) | CVal::Fn(_) => Operand::Const(0.0),
        }
    }

    /// Same shape as `self`, with new operands in [`CVal::operands`] order.
    fn reshape(&self, ops: Vec<Operand>) -> CVal {
        self.reshape_from(&mut ops.into_iter())
    }

    fn reshape_from(&self, ops: &mut impl Iterator<Item = Operand>) -> CVal {
        let mut next = || ops.next().expect("as many operands as the shape has");
        match self {
            CVal::Scalar(_) => CVal::Scalar(next()),
            CVal::Frame(xs) => CVal::Frame(xs.iter().map(|x| x.reshape_from(ops)).collect()),
            CVal::Event(fields) => CVal::Event(
                fields
                    .iter()
                    .map(|(name, _)| (name.clone(), ops.next().expect("one per field")))
                    .collect(),
            ),
            CVal::Fn(f) => CVal::Fn(f.clone()),
        }
    }

    /// How many frame layers wrap the scalars.
    fn depth(&self) -> usize {
        match self {
            CVal::Frame(xs) => 1 + xs.first().map_or(0, CVal::depth),
            _ => 0,
        }
    }
}

/// How many frame layers a type has.
fn type_depth(t: &Type) -> usize {
    match t {
        Type::Frame(elem, _) => 1 + type_depth(elem),
        _ => 0,
    }
}

/// Every fn and rill by name, and every anonymous fn by expression id.
pub struct Defs<'a> {
    map: HashMap<&'a str, (&'a Def, &'a Signature)>,
    lambdas: HashMap<u32, &'a Expr>,
    /// Declared events by name: their id and kind.
    events: HashMap<&'a str, (EventId, EventKind)>,
    decls: Vec<Declared>,
    /// The program's sequences, filled in by [`seq_tables`].
    pub seqs: Vec<SeqTable>,
}

impl<'a> Defs<'a> {
    pub fn new(program: &'a Program, checked: &'a Checked) -> Defs<'a> {
        let defs = program.items.iter().map(Item::def);
        let mut lambdas = HashMap::new();
        for d in program.items.iter().map(Item::def) {
            for p in &d.params {
                if let Some(e) = &p.default {
                    collect_lambdas(e, &mut lambdas);
                }
            }
            collect_lambdas_in_block(&d.body, &mut lambdas);
        }
        let events = checked
            .events
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                d.as_ref()
                    .map(|d| (d.name.as_str(), (EventId(i as u16), d.kind)))
            })
            .collect();
        Defs {
            events,
            decls: checked.events.iter().flatten().cloned().collect(),
            seqs: Vec::new(),
            map: defs
                .zip(&checked.signatures)
                .map(|(d, s)| (d.name.name.as_str(), (d, s)))
                .collect(),
            lambdas,
        }
    }

    pub fn get(&self, name: &str) -> Option<(&'a Def, &'a Signature)> {
        self.map.get(name).copied()
    }

    fn lambda(&self, id: u32) -> Option<&'a Expr> {
        self.lambdas.get(&id).copied()
    }
}

fn collect_lambdas<'a>(e: &'a Expr, out: &mut HashMap<u32, &'a Expr>) {
    match &e.kind {
        ExprKind::Fn { body, .. } => {
            out.insert(e.id, e);
            collect_lambdas_in_block(body, out);
        }
        ExprKind::Unary(_, x) | ExprKind::Field(x, _) | ExprKind::Cast(x, _) => {
            collect_lambdas(x, out)
        }
        ExprKind::Binary(_, a, b) | ExprKind::Index(a, b) => {
            collect_lambdas(a, out);
            collect_lambdas(b, out);
        }
        ExprKind::Range { start, end, .. } => {
            collect_lambdas(start, out);
            collect_lambdas(end, out);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                collect_lambdas(&a.value, out);
            }
        }
        ExprKind::If { cond, then, els } => {
            collect_lambdas(cond, out);
            collect_lambdas_in_block(then, out);
            if let Some(els) = els {
                collect_lambdas(els, out);
            }
        }
        ExprKind::Block(b) => collect_lambdas_in_block(b, out),
        ExprKind::Frame(xs) => {
            for x in xs {
                collect_lambdas(x, out);
            }
        }
        ExprKind::Number { .. } | ExprKind::Bool(_) | ExprKind::Name(_) => {}
        ExprKind::Repeat(x, _) => collect_lambdas(x, out),
        ExprKind::Invoke { step, id, args, .. } => {
            for x in step.iter().chain(id.iter()) {
                collect_lambdas(x, out);
            }
            for a in args {
                collect_lambdas(&a.value, out);
            }
        }
        ExprKind::Halt { id, .. } => {
            if let Some(x) = id {
                collect_lambdas(x, out);
            }
        }
    }
}

fn collect_lambdas_in_block<'a>(b: &'a Block, out: &mut HashMap<u32, &'a Expr>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { value: Some(e), .. }
            | Stmt::State { init: e, .. }
            | Stmt::Assign { value: e, .. }
            | Stmt::Return { value: e, .. }
            | Stmt::Expr(e) => collect_lambdas(e, out),
            Stmt::Let { value: None, .. } => {}
            Stmt::EventHandler { body, .. } => collect_lambdas_in_block(body, out),
            Stmt::For { iter, body, .. } => {
                collect_lambdas(iter, out);
                collect_lambdas_in_block(body, out);
            }
        }
    }
}

/// How an argument reaches a compiled instance.
#[derive(Clone, Debug, PartialEq)]
pub enum ArgSpec {
    /// Known at build time; folded into the code.
    Const(CVal),
    /// The parameter's default value, folded into the code.
    Default,
    /// A stream: one node input per channel. `None` means a scalar.
    Stream(Option<usize>),
}

/// Compile `def` called with `args` (one per parameter, defaults filled
/// in) into a standalone program.
pub fn compile_instance(
    defs: &Defs,
    types: &[Type],
    sample_rate: f32,
    def: &Def,
    sig: &Signature,
    args: &[ArgSpec],
) -> Result<Code, Diagnostic> {
    let mut c = Compiler::new(defs, types, sample_rate, def.name.span);
    let mut input_regs = Vec::new();
    let mut vals = Vec::new();
    for (a, p) in args.iter().zip(&def.params) {
        vals.push(match a {
            ArgSpec::Const(v) => v.clone(),
            ArgSpec::Default => {
                let d = p.default.as_ref().ok_or_else(|| {
                    internal(p.name.span, &format!("`{}` has no default", p.name.name))
                })?;
                c.scopes = vec![HashMap::new()];
                let v = c.expr(d)?;
                c.scopes.clear();
                v
            }
            ArgSpec::Stream(None) => {
                let r = c.reg()?;
                input_regs.push(r);
                CVal::Scalar(Operand::Reg(r))
            }
            ArgSpec::Stream(Some(n)) => {
                let mut ops = Vec::new();
                for _ in 0..*n {
                    let r = c.reg()?;
                    input_regs.push(r);
                    ops.push(Operand::Reg(r));
                }
                CVal::flat_frame(ops)
            }
        });
    }
    let out = c.inline(def, sig, vals, HashMap::new())?;
    Ok(Code {
        instrs: c.code,
        post: c.post,
        regs: c.regs as usize,
        input_regs,
        output: out.operands(),
        state_init: c.state_init,
        events: c.events,
        decls: defs.decls.clone(),
        seqs: defs.seqs.clone(),
        calls: c.calls,
        pools: c.pools,
    })
}

/// Build every sequence's table: its settings and steps, all constants.
pub fn seq_tables(
    defs: &Defs,
    types: &[Type],
    sample_rate: f32,
    program: &Program,
) -> Result<Vec<SeqTable>, Diagnostic> {
    let mut tables = Vec::new();
    for seq in &program.seqs {
        let mut c = Compiler::new(defs, types, sample_rate, seq.name.span);
        c.scopes = vec![HashMap::new()];
        let mut table = SeqTable {
            name: seq.name.name.clone(),
            step_beats: 0.0,
            steps: Vec::new(),
            settings: [2.0, 0.9, 0.8],
            repeat: 1,
            looping: false,
            instances: 64,
        };
        // A beat is a `1/beat` note; a step is `n/d` of a whole note.
        let mut beat = 4u32;
        let mut step = (1u32, 8u32);
        for setting in &seq.settings {
            let name = setting.name.name.as_str();
            let fraction = super::check::fraction(&setting.value);
            match (name, fraction) {
                ("meter", Some((_, d))) => beat = d,
                ("step", Some(f)) => step = f,
                ("meter" | "step", None) => {}
                _ => {
                    let value = c.constant(&setting.value)?;
                    match name {
                        "tempo" if value <= 0.0 => return Err(zero_tempo(setting.value.span)),
                        "tempo" => table.settings[super::vm::TEMPO] = value,
                        "gate" => table.settings[super::vm::GATE] = value.clamp(1e-6, 1.0),
                        "velocity" => table.settings[super::vm::VELOCITY] = value.clamp(0.0, 1.0),
                        "repeat" => table.repeat = value.max(1.0) as u32,
                        "loop" => table.looping = value != 0.0,
                        "instances" => table.instances = value.clamp(1.0, 65_535.0) as u16,
                        _ => {}
                    }
                }
            }
        }
        table.step_beats = f64::from(step.0) * f64::from(beat) / f64::from(step.1);
        for st in &seq.steps {
            let pitches = match &st.notes {
                Some(notes) => {
                    let v = c.expr(notes)?;
                    v.operands()
                        .into_iter()
                        .map(|o| match o {
                            Operand::Const(p) => Ok(p),
                            Operand::Reg(_) => {
                                Err(Diagnostic::error(notes.span, "a step must be a constant"))
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?
                }
                None => Vec::new(),
            };
            let velocity = match &st.velocity {
                Some(v) => Some(c.constant(v)?.clamp(0.0, 1.0)),
                None => None,
            };
            table.steps.push(SeqStep { pitches, velocity });
        }
        tables.push(table);
    }
    Ok(tables)
}

/// Evaluate one parameter default at build time.
pub fn default_value(
    defs: &Defs,
    types: &[Type],
    sample_rate: f32,
    def: &Def,
    param: usize,
) -> Result<CVal, Diagnostic> {
    let default = def.params[param].default.as_ref().ok_or_else(|| {
        internal(
            def.params[param].name.span,
            &format!("`{}` has no default", def.params[param].name.name),
        )
    })?;
    let mut c = Compiler::new(defs, types, sample_rate, def.name.span);
    c.scopes = vec![HashMap::new()];
    c.expr(default)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    val: CVal,
    /// `state`, which can be assigned to.
    mutable: bool,
}

/// Where `return` sends its value in the body being inlined.
struct Ret {
    /// Registers holding the result, allocated by the first `return` that
    /// needs them.
    val: Option<CVal>,
    /// Jumps to patch to the end of the body.
    patches: Vec<usize>,
    /// Branch depth of the body itself. A `return` at this depth is the
    /// body's last statement and needs no jump.
    depth: u32,
    /// The result when the only `return` is that last statement.
    direct: Option<CVal>,
}

struct Compiler<'a> {
    defs: &'a Defs<'a>,
    types: &'a [Type],
    sample_rate: f32,
    code: Vec<Instr>,
    events: Vec<EventCode>,
    /// Definitions and anonymous fns currently being inlined, innermost
    /// last. Meeting one again means recursion through a function value.
    inlining: Vec<String>,
    regs: u16,
    state_init: Vec<(u16, f32)>,
    state_regs: HashSet<u16>,
    scopes: Vec<HashMap<String, Binding>>,
    rets: Vec<Ret>,
    /// How many `if` branches deep the current code is.
    depth: u32,
    /// For internal errors: the definition being compiled.
    span: Span,
    /// Code run every tick after the main code: settings of playing
    /// sequences that follow streams.
    post: Vec<Instr>,
    calls: Vec<InvokeCall>,
    /// Voice pools: per pool, per copy, the copy's output operands.
    pools: Vec<Vec<Vec<Operand>>>,
    /// The voice pool and copy being compiled, innermost.
    voice: Option<(u16, u16)>,
    /// Inside a handler: the first scope that belongs to it. Names bound
    /// from there on are the handler's own.
    handler_scope: Option<usize>,
}

type CResult<T> = Result<T, Diagnostic>;

fn internal(span: Span, what: &str) -> Diagnostic {
    Diagnostic::error(span, format!("internal compiler error: {what}"))
}

impl<'a> Compiler<'a> {
    fn new(defs: &'a Defs<'a>, types: &'a [Type], sample_rate: f32, span: Span) -> Compiler<'a> {
        Compiler {
            defs,
            types,
            sample_rate,
            code: Vec::new(),
            events: Vec::new(),
            inlining: Vec::new(),
            regs: 0,
            state_init: Vec::new(),
            state_regs: HashSet::new(),
            scopes: Vec::new(),
            rets: Vec::new(),
            depth: 0,
            span,
            post: Vec::new(),
            calls: Vec::new(),
            pools: Vec::new(),
            voice: None,
            handler_scope: None,
        }
    }

    /// A constant scalar, folded at build time.
    fn constant(&mut self, e: &Expr) -> CResult<f32> {
        match self.expr(e)?.scalar() {
            Operand::Const(c) => Ok(c),
            Operand::Reg(_) => Err(Diagnostic::error(e.span, "this must be a constant")),
        }
    }

    fn reg(&mut self) -> CResult<u16> {
        let r = self.regs;
        self.regs = self
            .regs
            .checked_add(1)
            .ok_or_else(|| Diagnostic::error(self.span, "this rill is too large to compile"))?;
        Ok(r)
    }

    fn emit(&mut self, instr: Instr) -> usize {
        self.code.push(instr);
        self.code.len() - 1
    }

    /// Point the jump at `at` to the next instruction.
    fn patch(&mut self, at: usize) {
        let here = self.code.len() as u32;
        match &mut self.code[at] {
            Instr::Jump { target } | Instr::JumpUnless { target, .. } => *target = here,
            _ => unreachable!("patching a non-jump"),
        }
    }

    fn op2(&mut self, op: Op2, a: Operand, b: Operand) -> CResult<Operand> {
        if let (Operand::Const(a), Operand::Const(b)) = (a, b) {
            return Ok(Operand::Const(op.apply(a, b)));
        }
        let dst = self.reg()?;
        self.emit(Instr::Op2 { op, dst, a, b });
        Ok(Operand::Reg(dst))
    }

    fn tune(
        &mut self,
        tuning: Tuning,
        pitch: Operand,
        setting: Operand,
        a4: Operand,
    ) -> CResult<Operand> {
        if let (Operand::Const(p), Operand::Const(s), Operand::Const(a)) = (pitch, setting, a4) {
            return Ok(Operand::Const(tuning.frequency(p, s, a)));
        }
        let dst = self.reg()?;
        self.emit(Instr::Tune {
            tuning,
            dst,
            pitch,
            setting,
            a4,
        });
        Ok(Operand::Reg(dst))
    }

    fn op1(&mut self, op: Op1, x: Operand) -> CResult<Operand> {
        if let Operand::Const(x) = x {
            return Ok(Operand::Const(op.apply(x, self.sample_rate)));
        }
        let dst = self.reg()?;
        self.emit(Instr::Op1 { op, dst, x });
        Ok(Operand::Reg(dst))
    }

    /// Element-wise `op`. Two frames pair up element by element; a value
    /// with fewer layers applies to every element of the other, so it lines
    /// up with the outer layers.
    fn zip2(&mut self, op: Op2, a: &CVal, b: &CVal) -> CResult<CVal> {
        match (a, b) {
            (CVal::Frame(xs), CVal::Frame(ys)) => {
                if xs.len() != ys.len() {
                    return Err(internal(
                        self.span,
                        "frames of different sizes reached an operator",
                    ));
                }
                let mut out = Vec::with_capacity(xs.len());
                for (x, y) in xs.iter().zip(ys) {
                    out.push(self.zip2(op, x, y)?);
                }
                Ok(CVal::Frame(out))
            }
            (CVal::Frame(xs), _) => {
                let mut out = Vec::with_capacity(xs.len());
                for x in xs {
                    out.push(self.zip2(op, x, b)?);
                }
                Ok(CVal::Frame(out))
            }
            (_, CVal::Frame(ys)) => {
                let mut out = Vec::with_capacity(ys.len());
                for y in ys {
                    out.push(self.zip2(op, a, y)?);
                }
                Ok(CVal::Frame(out))
            }
            _ => Ok(CVal::Scalar(self.op2(op, a.scalar(), b.scalar())?)),
        }
    }

    fn map1(&mut self, op: Op1, x: &CVal) -> CResult<CVal> {
        self.map_operands(x, |s, o| s.op1(op, o))
    }

    /// Apply `f` to every operand of `x`, keeping its shape.
    fn map_operands(
        &mut self,
        x: &CVal,
        mut f: impl FnMut(&mut Self, Operand) -> CResult<Operand>,
    ) -> CResult<CVal> {
        let mut out = Vec::new();
        for o in x.operands() {
            out.push(f(self, o)?);
        }
        Ok(x.reshape(out))
    }

    /// Fold the outer layer of a frame with `op`. For a nested frame that
    /// combines the inner frames element by element.
    fn reduce(&mut self, op: Op2, x: &CVal) -> CResult<CVal> {
        let CVal::Frame(xs) = x else {
            return Ok(x.clone());
        };
        let Some((first, rest)) = xs.split_first() else {
            return Err(internal(self.span, "reducing an empty frame"));
        };
        let mut acc = first.clone();
        for x in rest {
            acc = self.zip2(op, &acc, x)?;
        }
        Ok(acc)
    }

    /// Copy `v` into fresh registers if it reads any `state`, so later
    /// assignments to the state do not change it.
    fn detach(&mut self, v: CVal) -> CResult<CVal> {
        if !v
            .operands()
            .iter()
            .any(|o| matches!(o, Operand::Reg(r) if self.state_regs.contains(r)))
        {
            return Ok(v);
        }
        let mut out = Vec::new();
        for o in v.operands() {
            match o {
                Operand::Reg(r) if self.state_regs.contains(&r) => {
                    let dst = self.reg()?;
                    self.emit(Instr::Copy { dst, src: o });
                    out.push(Operand::Reg(dst));
                }
                o => out.push(o),
            }
        }
        Ok(v.reshape(out))
    }

    /// Copy `v` into `slot`, allocating registers of the same shape the
    /// first time.
    fn copy_into(&mut self, slot: &mut Option<CVal>, v: &CVal) -> CResult<()> {
        // A function has no registers to copy into. `if` builds a choice
        // between functions itself; several `return`s must agree.
        if let CVal::Fn(f) = v {
            return match slot {
                None => {
                    *slot = Some(v.clone());
                    Ok(())
                }
                Some(CVal::Fn(prev)) if prev == f => Ok(()),
                Some(_) => Err(Diagnostic::error(
                    self.span,
                    "returning different functions from different branches is not supported yet",
                )
                .with_help(
                    "choose with an expression instead, as in `return if c { f } else { g }`",
                )),
            };
        }
        if slot.is_none() {
            let mut regs = Vec::new();
            for _ in v.operands() {
                regs.push(Operand::Reg(self.reg()?));
            }
            *slot = Some(v.reshape(regs));
        }
        let dsts = slot.as_ref().unwrap().operands();
        for (d, src) in dsts.iter().zip(v.operands()) {
            let Operand::Reg(dst) = *d else {
                unreachable!()
            };
            self.emit(Instr::Copy { dst, src });
        }
        Ok(())
    }

    fn bind(&mut self, name: &str, val: CVal, mutable: bool) {
        self.scopes
            .last_mut()
            .expect("a scope is open")
            .insert(name.to_owned(), Binding { val, mutable });
    }

    /// Bind the size parameters in `ty` (such as `N` in `[Sample; N]`) to
    /// the sizes of `val`.
    fn bind_sizes(&mut self, ty: &Type, val: &CVal) {
        if let (Type::Frame(elem, size), CVal::Frame(xs)) = (ty, val) {
            if let Size::Var(v) = size {
                self.bind(v, CVal::Scalar(Operand::Const(xs.len() as f32)), false);
            }
            if let Some(x) = xs.first() {
                self.bind_sizes(elem, x);
            }
        }
    }

    fn zero_for_type(&self, ty: &TypeExpr) -> CResult<CVal> {
        match ty {
            TypeExpr::Named(_) => Ok(CVal::Scalar(Operand::Const(0.0))),
            TypeExpr::Frame { elem, size, .. } => {
                let n = match size {
                    SizeExpr::Lit(n, _) => *n,
                    SizeExpr::Var(id) => {
                        let Some(binding) = self.lookup(&id.name) else {
                            return Err(internal(id.span, "unknown frame size"));
                        };
                        let Operand::Const(n) = binding.val.scalar() else {
                            return Err(internal(id.span, "frame size is not constant"));
                        };
                        n as u32
                    }
                };
                let elem = self.zero_for_type(elem)?;
                Ok(CVal::Frame((0..n).map(|_| elem.clone()).collect()))
            }
            TypeExpr::Fn { span, .. } => Err(internal(*span, "cannot zero-initialize a function")),
        }
    }

    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut Binding> {
        self.scopes.iter_mut().rev().find_map(|s| s.get_mut(name))
    }

    fn rebind(&mut self, name: &str, val: CVal) {
        if let Some(binding) = self.lookup_mut(name) {
            binding.val = val;
        }
    }

    fn rebind_index(&mut self, name: &str, index: usize, val: CVal) -> CResult<()> {
        let Some(binding) = self.lookup_mut(name) else {
            return Err(internal(self.span, "assignment to an unknown name"));
        };
        let CVal::Frame(xs) = &mut binding.val else {
            return Err(internal(self.span, "index assignment to a non-frame"));
        };
        let Some(slot) = xs.get_mut(index) else {
            return Err(internal(self.span, "index assignment out of range"));
        };
        *slot = val;
        Ok(())
    }

    // ---- bodies ---------------------------------------------------------

    /// Compile `def`'s body in place with `args` bound to its parameters,
    /// and return its result.
    fn inline(
        &mut self,
        def: &Def,
        sig: &Signature,
        args: Vec<CVal>,
        sizes: HashMap<String, CVal>,
    ) -> CResult<CVal> {
        self.enter(&def.name.name, def.name.span)?;
        let result = self.inline_body(def, sig, args, sizes);
        self.inlining.pop();
        result
    }

    fn inline_body(
        &mut self,
        def: &Def,
        sig: &Signature,
        args: Vec<CVal>,
        sizes: HashMap<String, CVal>,
    ) -> CResult<CVal> {
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        let saved_span = std::mem::replace(&mut self.span, def.name.span);

        for (name, val) in sizes {
            self.bind(&name, val, false);
        }
        for (p, a) in sig.params.iter().zip(&args) {
            self.bind_sizes(&p.ty, a);
        }
        for (p, a) in sig.params.iter().zip(args) {
            self.bind(&p.name, a, sig.kind == super::types::DefKind::Rill);
        }

        self.rets.push(Ret {
            val: None,
            patches: Vec::new(),
            depth: self.depth,
            direct: None,
        });
        let (value, diverged) = self.block_in_current_scope(&def.body)?;
        if !diverged && let Some(v) = value {
            // A fn's trailing expression is its result.
            self.ret(v)?;
        }
        if matches!(sig.kind, super::types::DefKind::Rill) {
            for stmt in &def.body.stmts {
                if let Stmt::EventHandler {
                    name,
                    params,
                    mode,
                    body,
                    ..
                } = stmt
                {
                    self.event_handler(name, params, mode, body)?;
                }
            }
        }
        let ret = self.rets.pop().expect("pushed above");
        for at in ret.patches {
            self.patch(at);
        }

        self.scopes = saved_scopes;
        self.span = saved_span;
        ret.direct
            .or(ret.val)
            .ok_or_else(|| internal(def.name.span, "body produced no value"))
    }

    fn ret(&mut self, v: CVal) -> CResult<()> {
        let depth = self.depth;
        let mut ret = self.rets.pop().expect("inside a body");
        if depth == ret.depth && ret.val.is_none() {
            // The body's last statement, and nothing returned before it.
            ret.direct = Some(v);
        } else {
            let mut slot = ret.val.take();
            self.copy_into(&mut slot, &v)?;
            ret.val = slot;
            if depth != ret.depth {
                ret.patches.push(self.emit(Instr::Jump { target: 0 }));
            }
        }
        self.rets.push(ret);
        Ok(())
    }

    /// Returns the block's value, if it ends in an expression, and whether
    /// every path through it returned.
    fn block(&mut self, b: &Block) -> CResult<(Option<CVal>, bool)> {
        self.scopes.push(HashMap::new());
        let result = self.block_in_current_scope(b);
        self.scopes.pop();
        result
    }

    fn block_in_current_scope(&mut self, b: &Block) -> CResult<(Option<CVal>, bool)> {
        let mut value = None;
        let mut diverged = false;
        for s in &b.stmts {
            let (v, d) = self.stmt(s)?;
            value = v;
            diverged |= d;
        }
        Ok(if diverged {
            (None, true)
        } else {
            (value, false)
        })
    }

    fn stmt(&mut self, s: &Stmt) -> CResult<(Option<CVal>, bool)> {
        match s {
            Stmt::Let {
                name, ty, value, ..
            } => {
                let v = match value {
                    Some(value) => self.expr(value)?,
                    None => self.zero_for_type(ty.as_ref().ok_or_else(|| {
                        internal(name.span, "uninitialized local without a type")
                    })?)?,
                };
                let v = self.detach(v)?;
                self.bind(&name.name, v, false);
                Ok((None, false))
            }
            Stmt::State { name, init, .. } => {
                let v = self.expr(init)?;
                let mut regs = Vec::new();
                for o in v.operands() {
                    let Operand::Const(c) = o else {
                        return Err(Diagnostic::error(
                            init.span,
                            "the initial value of `state` must be a constant",
                        ));
                    };
                    let r = self.reg()?;
                    self.state_init.push((r, c));
                    self.state_regs.insert(r);
                    regs.push(Operand::Reg(r));
                }
                let v = v.reshape(regs);
                self.bind(&name.name, v, true);
                Ok((None, false))
            }
            Stmt::Assign { target, value, .. } => {
                let v = self.expr(value)?;
                // Detach first so `s = [s[1], s[0]]` reads the old values.
                let v = self.detach(v)?;
                match target {
                    AssignTarget::Name(name) => match self.lookup(&name.name) {
                        Some(b) if b.mutable => {
                            let dsts = b.val.operands();
                            for (d, src) in dsts.iter().zip(v.operands()) {
                                if let Operand::Reg(dst) = *d {
                                    self.emit(Instr::Copy { dst, src });
                                }
                            }
                        }
                        Some(_) => self.rebind(&name.name, v),
                        None => return Err(internal(name.span, "assignment to an unknown name")),
                    },
                    AssignTarget::Index { base, index, .. } => {
                        let i = self.expr(index)?.scalar();
                        let Some(b) = self.lookup(&base.name) else {
                            return Err(internal(base.span, "assignment to an unknown name"));
                        };
                        let mutable = b.mutable;
                        let CVal::Frame(xs) = &b.val else {
                            return Err(internal(base.span, "index assignment to a non-frame"));
                        };
                        let Operand::Const(i) = i else {
                            return Err(Diagnostic::error(
                                index.span,
                                "assigned channel index must be known before audio starts",
                            ));
                        };
                        let i = i as usize;
                        let dsts = xs
                            .get(i)
                            .ok_or_else(|| {
                                Diagnostic::error(
                                    index.span,
                                    format!("channel {i} is out of range"),
                                )
                            })?
                            .operands();
                        if mutable {
                            for (d, src) in dsts.iter().zip(v.operands()) {
                                if let Operand::Reg(dst) = *d {
                                    self.emit(Instr::Copy { dst, src });
                                }
                            }
                        } else {
                            self.rebind_index(&base.name, i, v)?;
                        }
                    }
                };
                Ok((None, false))
            }
            Stmt::Return { value, .. } => {
                let v = self.expr(value)?;
                self.ret(v)?;
                Ok((None, true))
            }
            Stmt::EventHandler { .. } => Ok((None, false)),
            Stmt::For {
                name, iter, body, ..
            } => {
                let values = self.loop_values(iter)?;
                let mut returned = false;
                for value in values {
                    self.scopes.push(HashMap::new());
                    self.bind(&name.name, value, false);
                    let (_, diverged) = self.branch(|s| s.block(body))?;
                    self.scopes.pop();
                    if diverged {
                        returned = true;
                        break;
                    }
                }
                Ok((None, returned))
            }
            Stmt::Expr(e) => {
                let v = self.expr(e)?;
                Ok((Some(v), self.types[e.id as usize] == Type::Never))
            }
        }
    }

    // ---- expressions ----------------------------------------------------

    fn expr(&mut self, e: &Expr) -> CResult<CVal> {
        match &e.kind {
            ExprKind::Number { value, .. } => Ok(CVal::Scalar(Operand::Const(*value as f32))),
            ExprKind::Bool(b) => Ok(CVal::Scalar(Operand::Const(f32::from(u8::from(*b))))),
            ExprKind::Name(name) => {
                if let Some(b) = self.lookup(name) {
                    return Ok(b.val.clone());
                }
                if let Some(p) = super::check::pitch_literal(name) {
                    return Ok(CVal::Scalar(Operand::Const(p)));
                }
                if let Some(c) = constant(name, self.sample_rate) {
                    return Ok(CVal::Scalar(Operand::Const(c)));
                }
                if self.defs.get(name).is_some() || !super::builtins::lookup(name).is_empty() {
                    return Ok(CVal::Fn(FnVal::Named(name.clone())));
                }
                Err(internal(e.span, &format!("unknown name `{name}`")))
            }
            ExprKind::Cast(x, _) => {
                let v = self.expr(x)?;
                // Only `as Int` changes the value: it drops the fraction.
                let to_int = self.types[e.id as usize] == Type::Int
                    && self.types[x.id as usize] != Type::Int;
                if to_int {
                    self.map1(Op1::Trunc, &v)
                } else {
                    Ok(v)
                }
            }
            ExprKind::Fn { .. } => {
                // Capture by value: `state` read now keeps this tick's value.
                let mut captured = self.scopes.clone();
                for scope in &mut captured {
                    for binding in scope.values_mut() {
                        if binding.mutable {
                            binding.val = self.detach(binding.val.clone())?;
                            binding.mutable = false;
                        }
                    }
                }
                Ok(CVal::Fn(FnVal::Lambda { id: e.id, captured }))
            }
            ExprKind::Unary(op, x) => {
                let v = self.expr(x)?;
                match op {
                    UnOp::Plus => Ok(v),
                    // `-6dB`: gains are factors, so negating one inverts it.
                    UnOp::Neg if is_gain(&self.types[x.id as usize]) => {
                        self.zip2(Op2::Div, &CVal::Scalar(Operand::Const(1.0)), &v)
                    }
                    UnOp::Neg => self.map1(Op1::Neg, &v),
                    UnOp::Not => self.map1(Op1::Not, &v),
                }
            }
            ExprKind::Binary(op, a, b) => {
                let va = self.expr(a)?;
                let vb = self.expr(b)?;
                let (ga, gb) = (
                    is_gain(&self.types[a.id as usize]),
                    is_gain(&self.types[b.id as usize]),
                );
                if ga || gb {
                    return self.gain_binary(*op, &va, &vb, ga, gb, &self.types[e.id as usize]);
                }
                let (ta, tb) = (&self.types[a.id as usize], &self.types[b.id as usize]);
                if is_freq(&self.types[e.id as usize]) && is_freq(ta) && is_interval(tb) {
                    return self.freq_interval_binary(*op, &va, &vb);
                }
                let op = op2_for(*op, &self.types[e.id as usize]);
                self.zip2(op, &va, &vb)
            }
            ExprKind::Range { .. } => Err(Diagnostic::error(
                e.span,
                "a range can only be used as a `for` loop iterable",
            )),
            ExprKind::Call {
                callee,
                sizes,
                args,
                ..
            } => self.call(e, callee, sizes, args),
            ExprKind::If { cond, then, els } => self.if_expr(e, cond, then, els.as_deref()),
            ExprKind::Block(b) => Ok(self.block(b)?.0.unwrap_or_else(CVal::unit)),
            ExprKind::Frame(elems) => {
                let mut xs = Vec::new();
                for el in elems {
                    xs.push(self.expr(el)?);
                }
                Ok(CVal::Frame(xs))
            }
            ExprKind::Index(base, index) => {
                let b = self.expr(base)?;
                let i = self.expr(index)?.scalar();
                let CVal::Frame(xs) = b else {
                    return Err(internal(base.span, "indexing a non-frame value"));
                };
                if let Operand::Const(i) = i {
                    return xs.get(i as usize).cloned().ok_or_else(|| {
                        Diagnostic::error(index.span, format!("channel {i} is out of range"))
                    });
                }
                // Unknown index: pick with a chain of selects, one per
                // channel of the element. Out of range reads element 0.
                let mut result = None;
                self.copy_into(&mut result, &xs[0])?;
                let result = result.expect("copied above");
                let dsts = result.operands();
                for (c, x) in xs.iter().enumerate().skip(1) {
                    let hit = self.op2(Op2::Eq, i, Operand::Const(c as f32))?;
                    for (d, o) in dsts.iter().zip(x.operands()) {
                        let Operand::Reg(dst) = *d else {
                            unreachable!("copy_into allocates registers")
                        };
                        self.emit(Instr::Select {
                            dst,
                            cond: hit,
                            a: o,
                            b: *d,
                        });
                    }
                }
                Ok(result)
            }
            ExprKind::Repeat(x, n) => self.repeat(x, *n),
            ExprKind::Invoke {
                step,
                id,
                target,
                args,
            } => self.invoke(e, step.as_deref(), id.as_deref(), target, args),
            ExprKind::Halt { id, target } => {
                let seq = self
                    .defs
                    .seqs
                    .iter()
                    .position(|t| t.name == target.name)
                    .ok_or_else(|| internal(target.span, "unknown sequence"))?;
                let id = match id {
                    Some(id) => Some(self.expr(id)?.scalar()),
                    None => None,
                };
                self.emit(Instr::Halt {
                    seq: seq as u16,
                    id,
                });
                Ok(CVal::unit())
            }
            ExprKind::Field(base, field) => {
                let CVal::Event(fields) = self.expr(base)? else {
                    return Err(internal(e.span, "field access on a non-event value"));
                };
                fields
                    .iter()
                    .find(|(name, _)| name == &field.name)
                    .map(|(_, op)| CVal::Scalar(*op))
                    .ok_or_else(|| {
                        internal(field.span, &format!("unknown event field `{}`", field.name))
                    })
            }
        }
    }

    fn event_handler(
        &mut self,
        name: &Ident,
        params: &[Ident],
        mode: &HandlerMode,
        body: &Block,
    ) -> CResult<()> {
        let saved_code = std::mem::take(&mut self.code);
        let saved_scopes = self.scopes.clone();
        self.scopes.push(HashMap::new());
        let saved_handler = self.handler_scope.replace(self.scopes.len() - 1);
        let (handles, kind) = if name.name == "start" {
            (Handles::Start, None)
        } else {
            let &(event, kind) = self
                .defs
                .events
                .get(name.name.as_str())
                .ok_or_else(|| internal(name.span, "handler of an undeclared event"))?;
            (Handles::Event(event), Some(kind))
        };
        // The payload arrives in registers, one per field.
        let mut payload = Vec::new();
        let mut fields = Vec::new();
        for field in kind.map_or(&[][..], EventKind::fields) {
            let reg = self.reg()?;
            payload.push(reg);
            fields.push(((*field).to_owned(), Operand::Reg(reg)));
        }
        if let (Some(param), Some(kind)) = (params.first(), kind) {
            let value = match kind {
                EventKind::ControlChange => CVal::Scalar(fields[0].1),
                EventKind::NoteOn | EventKind::NoteOff => CVal::Event(fields),
            };
            self.bind(&param.name, value, false);
        }
        let mode = match mode {
            HandlerMode::Plain => Mode::Plain,
            HandlerMode::Release => Mode::Release,
            HandlerMode::Claim { tail } => {
                let seconds = match tail {
                    Some(t) => self.constant(t)?,
                    None => 0.1,
                };
                Mode::Claim {
                    tail: (seconds.max(0.0) * self.sample_rate).round() as u32,
                }
            }
        };
        let (_, diverged) = self.block(body)?;
        if diverged {
            return Err(internal(name.span, "event handlers cannot return"));
        }
        let instrs = std::mem::take(&mut self.code);
        self.scopes = saved_scopes;
        self.code = saved_code;
        self.handler_scope = saved_handler;
        self.events.push(EventCode {
            handles,
            payload,
            instrs,
            mode,
            voice: self.voice,
        });
        Ok(())
    }

    /// `[x; n]`: `x` compiled `n` times, each copy a voice of a new pool.
    fn repeat(&mut self, x: &Expr, n: u32) -> CResult<CVal> {
        let pool = self.pools.len() as u16;
        self.pools.push(Vec::new());
        let saved = self.voice;
        let mut out = Vec::with_capacity(n as usize);
        for copy in 0..n {
            self.voice = Some((pool, copy as u16));
            let v = self.expr(x);
            self.voice = saved;
            let v = v?;
            self.pools[usize::from(pool)].push(v.operands());
            out.push(v);
        }
        Ok(CVal::Frame(out))
    }

    /// `invoke`, `trigger` or `halt` of a sequence, or `invoke` of an event.
    fn invoke(
        &mut self,
        e: &Expr,
        step: Option<&Expr>,
        id: Option<&Expr>,
        target: &Ident,
        args: &[Arg],
    ) -> CResult<CVal> {
        if let Some(&(event, kind)) = self.defs.events.get(target.name.as_str()) {
            let mut values = [Operand::Const(0.0); 3];
            for a in args {
                let Some(name) = &a.name else { continue };
                let v = self.expr(&a.value)?.scalar();
                if let Some(i) = kind.fields().iter().position(|f| *f == name.name) {
                    values[i] = v;
                }
            }
            self.emit(Instr::InvokeEvent { event, values });
            return Ok(CVal::unit());
        }
        let seq = self
            .defs
            .seqs
            .iter()
            .position(|t| t.name == target.name)
            .ok_or_else(|| internal(target.span, "unknown sequence"))?;
        let instances = self.defs.seqs[seq].instances;
        let id = match id {
            Some(id) => Some(self.expr(id)?.scalar()),
            None => None,
        };
        let step = match step {
            Some(s) => Some(self.expr(s)?.scalar()),
            None => None,
        };
        let call = self.calls.len() as u16;
        let dst = self.reg()?;
        let mut settings = [Source::Default; 3];
        let mut repeat = None;
        let mut looping = None;
        let mut captures: Vec<Operand> = Vec::new();
        // Names of the handler's own values, captured per instance.
        let mut captured: Vec<String> = Vec::new();
        let mut followed: Vec<(usize, &Expr)> = Vec::new();
        for a in args {
            let Some(name) = &a.name else { continue };
            let index = match name.name.as_str() {
                "tempo" => super::vm::TEMPO,
                "gate" => super::vm::GATE,
                "velocity" => super::vm::VELOCITY,
                "repeat" => {
                    repeat = Some(self.expr(&a.value)?.scalar());
                    continue;
                }
                "loop" => {
                    looping = Some(self.expr(&a.value)?.scalar());
                    continue;
                }
                _ => continue,
            };
            // The value now, for starting; and, if it follows a stream,
            // code that keeps it up to date.
            let now = self.expr(&a.value)?.scalar();
            if index == super::vm::TEMPO
                && let Operand::Const(t) = now
                && t <= 0.0
            {
                return Err(zero_tempo(a.value.span));
            }
            let (streams, own) = self.dependencies(&a.value);
            if !streams || matches!(now, Operand::Const(_)) {
                settings[index] = Source::Now(now);
            } else if own.is_empty() {
                // Only the rill's streams: one register for every instance.
                let saved = std::mem::replace(&mut self.code, std::mem::take(&mut self.post));
                let v = self.expr(&a.value);
                self.post = std::mem::replace(&mut self.code, saved);
                settings[index] = match v?.scalar() {
                    Operand::Reg(r) => Source::Follow(r),
                    c @ Operand::Const(_) => Source::Now(c),
                };
            } else {
                for n in own {
                    if !captured.contains(&n) {
                        captured.push(n);
                    }
                }
                settings[index] = Source::PerSlot { initial: now };
                followed.push((index, &a.value));
            }
        }
        // Values of the handler's own names, in the order captured.
        let mut layout: Vec<(String, CVal)> = Vec::new();
        for n in &captured {
            let v = self
                .lookup(n)
                .map(|b| b.val.clone())
                .ok_or_else(|| internal(e.span, "captured name not found"))?;
            captures.extend(v.operands());
            layout.push((n.clone(), v));
        }
        if !followed.is_empty() {
            // Per instance slot: load its captured values, then compute the
            // settings that follow streams.
            let saved_code = std::mem::replace(&mut self.code, std::mem::take(&mut self.post));
            let base = self.handler_scope.unwrap_or(self.scopes.len());
            let saved_scopes = self.scopes.clone();
            self.scopes.truncate(base);
            let result = (|| -> CResult<()> {
                for slot in 0..instances {
                    self.scopes.push(HashMap::new());
                    let mut index = 0u8;
                    for (n, v) in &layout {
                        let mut regs = Vec::new();
                        for _ in v.operands() {
                            let dst = self.reg()?;
                            self.emit(Instr::LoadCapture {
                                dst,
                                seq: seq as u16,
                                slot,
                                index,
                            });
                            index += 1;
                            regs.push(Operand::Reg(dst));
                        }
                        let val = v.reshape(regs);
                        self.bind(n, val, false);
                    }
                    for &(setting, x) in &followed {
                        let value = self.expr(x)?.scalar();
                        self.emit(Instr::SetSlot {
                            seq: seq as u16,
                            slot,
                            call,
                            setting: setting as u8,
                            value,
                        });
                    }
                    self.scopes.pop();
                }
                Ok(())
            })();
            self.scopes = saved_scopes;
            self.post = std::mem::replace(&mut self.code, saved_code);
            result?;
        }
        self.calls.push(InvokeCall {
            seq: seq as u16,
            id,
            step,
            dst,
            settings,
            repeat,
            looping,
            captures,
        });
        self.emit(Instr::InvokeSeq { call });
        Ok(CVal::Scalar(Operand::Reg(dst)))
    }

    /// Whether `e` reads any of the rill's streams (values that change over
    /// time, from outside the handler), and which of the handler's own
    /// names it reads.
    fn dependencies(&self, e: &Expr) -> (bool, Vec<String>) {
        let mut names = Vec::new();
        names_in(e, &mut names);
        let base = self.handler_scope.unwrap_or(usize::MAX);
        let mut streams = false;
        let mut own = Vec::new();
        for n in names {
            let Some(depth) = self.scopes.iter().rposition(|s| s.contains_key(&n)) else {
                continue;
            };
            if depth >= base {
                if !own.contains(&n) {
                    own.push(n);
                }
            } else if self.scopes[depth][&n]
                .val
                .operands()
                .iter()
                .any(|o| matches!(o, Operand::Reg(_)))
            {
                streams = true;
            }
        }
        (streams, own)
    }

    fn if_expr(
        &mut self,
        e: &Expr,
        cond: &Expr,
        then: &Block,
        els: Option<&Expr>,
    ) -> CResult<CVal> {
        let wants_value = !matches!(self.types[e.id as usize], Type::Unit | Type::Never);
        let c = self.expr(cond)?.scalar();

        if let Operand::Const(c) = c {
            // Known at build time: only the taken branch exists.
            return if c != 0.0 {
                Ok(self.branch(|s| s.block(then))?.0.unwrap_or_else(CVal::unit))
            } else if let Some(els) = els {
                self.branch(|s| s.expr(els))
            } else {
                Ok(CVal::unit())
            };
        }

        if matches!(self.types[e.id as usize], Type::Fn(..)) {
            return self.choose_fn(c, then, els);
        }

        let skip_then = self.emit(Instr::JumpUnless { cond: c, target: 0 });
        let mut result: Option<CVal> = None;
        let (v1, _) = self.branch(|s| s.block(then))?;
        if wants_value && let Some(v) = v1 {
            self.copy_into(&mut result, &v)?;
        }
        match els {
            Some(els) => {
                let skip_else = self.emit(Instr::Jump { target: 0 });
                self.patch(skip_then);
                let v2 = self.branch(|s| s.expr(els))?;
                if wants_value && self.types[els.id as usize] != Type::Never {
                    self.copy_into(&mut result, &v2)?;
                }
                self.patch(skip_else);
            }
            None => self.patch(skip_then),
        }
        Ok(result.unwrap_or_else(CVal::unit))
    }

    fn loop_values(&mut self, iter: &Expr) -> CResult<Vec<CVal>> {
        match &iter.kind {
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                let start = self.expr(start)?.scalar();
                let end = self.expr(end)?.scalar();
                let (Operand::Const(start), Operand::Const(end)) = (start, end) else {
                    return Err(Diagnostic::error(
                        iter.span,
                        "range bounds must be known before audio starts",
                    ));
                };
                let start = range_bound(start, iter.span, "start")?;
                let mut end = range_bound(end, iter.span, "end")?;
                if *inclusive {
                    end = end.checked_add(1).ok_or_else(|| {
                        Diagnostic::error(iter.span, "inclusive range end is too large")
                    })?;
                }
                Ok((start..end)
                    .map(|i| CVal::Scalar(Operand::Const(i as f32)))
                    .collect())
            }
            _ => {
                let value = self.expr(iter)?;
                let value = self.detach(value)?;
                let CVal::Frame(values) = value else {
                    return Err(internal(iter.span, "looping over a non-iterable value"));
                };
                Ok(values)
            }
        }
    }

    /// `if c { f } else { g }` where the result is a function and `c` is
    /// only known while playing. Each side still runs only when taken, for
    /// any code it has; the result calls one or the other depending on `c`.
    fn choose_fn(&mut self, c: Operand, then: &Block, els: Option<&Expr>) -> CResult<CVal> {
        // Keep the condition as it is now, even if it reads `state` that is
        // assigned before the function is called.
        let c = self.detach(CVal::Scalar(c))?.scalar();
        let skip_then = self.emit(Instr::JumpUnless { cond: c, target: 0 });
        let (v1, _) = self.branch(|s| s.block(then))?;
        let skip_else = self.emit(Instr::Jump { target: 0 });
        self.patch(skip_then);
        let v2 = match els {
            Some(els) => Some(self.branch(|s| s.expr(els))?),
            None => None,
        };
        self.patch(skip_else);
        match (v1, v2) {
            (Some(CVal::Fn(a)), Some(CVal::Fn(b))) if a == b => Ok(CVal::Fn(a)),
            (Some(CVal::Fn(a)), Some(CVal::Fn(b))) => Ok(CVal::Fn(FnVal::Choice {
                cond: c,
                then: Box::new(a),
                els: Box::new(b),
            })),
            // One side always returns, so the other is the only value.
            (Some(v), None) | (None, Some(v)) => Ok(v),
            _ => Err(internal(
                self.span,
                "function-valued `if` without functions",
            )),
        }
    }

    fn branch<T>(&mut self, f: impl FnOnce(&mut Self) -> CResult<T>) -> CResult<T> {
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn call(
        &mut self,
        e: &Expr,
        callee: &Ident,
        sizes: &[SizeExpr],
        args: &[Arg],
    ) -> CResult<CVal> {
        let name = callee.name.as_str();
        if let Some(binding) = self.lookup(name)
            && let CVal::Fn(f) = binding.val.clone()
        {
            if !sizes.is_empty() {
                return Err(Diagnostic::error(
                    callee.span,
                    "function values do not take explicit size arguments",
                ));
            }
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(self.expr(&a.value)?);
            }
            return self.call_value(&f, vals, e.span);
        }
        // User definitions shadow built-ins.
        if let Some((def, sig)) = self.defs.get(name) {
            if sig.rate != (1, 1) {
                return Err(rate_unsupported(callee.span, name));
            }
            let slots = order_args(sig, args);
            let mut vals: Vec<Option<CVal>> = Vec::new();
            for slot in &slots {
                vals.push(match slot {
                    Some(a) => Some(self.expr(a)?),
                    None => None,
                });
            }
            let filled = self.fill_defaults(def, sig, vals, e.span)?;
            let sizes = self.size_values(sizes, sig)?;
            return self.call_lifted(def, sig, filled, sizes);
        }
        if !sizes.is_empty() {
            return Err(Diagnostic::error(
                callee.span,
                "built-in functions do not take explicit size arguments",
            ));
        }
        self.call_builtin(e.span, name, args)
    }

    /// Complete `vals` (one per parameter) with defaults. Defaults are
    /// constants, compiled where only the size parameters are visible.
    fn fill_defaults(
        &mut self,
        def: &Def,
        sig: &Signature,
        vals: Vec<Option<CVal>>,
        span: Span,
    ) -> CResult<Vec<CVal>> {
        let saved = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        for (p, v) in sig.params.iter().zip(&vals) {
            if let Some(v) = v {
                self.bind_sizes(&p.ty, v);
            }
        }
        let mut filled = Vec::new();
        let mut result = Ok(());
        for ((v, p), dp) in vals.into_iter().zip(&sig.params).zip(&def.params) {
            match v {
                Some(v) => filled.push(v),
                None => match dp.default.as_ref() {
                    Some(d) => match self.expr(d) {
                        Ok(v) => filled.push(v),
                        Err(err) => {
                            result = Err(err);
                            break;
                        }
                    },
                    None => {
                        result = Err(internal(span, &format!("missing argument `{}`", p.name)));
                        break;
                    }
                },
            }
        }
        self.scopes = saved;
        result.map(|()| filled)
    }

    fn size_values(
        &mut self,
        sizes: &[SizeExpr],
        sig: &Signature,
    ) -> CResult<HashMap<String, CVal>> {
        let mut out = HashMap::new();
        for (name, size) in sig.generics.iter().zip(sizes) {
            let val = match size {
                SizeExpr::Lit(n, _) => CVal::Scalar(Operand::Const(*n as f32)),
                SizeExpr::Var(id) => self
                    .lookup(&id.name)
                    .map(|b| b.val.clone())
                    .ok_or_else(|| internal(id.span, &format!("unknown size `{}`", id.name)))?,
            };
            out.insert(name.clone(), val);
        }
        Ok(out)
    }

    /// Call a function value with positional arguments. Missing trailing
    /// arguments take the function's defaults.
    fn call_value(&mut self, f: &FnVal, vals: Vec<CVal>, span: Span) -> CResult<CVal> {
        match f {
            FnVal::Named(name) => {
                if let Some((def, sig)) = self.defs.get(name) {
                    let mut slots: Vec<Option<CVal>> = vals.into_iter().map(Some).collect();
                    slots.resize(sig.params.len(), None);
                    let filled = self.fill_defaults(def, sig, slots, span)?;
                    return self.inline(def, sig, filled, HashMap::new());
                }
                let sigs = super::builtins::lookup(name);
                let mut vals = vals;
                if let [sig] = sigs.as_slice() {
                    for p in &sig.params[vals.len().min(sig.params.len())..] {
                        let d = super::builtins::default_value(name, &p.name).ok_or_else(|| {
                            internal(span, &format!("missing argument `{}`", p.name))
                        })?;
                        vals.push(CVal::Scalar(Operand::Const(d)));
                    }
                }
                self.builtin(span, name, vals)
            }
            FnVal::Lambda { id, captured } => {
                let lambda = self
                    .defs
                    .lambda(*id)
                    .ok_or_else(|| internal(span, "unknown anonymous fn"))?;
                self.inline_lambda(lambda, captured.clone(), vals)
            }
            FnVal::Choice { cond, then, els } => {
                let skip_then = self.emit(Instr::JumpUnless {
                    cond: *cond,
                    target: 0,
                });
                let mut result = None;
                let a = self.branch(|s| s.call_value(then, vals.clone(), span))?;
                self.copy_into(&mut result, &a)?;
                let skip_else = self.emit(Instr::Jump { target: 0 });
                self.patch(skip_then);
                let b = self.branch(|s| s.call_value(els, vals, span))?;
                self.copy_into(&mut result, &b)?;
                self.patch(skip_else);
                Ok(result.unwrap_or_else(CVal::unit))
            }
        }
    }

    /// Compile an anonymous fn's body in place, in the scopes it captured.
    fn inline_lambda(
        &mut self,
        lambda: &Expr,
        captured: Vec<HashMap<String, Binding>>,
        args: Vec<CVal>,
    ) -> CResult<CVal> {
        let ExprKind::Fn { params, body, .. } = &lambda.kind else {
            return Err(internal(lambda.span, "not an anonymous fn"));
        };
        let key = format!("fn#{}", lambda.id);
        self.enter(&key, lambda.span)?;
        let saved_scopes = std::mem::replace(&mut self.scopes, captured);
        let saved_span = std::mem::replace(&mut self.span, lambda.span);
        self.scopes.push(HashMap::new());
        for (p, a) in params.iter().zip(args) {
            self.bind(&p.name.name, a, false);
        }

        self.rets.push(Ret {
            val: None,
            patches: Vec::new(),
            depth: self.depth,
            direct: None,
        });
        let result = self
            .block_in_current_scope(body)
            .and_then(|(value, diverged)| {
                if !diverged && let Some(v) = value {
                    self.ret(v)?;
                }
                Ok(())
            });
        let ret = self.rets.pop().expect("pushed above");
        for at in ret.patches {
            self.patch(at);
        }
        self.scopes = saved_scopes;
        self.span = saved_span;
        self.inlining.pop();
        result?;
        Ok(ret.direct.or(ret.val).unwrap_or_else(CVal::unit))
    }

    /// Note that `key` is being inlined, refusing recursion.
    fn enter(&mut self, key: &str, span: Span) -> CResult<()> {
        if self.inlining.iter().any(|k| k == key) {
            return Err(Diagnostic::error(
                span,
                "recursion through a function value is not allowed",
            )
            .with_help("the run stage has no unbounded loops; a fn cannot end up calling itself"));
        }
        self.inlining.push(key.to_owned());
        Ok(())
    }

    /// Inline `def`, once per element if arguments have more frame layers
    /// than their parameters (lifting). The checker made sure every lifted
    /// argument has the same extra layers.
    fn call_lifted(
        &mut self,
        def: &Def,
        sig: &Signature,
        args: Vec<CVal>,
        sizes: HashMap<String, CVal>,
    ) -> CResult<CVal> {
        let extra: Vec<usize> = sig
            .params
            .iter()
            .zip(&args)
            .map(|(p, a)| match a {
                CVal::Frame(_) => a.depth().saturating_sub(type_depth(&p.ty)),
                _ => 0,
            })
            .collect();
        self.lift(&extra, &args, &mut |s, args| {
            s.inline(def, sig, args, sizes.clone())
        })
    }

    /// Run `f` once per element of the extra layers of `args`, `extra[i]`
    /// layers for argument `i`, and collect the results in those layers.
    fn lift(
        &mut self,
        extra: &[usize],
        args: &[CVal],
        f: &mut dyn FnMut(&mut Self, Vec<CVal>) -> CResult<CVal>,
    ) -> CResult<CVal> {
        let n = extra.iter().zip(args).find_map(|(&e, a)| match a {
            CVal::Frame(xs) if e > 0 => Some(xs.len()),
            _ => None,
        });
        let Some(n) = n else {
            return f(self, args.to_vec());
        };
        let inner: Vec<usize> = extra.iter().map(|&e| e.saturating_sub(1)).collect();
        let mut out = Vec::with_capacity(n);
        // The copies form a voice pool.
        let pool = self.pools.len() as u16;
        self.pools.push(Vec::new());
        let saved = self.voice;
        for c in 0..n {
            let per_element: Vec<CVal> = extra
                .iter()
                .zip(args)
                .map(|(&e, a)| match a {
                    CVal::Frame(xs) if e > 0 => xs[c].clone(),
                    _ => a.clone(),
                })
                .collect();
            self.voice = Some((pool, c as u16));
            let v = self.lift(&inner, &per_element, f);
            self.voice = saved;
            let v = v?;
            self.pools[usize::from(pool)].push(v.operands());
            out.push(v);
        }
        Ok(CVal::Frame(out))
    }

    /// An operator with a `Gain` on one side. Gains are amplitude factors,
    /// so moving in level multiplies and scaling a level raises to a power.
    fn gain_binary(
        &mut self,
        op: BinOp,
        a: &CVal,
        b: &CVal,
        a_gain: bool,
        b_gain: bool,
        result: &Type,
    ) -> CResult<CVal> {
        let one = CVal::Scalar(Operand::Const(1.0));
        match (op, a_gain, b_gain) {
            // `x + g`, `x - g`, and combining levels.
            (BinOp::Add, _, true) => self.zip2(Op2::Mul, a, b),
            (BinOp::Sub, _, true) => self.zip2(Op2::Div, a, b),
            // Scaling a level: half of -6dB is -3dB.
            (BinOp::Mul, true, false) => self.zip2(Op2::Pow, a, b),
            (BinOp::Mul, false, true) => self.zip2(Op2::Pow, b, a),
            (BinOp::Div, true, false) => {
                let inverse = self.zip2(Op2::Div, &one, b)?;
                self.zip2(Op2::Pow, a, &inverse)
            }
            // The ratio of two levels, as a plain number.
            (BinOp::Div, true, true) => {
                let la = self.map1(Op1::Log, a)?;
                let lb = self.map1(Op1::Log, b)?;
                self.zip2(Op2::Div, &la, &lb)
            }
            // Comparisons work on the factors directly.
            _ => self.zip2(op2_for(op, result), a, b),
        }
    }

    /// `freq + interval` transposes by semitones, so `440Hz + 12st` is
    /// `880Hz`; subtraction moves the same distance downward.
    fn freq_interval_binary(&mut self, op: BinOp, freq: &CVal, interval: &CVal) -> CResult<CVal> {
        let octaves = self.zip2(Op2::Div, interval, &CVal::Scalar(Operand::Const(12.0)))?;
        let ratio = self.zip2(Op2::Pow, &CVal::Scalar(Operand::Const(2.0)), &octaves)?;
        match op {
            BinOp::Add => self.zip2(Op2::Mul, freq, &ratio),
            BinOp::Sub => self.zip2(Op2::Div, freq, &ratio),
            _ => Err(internal(
                self.span,
                "unsupported frequency interval operator",
            )),
        }
    }

    /// A direct call of a built-in: arguments by position or name, with
    /// defaults for the rest.
    fn call_builtin(&mut self, span: Span, name: &str, args: &[Arg]) -> CResult<CVal> {
        let sigs = super::builtins::lookup(name);
        let mut vals = Vec::new();
        if let [sig] = sigs.as_slice() {
            for (slot, p) in order_args(sig, args).into_iter().zip(&sig.params) {
                vals.push(match slot {
                    Some(a) => self.expr(a)?,
                    None => {
                        let d = super::builtins::default_value(name, &p.name).ok_or_else(|| {
                            internal(span, &format!("missing argument `{}`", p.name))
                        })?;
                        CVal::Scalar(Operand::Const(d))
                    }
                });
            }
        } else {
            for a in args {
                vals.push(self.expr(&a.value)?);
            }
        }
        self.builtin(span, name, vals)
    }

    /// A built-in applied to its arguments, in parameter order.
    fn builtin(&mut self, span: Span, name: &str, vals: Vec<CVal>) -> CResult<CVal> {
        if let Some(op) = Op1::builtin(name) {
            return self.map1(op, &vals[0]);
        }
        if let Some(tuning) = Tuning::builtin(name) {
            let (setting, a4) = (vals[1].scalar(), vals[2].scalar());
            return self.map_operands(&vals[0], |s, pitch| s.tune(tuning, pitch, setting, a4));
        }
        match (name, vals.as_slice()) {
            // A gain is stored as its amplitude factor, so the level of an
            // amplitude is its size, kept above -120dB so silence stays finite.
            ("level", [x]) => {
                let size = self.map1(Op1::Abs, x)?;
                self.zip2(Op2::Max, &size, &CVal::Scalar(Operand::Const(1e-6)))
            }
            ("amp", [x]) => Ok(x.clone()),
            ("pow", [x, y]) => self.zip2(Op2::Pow, x, y),
            ("min", [x, y]) => self.zip2(Op2::Min, x, y),
            ("max", [x, y]) => self.zip2(Op2::Max, x, y),
            ("min", [x]) => self.reduce(Op2::Min, x),
            ("max", [x]) => self.reduce(Op2::Max, x),
            ("sum", [x]) => self.reduce(Op2::Add, x),
            ("clamp", [x, lo, hi]) => {
                let low = self.zip2(Op2::Min, x, hi)?;
                self.zip2(Op2::Max, &low, lo)
            }
            _ => Err(Diagnostic::error(
                span,
                format!("`{name}` cannot be used here"),
            )),
        }
    }
}

/// A `Gain`, or a frame of them.
fn is_gain(t: &Type) -> bool {
    match t {
        Type::Gain => true,
        Type::Frame(elem, _) => is_gain(elem),
        _ => false,
    }
}

fn is_freq(t: &Type) -> bool {
    match t {
        Type::Freq => true,
        Type::Frame(elem, _) => is_freq(elem),
        _ => false,
    }
}

fn is_interval(t: &Type) -> bool {
    match t {
        Type::Interval => true,
        Type::Frame(elem, _) => is_interval(elem),
        _ => false,
    }
}

/// Built-in constants. `RATE` is the host rate the graph is built for.
pub fn constant(name: &str, sample_rate: f32) -> Option<f32> {
    match name {
        "PI" => Some(std::f32::consts::PI),
        "TAU" => Some(std::f32::consts::TAU),
        "RATE" => Some(sample_rate),
        _ => None,
    }
}

/// The run-time op for `op`, given the type the checker gave the result.
pub fn op2_for(op: BinOp, result: &Type) -> Op2 {
    match op {
        BinOp::Add => Op2::Add,
        BinOp::Sub => Op2::Sub,
        BinOp::Mul => Op2::Mul,
        BinOp::Div if *result == Type::Int => Op2::IDiv,
        BinOp::Div => Op2::Div,
        BinOp::Rem => Op2::Rem,
        BinOp::Lt => Op2::Lt,
        BinOp::Le => Op2::Le,
        BinOp::Gt => Op2::Gt,
        BinOp::Ge => Op2::Ge,
        BinOp::Eq => Op2::Eq,
        BinOp::Ne => Op2::Ne,
        BinOp::And => Op2::And,
        BinOp::Or => Op2::Or,
    }
}

/// The argument expression for each parameter, by position or name.
pub fn order_args<'e>(sig: &Signature, args: &'e [Arg]) -> Vec<Option<&'e Expr>> {
    let mut slots = vec![None; sig.params.len()];
    for (i, a) in args.iter().enumerate() {
        let pi = match &a.name {
            Some(n) => sig.params.iter().position(|p| p.name == n.name),
            None => Some(i),
        };
        if let Some(pi) = pi {
            slots[pi] = Some(&a.value);
        }
    }
    slots
}

pub fn rate_unsupported(span: Span, name: &str) -> Diagnostic {
    Diagnostic::error(
        span,
        format!("`{name}` changes the sample rate, which is not supported yet"),
    )
    .with_help("rate-changing rills type-check, but cannot be run until resampling lands")
}

fn range_bound(value: f32, span: Span, name: &str) -> CResult<i32> {
    if value.fract() != 0.0 {
        return Err(Diagnostic::error(
            span,
            format!("range {name} must be a whole number"),
        ));
    }
    if value < i32::MIN as f32 || value > i32::MAX as f32 {
        return Err(Diagnostic::error(
            span,
            format!("range {name} is out of bounds"),
        ));
    }
    Ok(value as i32)
}

/// A tempo known when building must move the sequence on. One that follows
/// a stream may still reach 0 while playing; the sequence then holds still.
fn zero_tempo(span: Span) -> Diagnostic {
    Diagnostic::error(span, "a tempo is above 0bpm")
        .with_help("at 0bpm the sequence would never move; to stop it, use `halt`")
}

/// Every name `e` reads, in order.
fn names_in(e: &Expr, out: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Name(n) => out.push(n.clone()),
        ExprKind::Number { .. } | ExprKind::Bool(_) => {}
        ExprKind::Unary(_, x)
        | ExprKind::Cast(x, _)
        | ExprKind::Field(x, _)
        | ExprKind::Repeat(x, _) => names_in(x, out),
        ExprKind::Binary(_, a, b) | ExprKind::Index(a, b) => {
            names_in(a, out);
            names_in(b, out);
        }
        ExprKind::Range { start, end, .. } => {
            names_in(start, out);
            names_in(end, out);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                names_in(&a.value, out);
            }
        }
        ExprKind::Frame(xs) => {
            for x in xs {
                names_in(x, out);
            }
        }
        ExprKind::If { cond, then, els } => {
            names_in(cond, out);
            names_in_block(then, out);
            if let Some(els) = els {
                names_in(els, out);
            }
        }
        ExprKind::Block(b) | ExprKind::Fn { body: b, .. } => names_in_block(b, out),
        ExprKind::Invoke { step, id, args, .. } => {
            for x in step.iter().chain(id.iter()) {
                names_in(x, out);
            }
            for a in args {
                names_in(&a.value, out);
            }
        }
        ExprKind::Halt { id, .. } => {
            if let Some(x) = id {
                names_in(x, out);
            }
        }
    }
}

fn names_in_block(b: &Block, out: &mut Vec<String>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { value: Some(e), .. }
            | Stmt::State { init: e, .. }
            | Stmt::Assign { value: e, .. }
            | Stmt::Return { value: e, .. }
            | Stmt::Expr(e) => names_in(e, out),
            Stmt::Let { value: None, .. } => {}
            Stmt::EventHandler { body, .. } => names_in_block(body, out),
            Stmt::For { iter, body, .. } => {
                names_in(iter, out);
                names_in_block(body, out);
            }
        }
    }
}
