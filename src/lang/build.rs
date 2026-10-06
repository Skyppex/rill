//! The build stage: run the top-level code and produce a [`Graph`].
//!
//! Top-level values are streams in the graph (a node output) or constants.
//! A frame is a list of them, one per channel. Each call to a fn or rill
//! becomes one compiled [`Program`] node, or several if it is lifted over
//! channels; operators between streams become small arithmetic nodes, and
//! anything whose inputs are all constant is folded away.
//!
//! A `let` binds a value, not an expression, so a bound stream used twice is
//! one node.

use std::collections::HashMap;

use super::ast::*;
use super::check::Checked;
use super::compile::{self, ArgSpec, CVal, Defs, compile_instance, op2_for, order_args};
use super::diag::{Diagnostic, Span};
use super::types::{Signature, Size, Type};
use super::vm::{Operand, Program as ProgramNode};
use crate::engine::Config;
use crate::graph::{Graph, Input, NodeId};
use crate::nodes::{Binary, Select, Unary};
use crate::ops::{Op1, Op2};

/// A built graph and anything worth warning about.
pub struct Built {
    pub graph: Graph,
    pub warnings: Vec<Diagnostic>,
}

/// Run the top level of a checked program for an engine with `config`.
pub fn build(
    program: &Program,
    checked: &Checked,
    config: &Config,
) -> Result<Built, Vec<Diagnostic>> {
    let mut b = Builder {
        defs: Defs::new(program, checked),
        types: &checked.types,
        rate: config.sample_rate as f32,
        out_channels: config.out_channels,
        graph: Graph::new(),
        scopes: vec![HashMap::new()],
        outs: Vec::new(),
        errors: Vec::new(),
    };
    for item in &program.items {
        if let Item::Stmt(s) = item {
            b.stmt(s);
        }
    }
    let mut warnings = Vec::new();
    if b.outs.is_empty() && b.errors.is_empty() {
        let end = program.items.last().map_or(Span::default(), |i| match i {
            Item::Fn(d) | Item::Rill(d) => d.span,
            Item::Stmt(s) => s.span(),
        });
        warnings.push(
            Diagnostic::warning(end, "nothing is sent to `out`, so this plays silence")
                .with_help("end with something like `out(voice)`"),
        );
    }
    b.finish_output();
    if b.errors.is_empty() {
        Ok(Built {
            graph: b.graph,
            warnings,
        })
    } else {
        Err(b.errors)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum TVal {
    Scalar(Input),
    Frame(Vec<Input>),
}

impl TVal {
    fn unit() -> TVal {
        TVal::Frame(Vec::new())
    }

    fn inputs(&self) -> &[Input] {
        match self {
            TVal::Scalar(i) => std::slice::from_ref(i),
            TVal::Frame(is) => is,
        }
    }

    fn scalar(&self) -> Input {
        self.inputs()[0]
    }

    fn reshape(&self, inputs: Vec<Input>) -> TVal {
        match self {
            TVal::Scalar(_) => TVal::Scalar(inputs[0]),
            TVal::Frame(_) => TVal::Frame(inputs),
        }
    }

    /// The constant value, if every channel is one.
    fn as_const(&self) -> Option<CVal> {
        let ops: Option<Vec<Operand>> = self
            .inputs()
            .iter()
            .map(|i| match i {
                Input::Const(c) => Some(Operand::Const(*c)),
                Input::Port(..) => None,
            })
            .collect();
        let ops = ops?;
        Some(match self {
            TVal::Scalar(_) => CVal::Scalar(ops[0]),
            TVal::Frame(_) => CVal::Frame(ops),
        })
    }
}

struct Builder<'a> {
    defs: Defs<'a>,
    types: &'a [Type],
    rate: f32,
    out_channels: usize,
    graph: Graph,
    scopes: Vec<HashMap<String, TVal>>,
    /// Every `out(...)`, mixed together at the end.
    outs: Vec<(TVal, Span)>,
    errors: Vec<Diagnostic>,
}

type BResult<T> = Result<T, Diagnostic>;

impl Builder<'_> {
    fn stmt(&mut self, s: &Stmt) {
        let result = match s {
            Stmt::Let { name, value, .. } => self.expr(value).map(|v| {
                self.scopes.last_mut().unwrap().insert(name.name.clone(), v);
            }),
            Stmt::Expr(e) => self.expr(e).map(|_| ()),
            // The checker rejects these at the top level.
            Stmt::State { span, .. } | Stmt::Assign { span, .. } | Stmt::Return { span, .. } => {
                Err(Diagnostic::error(*span, "not allowed at the top level"))
            }
        };
        if let Err(e) = result {
            self.errors.push(e);
        }
    }

    fn op2(&mut self, op: Op2, a: Input, b: Input) -> Input {
        if let (Input::Const(a), Input::Const(b)) = (a, b) {
            return Input::Const(op.apply(a, b));
        }
        self.graph.node(Binary(op), [a, b]).into()
    }

    fn op1(&mut self, op: Op1, x: Input) -> Input {
        if let Input::Const(x) = x {
            return Input::Const(op.apply(x, self.rate));
        }
        self.graph.node(Unary(op), [x]).into()
    }

    fn zip2(&mut self, op: Op2, a: &TVal, b: &TVal) -> TVal {
        let (ai, bi) = (a.inputs(), b.inputs());
        let n = ai.len().max(bi.len());
        let out: Vec<Input> = (0..n)
            .map(|i| {
                let x = ai[if ai.len() == 1 { 0 } else { i }];
                let y = bi[if bi.len() == 1 { 0 } else { i }];
                self.op2(op, x, y)
            })
            .collect();
        match (a, b) {
            (TVal::Scalar(_), TVal::Scalar(_)) => TVal::Scalar(out[0]),
            _ => TVal::Frame(out),
        }
    }

    fn map1(&mut self, op: Op1, x: &TVal) -> TVal {
        let out = x.inputs().iter().map(|&i| self.op1(op, i)).collect();
        x.reshape(out)
    }

    fn reduce(&mut self, op: Op2, x: &TVal) -> TVal {
        let ins = x.inputs();
        let mut acc = ins[0];
        for &i in &ins[1..] {
            acc = self.op2(op, acc, i);
        }
        TVal::Scalar(acc)
    }

    fn select(&mut self, cond: Input, a: Input, b: Input) -> Input {
        if a == b {
            return a;
        }
        self.graph.node(Select, [cond, a, b]).into()
    }

    fn lookup(&self, name: &str) -> Option<&TVal> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn expr(&mut self, e: &Expr) -> BResult<TVal> {
        match &e.kind {
            ExprKind::Number { value, .. } => Ok(TVal::Scalar(Input::Const(*value as f32))),
            ExprKind::Bool(b) => Ok(TVal::Scalar(Input::Const(f32::from(u8::from(*b))))),
            ExprKind::Name(name) => {
                if let Some(v) = self.lookup(name) {
                    return Ok(v.clone());
                }
                compile::constant(name, self.rate)
                    .map(|c| TVal::Scalar(Input::Const(c)))
                    .ok_or_else(|| Diagnostic::error(e.span, format!("unknown name `{name}`")))
            }
            ExprKind::Unary(op, x) => {
                let v = self.expr(x)?;
                Ok(match op {
                    UnOp::Plus => v,
                    UnOp::Neg => self.map1(Op1::Neg, &v),
                    UnOp::Not => self.map1(Op1::Not, &v),
                })
            }
            ExprKind::Binary(op, a, b) => {
                let va = self.expr(a)?;
                let vb = self.expr(b)?;
                Ok(self.zip2(op2_for(*op, &self.types[e.id as usize]), &va, &vb))
            }
            ExprKind::Call { callee, args, .. } => self.call(e, callee, args),
            ExprKind::If { cond, then, els } => {
                let c = self.expr(cond)?.scalar();
                if let Input::Const(c) = c {
                    return if c != 0.0 {
                        self.block(then)
                    } else if let Some(els) = els {
                        self.expr(els)
                    } else {
                        Ok(TVal::unit())
                    };
                }
                // A stream condition: build both sides and pick per frame.
                let outs_before = self.outs.len();
                let a = self.block(then)?;
                let b = match els {
                    Some(els) => self.expr(els)?,
                    None => TVal::unit(),
                };
                if self.outs.len() != outs_before {
                    return Err(Diagnostic::error(
                        e.span,
                        "`out` cannot depend on a condition that changes over time",
                    )
                    .with_help(
                        "call `out` unconditionally and choose its input with `if` instead",
                    ));
                }
                let picked = a
                    .inputs()
                    .iter()
                    .zip(b.inputs())
                    .map(|(&x, &y)| self.select(c, x, y))
                    .collect();
                Ok(a.reshape(picked))
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Frame(elems) => {
                let mut ins = Vec::new();
                for el in elems {
                    ins.push(self.expr(el)?.scalar());
                }
                Ok(TVal::Frame(ins))
            }
            ExprKind::Index(base, index) => {
                let b = self.expr(base)?;
                let i = self.expr(index)?.scalar();
                let ins = b.inputs().to_vec();
                if let Input::Const(i) = i {
                    return ins
                        .get(i as usize)
                        .map(|&x| TVal::Scalar(x))
                        .ok_or_else(|| {
                            Diagnostic::error(index.span, format!("channel {i} is out of range"))
                        });
                }
                let mut acc = ins[0];
                for (c, &x) in ins.iter().enumerate().skip(1) {
                    let hit = self.op2(Op2::Eq, i, Input::Const(c as f32));
                    acc = self.select(hit, x, acc);
                }
                Ok(TVal::Scalar(acc))
            }
        }
    }

    fn block(&mut self, b: &Block) -> BResult<TVal> {
        self.scopes.push(HashMap::new());
        let mut value = TVal::unit();
        let mut result = Ok(());
        for s in &b.stmts {
            match s {
                Stmt::Expr(e) => match self.expr(e) {
                    Ok(v) => value = v,
                    Err(err) => {
                        result = Err(err);
                        break;
                    }
                },
                s => {
                    value = TVal::unit();
                    let before = self.errors.len();
                    self.stmt(s);
                    if self.errors.len() != before {
                        result = Err(self.errors.pop().unwrap());
                        break;
                    }
                }
            }
        }
        self.scopes.pop();
        result.map(|()| value)
    }

    fn call(&mut self, e: &Expr, callee: &Ident, args: &[Arg]) -> BResult<TVal> {
        let name = callee.name.as_str();
        let Some((def, sig)) = self.defs.get(name) else {
            return self.builtin(e, name, args);
        };
        if sig.rate != (1, 1) {
            return Err(compile::rate_unsupported(callee.span, name));
        }

        let slots = order_args(sig, args);
        let mut vals = Vec::new();
        for slot in &slots {
            vals.push(match slot {
                Some(a) => Some(self.expr(a)?),
                None => None,
            });
        }
        // Defaults see only the size parameters.
        let saved = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        for (p, v) in sig.params.iter().zip(&vals) {
            if let (Type::Frame(_, Size::Var(n)), Some(TVal::Frame(ins))) = (&p.ty, v) {
                let len = TVal::Scalar(Input::Const(ins.len() as f32));
                self.scopes[0].insert(n.clone(), len);
            }
        }
        let mut filled = Vec::new();
        let mut failed = None;
        for (v, dp) in vals.into_iter().zip(&def.params) {
            match v {
                Some(v) => filled.push(v),
                None => match dp.default.as_ref().map(|d| self.expr(d)) {
                    Some(Ok(v)) => filled.push(v),
                    Some(Err(err)) => failed = Some(err),
                    None => {
                        failed = Some(Diagnostic::error(e.span, "missing argument"));
                    }
                },
            }
        }
        self.scopes = saved;
        if let Some(err) = failed {
            return Err(err);
        }

        let lift = sig
            .params
            .iter()
            .zip(&filled)
            .find_map(|(p, a)| match (a, &p.ty) {
                (TVal::Frame(ins), t) if !matches!(t, Type::Frame(..)) => Some(ins.len()),
                _ => None,
            });
        let Some(n) = lift else {
            return self.instance(e.span, def, sig, &filled);
        };
        let mut out = Vec::new();
        for c in 0..n {
            let per_channel: Vec<TVal> = sig
                .params
                .iter()
                .zip(&filled)
                .map(|(p, a)| match (a, &p.ty) {
                    (TVal::Frame(ins), t) if !matches!(t, Type::Frame(..)) => TVal::Scalar(ins[c]),
                    _ => a.clone(),
                })
                .collect();
            out.push(self.instance(e.span, def, sig, &per_channel)?.scalar());
        }
        Ok(TVal::Frame(out))
    }

    /// One compiled instance of `def`, as a node unless its output turns out
    /// to be constant.
    fn instance(&mut self, span: Span, def: &Def, sig: &Signature, args: &[TVal]) -> BResult<TVal> {
        let mut specs = Vec::new();
        let mut inputs = Vec::new();
        for a in args {
            specs.push(match a.as_const() {
                Some(c) => ArgSpec::Const(c),
                None => {
                    inputs.extend_from_slice(a.inputs());
                    ArgSpec::Stream(match a {
                        TVal::Scalar(_) => None,
                        TVal::Frame(ins) => Some(ins.len()),
                    })
                }
            });
        }
        let code =
            compile_instance(&self.defs, self.types, self.rate, def, sig, &specs).map_err(|d| {
                if d.span == Span::default() {
                    Diagnostic { span, ..d }
                } else {
                    d
                }
            })?;

        let frame = matches!(sig.ret, Type::Frame(..));
        let consts: Option<Vec<Input>> = code
            .output
            .iter()
            .map(|o| match o {
                Operand::Const(c) => Some(Input::Const(*c)),
                Operand::Reg(_) => None,
            })
            .collect();
        let outs: Vec<Input> = match consts {
            Some(c) => c,
            None => {
                let channels = code.output.len();
                let id: NodeId = self.graph.node(ProgramNode::new(code), inputs);
                (0..channels).map(|c| id.channel(c)).collect()
            }
        };
        Ok(if frame {
            TVal::Frame(outs)
        } else {
            TVal::Scalar(outs[0])
        })
    }

    fn builtin(&mut self, e: &Expr, name: &str, args: &[Arg]) -> BResult<TVal> {
        let mut vals = Vec::new();
        for a in args {
            vals.push(self.expr(&a.value)?);
        }
        if name == "out" {
            self.outs.push((vals[0].clone(), e.span));
            return Ok(TVal::unit());
        }
        if let Some(op) = Op1::builtin(name) {
            return Ok(self.map1(op, &vals[0]));
        }
        Ok(match (name, vals.as_slice()) {
            ("f32" | "sample", [x]) => x.clone(),
            ("pow", [x, y]) => self.zip2(Op2::Pow, x, y),
            ("min", [x, y]) => self.zip2(Op2::Min, x, y),
            ("max", [x, y]) => self.zip2(Op2::Max, x, y),
            ("min", [x]) => self.reduce(Op2::Min, x),
            ("max", [x]) => self.reduce(Op2::Max, x),
            ("sum", [x]) => self.reduce(Op2::Add, x),
            ("clamp", [x, lo, hi]) => {
                let low = self.zip2(Op2::Min, x, hi);
                self.zip2(Op2::Max, &low, lo)
            }
            _ => {
                return Err(Diagnostic::error(
                    e.span,
                    format!("`{name}` cannot be used here"),
                ));
            }
        })
    }

    /// Mix every `out(...)` into the graph's output.
    fn finish_output(&mut self) {
        let n = self.out_channels;
        let mut mono: Option<Input> = None;
        let mut channels: Option<Vec<Input>> = None;
        let outs = std::mem::take(&mut self.outs);
        for (v, span) in outs {
            match v.inputs() {
                [x] => {
                    mono = Some(match mono {
                        Some(m) => self.op2(Op2::Add, m, *x),
                        None => *x,
                    })
                }
                ins if ins.len() == n => {
                    channels = Some(match channels {
                        Some(chs) => chs
                            .iter()
                            .zip(ins)
                            .map(|(&a, &b)| self.op2(Op2::Add, a, b))
                            .collect(),
                        None => ins.to_vec(),
                    })
                }
                ins => self.errors.push(
                    Diagnostic::error(
                        span,
                        format!("`out` got {} channels, but the output has {n}", ins.len()),
                    )
                    .with_help("send one channel to play it everywhere, or one per output channel"),
                ),
            }
        }
        match (mono, channels) {
            (None, None) => {}
            (Some(m), None) => self.graph.out(m),
            (m, Some(chs)) => {
                let chs: Vec<Input> = match m {
                    Some(m) => chs.iter().map(|&c| self.op2(Op2::Add, c, m)).collect(),
                    None => chs,
                };
                self.graph.out_channels(chs);
            }
        }
    }
}
