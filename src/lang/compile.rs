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
use super::vm::{Code, EventBinding, EventCode, Instr, Operand, Tuning};
use crate::ops::{Op1, Op2};

/// A compile-time value: one operand per channel.
#[derive(Clone, Debug, PartialEq)]
pub enum CVal {
    Scalar(Operand),
    Frame(Vec<Operand>),
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

    pub fn operands(&self) -> &[Operand] {
        match self {
            CVal::Scalar(o) => std::slice::from_ref(o),
            CVal::Frame(os) => os,
            CVal::Event(_) | CVal::Fn(_) => &[],
        }
    }

    fn scalar(&self) -> Operand {
        match self {
            CVal::Scalar(o) => *o,
            CVal::Frame(os) => os[0],
            CVal::Event(_) | CVal::Fn(_) => Operand::Const(0.0),
        }
    }

    /// Same shape as `self`, with new operands.
    fn reshape(&self, ops: Vec<Operand>) -> CVal {
        match self {
            CVal::Scalar(_) => CVal::Scalar(ops[0]),
            CVal::Frame(_) => CVal::Frame(ops),
            CVal::Event(fields) => CVal::Event(
                fields
                    .iter()
                    .zip(ops)
                    .map(|((name, _), op)| (name.clone(), op))
                    .collect(),
            ),
            CVal::Fn(f) => CVal::Fn(f.clone()),
        }
    }
}

/// Every fn and rill by name, and every anonymous fn by expression id.
pub struct Defs<'a> {
    map: HashMap<&'a str, (&'a Def, &'a Signature)>,
    lambdas: HashMap<u32, &'a Expr>,
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
        Defs {
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
        ExprKind::Unary(_, x) | ExprKind::Field(x, _) => collect_lambdas(x, out),
        ExprKind::Binary(_, a, b) | ExprKind::Index(a, b) => {
            collect_lambdas(a, out);
            collect_lambdas(b, out);
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
    }
}

fn collect_lambdas_in_block<'a>(b: &'a Block, out: &mut HashMap<u32, &'a Expr>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { value: e, .. }
            | Stmt::State { init: e, .. }
            | Stmt::Assign { value: e, .. }
            | Stmt::Return { value: e, .. }
            | Stmt::Expr(e) => collect_lambdas(e, out),
            Stmt::EventHandler { body, .. } => collect_lambdas_in_block(body, out),
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
    let mut c = Compiler {
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
        span: def.name.span,
    };
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
                CVal::Frame(ops)
            }
        });
    }
    let out = c.inline(def, sig, vals)?;
    Ok(Code {
        instrs: c.code,
        regs: c.regs as usize,
        input_regs,
        output: out.operands().to_vec(),
        state_init: c.state_init,
        events: c.events,
    })
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
    let mut c = Compiler {
        defs,
        types,
        sample_rate,
        code: Vec::new(),
        events: Vec::new(),
        inlining: Vec::new(),
        regs: 0,
        state_init: Vec::new(),
        state_regs: HashSet::new(),
        scopes: vec![HashMap::new()],
        rets: Vec::new(),
        depth: 0,
        span: def.name.span,
    };
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
}

type CResult<T> = Result<T, Diagnostic>;

fn internal(span: Span, what: &str) -> Diagnostic {
    Diagnostic::error(span, format!("internal compiler error: {what}"))
}

impl Compiler<'_> {
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

    /// Element-wise `op`, broadcasting a scalar over a frame.
    fn zip2(&mut self, op: Op2, a: &CVal, b: &CVal) -> CResult<CVal> {
        let (ao, bo) = (a.operands(), b.operands());
        let n = ao.len().max(bo.len());
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let x = ao[if ao.len() == 1 { 0 } else { i }];
            let y = bo[if bo.len() == 1 { 0 } else { i }];
            out.push(self.op2(op, x, y)?);
        }
        Ok(match (a, b) {
            (CVal::Scalar(_), CVal::Scalar(_)) => CVal::Scalar(out[0]),
            _ => CVal::Frame(out),
        })
    }

    fn map1(&mut self, op: Op1, x: &CVal) -> CResult<CVal> {
        let mut out = Vec::new();
        for &o in x.operands() {
            out.push(self.op1(op, o)?);
        }
        Ok(x.reshape(out))
    }

    /// Fold a frame with `op`.
    fn reduce(&mut self, op: Op2, x: &CVal) -> CResult<CVal> {
        let ops = x.operands();
        let mut acc = ops[0];
        for &o in &ops[1..] {
            acc = self.op2(op, acc, o)?;
        }
        Ok(CVal::Scalar(acc))
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
        for &o in v.operands() {
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
        let dsts = slot.as_ref().unwrap().operands().to_vec();
        for (d, &src) in dsts.iter().zip(v.operands()) {
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

    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    // ---- bodies ---------------------------------------------------------

    /// Compile `def`'s body in place with `args` bound to its parameters,
    /// and return its result.
    fn inline(&mut self, def: &Def, sig: &Signature, args: Vec<CVal>) -> CResult<CVal> {
        self.enter(&def.name.name, def.name.span)?;
        let result = self.inline_body(def, sig, args);
        self.inlining.pop();
        result
    }

    fn inline_body(&mut self, def: &Def, sig: &Signature, args: Vec<CVal>) -> CResult<CVal> {
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        let saved_span = std::mem::replace(&mut self.span, def.name.span);

        for (p, a) in sig.params.iter().zip(&args) {
            if let (Type::Frame(_, Size::Var(v)), CVal::Frame(ops)) = (&p.ty, a) {
                self.bind(v, CVal::Scalar(Operand::Const(ops.len() as f32)), false);
            }
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
                    name, params, body, ..
                } = stmt
                {
                    self.event_handler(name, params, body)?;
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
            Stmt::Let { name, value, .. } => {
                let v = self.expr(value)?;
                let v = self.detach(v)?;
                self.bind(&name.name, v, false);
                Ok((None, false))
            }
            Stmt::State { name, init, .. } => {
                let v = self.expr(init)?;
                let mut regs = Vec::new();
                for &o in v.operands() {
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
                let dsts = match self.lookup(&target.name) {
                    Some(b) if b.mutable => b.val.operands().to_vec(),
                    _ => return Err(internal(target.span, "assignment to a non-state name")),
                };
                for (d, &src) in dsts.iter().zip(v.operands()) {
                    if let Operand::Reg(dst) = *d {
                        self.emit(Instr::Copy { dst, src });
                    }
                }
                Ok((None, false))
            }
            Stmt::Return { value, .. } => {
                let v = self.expr(value)?;
                self.ret(v)?;
                Ok((None, true))
            }
            Stmt::EventHandler { .. } => Ok((None, false)),
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
                let op = op2_for(*op, &self.types[e.id as usize]);
                self.zip2(op, &va, &vb)
            }
            ExprKind::Call { callee, args, .. } => self.call(e, callee, args),
            ExprKind::If { cond, then, els } => self.if_expr(e, cond, then, els.as_deref()),
            ExprKind::Block(b) => Ok(self.block(b)?.0.unwrap_or_else(CVal::unit)),
            ExprKind::Frame(elems) => {
                let mut ops = Vec::new();
                for el in elems {
                    ops.push(self.expr(el)?.scalar());
                }
                Ok(CVal::Frame(ops))
            }
            ExprKind::Index(base, index) => {
                let b = self.expr(base)?;
                let i = self.expr(index)?.scalar();
                let ops = b.operands().to_vec();
                if let Operand::Const(i) = i {
                    return ops
                        .get(i as usize)
                        .map(|&o| CVal::Scalar(o))
                        .ok_or_else(|| {
                            Diagnostic::error(index.span, format!("channel {i} is out of range"))
                        });
                }
                // Unknown index: pick with a chain of selects. Out of range
                // reads channel 0.
                let dst = self.reg()?;
                self.emit(Instr::Copy { dst, src: ops[0] });
                for (c, &o) in ops.iter().enumerate().skip(1) {
                    let hit = self.op2(Op2::Eq, i, Operand::Const(c as f32))?;
                    self.emit(Instr::Select {
                        dst,
                        cond: hit,
                        a: o,
                        b: Operand::Reg(dst),
                    });
                }
                Ok(CVal::Scalar(Operand::Reg(dst)))
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

    fn event_handler(&mut self, name: &Ident, params: &[Ident], body: &Block) -> CResult<()> {
        let saved_code = std::mem::take(&mut self.code);
        let saved_scopes = self.scopes.clone();
        self.scopes.push(HashMap::new());
        let mut bindings = Vec::new();
        for param in params {
            if param.name == "cc" {
                let reg = self.reg()?;
                self.bind(&param.name, CVal::Scalar(Operand::Reg(reg)), false);
                bindings.push(EventBinding::Scalar {
                    name: param.name.clone(),
                    reg,
                });
            } else {
                let mut fields = Vec::new();
                for field in event_fields_for(&param.name) {
                    let reg = self.reg()?;
                    fields.push(((*field).to_owned(), Operand::Reg(reg)));
                    bindings.push(EventBinding::Field {
                        path: format!("{}.{}", param.name, field),
                        fallback: (*field).to_owned(),
                        reg,
                    });
                }
                self.bind(&param.name, CVal::Event(fields), false);
            }
        }
        let (_, diverged) = self.block(body)?;
        if diverged {
            return Err(internal(name.span, "event handlers cannot return"));
        }
        let instrs = std::mem::take(&mut self.code);
        self.scopes = saved_scopes;
        self.code = saved_code;
        self.events.push(EventCode {
            name: name.name.clone(),
            bindings,
            instrs,
        });
        Ok(())
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

    fn call(&mut self, e: &Expr, callee: &Ident, args: &[Arg]) -> CResult<CVal> {
        let name = callee.name.as_str();
        if let Some(binding) = self.lookup(name)
            && let CVal::Fn(f) = binding.val.clone()
        {
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
            return self.call_lifted(def, sig, filled);
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
            if let (Type::Frame(_, Size::Var(n)), Some(CVal::Frame(ops))) = (&p.ty, v) {
                self.bind(n, CVal::Scalar(Operand::Const(ops.len() as f32)), false);
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

    /// Call a function value with positional arguments. Missing trailing
    /// arguments take the function's defaults.
    fn call_value(&mut self, f: &FnVal, vals: Vec<CVal>, span: Span) -> CResult<CVal> {
        match f {
            FnVal::Named(name) => {
                if let Some((def, sig)) = self.defs.get(name) {
                    let mut slots: Vec<Option<CVal>> = vals.into_iter().map(Some).collect();
                    slots.resize(sig.params.len(), None);
                    let filled = self.fill_defaults(def, sig, slots, span)?;
                    return self.inline(def, sig, filled);
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

    /// Inline `def`, once per channel if a scalar parameter got a frame.
    fn call_lifted(&mut self, def: &Def, sig: &Signature, args: Vec<CVal>) -> CResult<CVal> {
        let lift = sig
            .params
            .iter()
            .zip(&args)
            .find_map(|(p, a)| match (a, &p.ty) {
                (CVal::Frame(ops), t) if !matches!(t, Type::Frame(..)) => Some(ops.len()),
                _ => None,
            });
        let Some(n) = lift else {
            return self.inline(def, sig, args);
        };
        let mut out = Vec::new();
        for c in 0..n {
            let per_channel = sig
                .params
                .iter()
                .zip(&args)
                .map(|(p, a)| match (a, &p.ty) {
                    (CVal::Frame(ops), t) if !matches!(t, Type::Frame(..)) => CVal::Scalar(ops[c]),
                    _ => a.clone(),
                })
                .collect();
            out.push(self.inline(def, sig, per_channel)?.scalar());
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
            let mut out = Vec::new();
            for &pitch in vals[0].operands() {
                out.push(self.tune(tuning, pitch, setting, a4)?);
            }
            return Ok(vals[0].reshape(out));
        }
        match (name, vals.as_slice()) {
            // A gain is stored as its amplitude factor, so the level of an
            // amplitude is its size, kept above -120dB so silence stays finite.
            ("level", [x]) => {
                let size = self.map1(Op1::Abs, x)?;
                self.zip2(Op2::Max, &size, &CVal::Scalar(Operand::Const(1e-6)))
            }
            ("amp" | "Float" | "Sample", [x]) => Ok(x.clone()),
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

fn event_fields_for(param: &str) -> &'static [&'static str] {
    match param {
        "note" => &["pitch", "velocity", "release"],
        "control" => &["channel", "index"],
        _ => &["pitch", "velocity", "release", "channel", "index"],
    }
}

/// A `Gain`, or a frame of them.
fn is_gain(t: &Type) -> bool {
    match t {
        Type::Gain => true,
        Type::Frame(elem, _) => **elem == Type::Gain,
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
