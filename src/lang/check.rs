//! Name resolution and type checking.
//!
//! Rules enforced here, beyond plain type agreement:
//!
//! - Units never appear or vanish implicitly: `440 + 1Hz` is an error, and
//!   `Hz / Hz` is a plain number.
//! - A rill taking a scalar can be applied to `[T; N]`; it runs once per
//!   channel and returns a frame (lifting). A fn or built-in takes exactly
//!   what it declares and never lifts.
//! - `state` lives only at the top of a rill body; only `state` can be
//!   assigned to.
//! - Every path through a rill ends in exactly one `return`.
//! - A `fn` is pure: it cannot call a rill.
//! - No recursion: the run stage has no unbounded loops and every rill
//!   instance needs a fixed amount of state.
//! - Defaults and `state` initial values are known before audio starts.
//! - The entry rill (see [`check_entry`]) can run with no arguments and
//!   returns audio.

use std::collections::{HashMap, HashSet};

use super::ast::*;
use super::builtins;
use super::diag::{Diagnostic, Span, suggest};
use super::types::{DefKind, ParamSig, Signature, Size, Type, coerces, join};

/// Result of a successful check.
#[derive(Clone, Debug)]
pub struct Checked {
    /// Type of every expression, indexed by [`Expr::id`].
    pub types: Vec<Type>,
    /// Every fn and rill, in source order.
    pub signatures: Vec<Signature>,
    pub warnings: Vec<Diagnostic>,
}

/// Check `program`. On failure the list holds the errors and any warnings.
pub fn check(program: &Program) -> Result<Checked, Vec<Diagnostic>> {
    let mut c = Checker {
        defs: HashMap::new(),
        signatures: Vec::new(),
        types: vec![Type::Error; program.expr_count as usize],
        diags: Vec::new(),
        scopes: Vec::new(),
        place: Place::Fn,
        current: None,
        ret: Type::Unit,
        in_event: false,
        calls: HashMap::new(),
        def_order: Vec::new(),
    };

    for item in &program.items {
        match item {
            Item::Fn(d) => c.declare(d, DefKind::Fn),
            Item::Rill(d) => c.declare(d, DefKind::Rill),
        }
    }
    for (index, item) in program.items.iter().enumerate() {
        c.check_def(item.def(), index);
    }

    c.check_recursion();

    let (errors, warnings): (Vec<_>, Vec<_>) = c.diags.into_iter().partition(|d| d.is_error());
    if errors.is_empty() {
        Ok(Checked {
            types: c.types,
            signatures: c.signatures,
            warnings,
        })
    } else {
        let mut all = errors;
        all.extend(warnings);
        all.sort_by_key(|d| d.span.start);
        Err(all)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Fn,
    Rill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VarKind {
    Param,
    Let,
    State,
    /// A size parameter like `N`, usable as a number.
    Size,
}

#[derive(Clone, Debug)]
struct Var {
    ty: Type,
    kind: VarKind,
}

#[derive(Clone, Debug, Default)]
struct Subst {
    types: HashMap<&'static str, Type>,
    sizes: HashMap<String, Size>,
}

type OpError = (String, Option<String>);

struct Checker {
    /// Name -> index into `signatures` (first definition wins).
    defs: HashMap<String, usize>,
    signatures: Vec<Signature>,
    types: Vec<Type>,
    diags: Vec<Diagnostic>,
    scopes: Vec<HashMap<String, Var>>,
    place: Place,
    current: Option<String>,
    ret: Type,
    in_event: bool,
    /// Caller -> (callee, call site), user definitions only.
    calls: HashMap<String, Vec<(String, Span)>>,
    def_order: Vec<String>,
}

impl Checker {
    fn error(&mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error(span, message)
    }

    fn report(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    // ---- declarations -------------------------------------------------

    fn declare(&mut self, d: &Def, kind: DefKind) {
        let name = &d.name.name;
        if self.defs.contains_key(name) {
            let e = self.error(d.name.span, format!("`{name}` is defined more than once"));
            self.report(e);
        } else if builtins::constant(name).is_some() {
            let e = self.error(d.name.span, format!("`{name}` is a built-in constant"));
            self.report(e);
        }

        let mut generics: Vec<String> = Vec::new();
        for g in &d.generics {
            if generics.contains(&g.name) {
                let e = self.error(
                    g.span,
                    format!("size parameter `{}` is declared twice", g.name),
                );
                self.report(e);
            }
            generics.push(g.name.clone());
        }

        let mut params = Vec::new();
        let mut seen = HashSet::new();
        for p in &d.params {
            if !seen.insert(p.name.name.clone()) {
                let e = self.error(
                    p.name.span,
                    format!("parameter `{}` is declared twice", p.name.name),
                );
                self.report(e);
            }
            let ty = self.resolve_type(&p.ty, &generics);
            if let Some(default) = &p.default {
                self.check_default(default, &ty, &p.name.name);
            }
            params.push(ParamSig {
                name: p.name.name.clone(),
                ty,
                has_default: p.default.is_some(),
            });
        }
        let ret = self.resolve_type(&d.ret, &generics);

        for g in &d.generics {
            let used = params.iter().any(|p| mentions_size(&p.ty, &g.name));
            if !used {
                let e = self
                    .error(g.span, format!("size `{}` is not used by any parameter", g.name))
                    .with_help("sizes are inferred from the arguments, so each one must appear in a parameter type");
                self.report(e);
            }
        }

        let rate = match (&d.rate, kind) {
            (Some(r), DefKind::Fn) => {
                let e = self
                    .error(r.span, "only rills can change the sample rate")
                    .with_help("a fn maps one value to one value; make it a rill");
                self.report(e);
                (1, 1)
            }
            (Some(r), _) => (r.num, r.den),
            (None, _) => (1, 1),
        };

        let sig = Signature {
            kind,
            name: name.clone(),
            generics,
            params,
            ret,
            rate,
        };
        if !self.defs.contains_key(name) {
            self.defs.insert(name.clone(), self.signatures.len());
            self.def_order.push(name.clone());
        }
        self.signatures.push(sig);
    }

    fn check_default(&mut self, default: &Expr, ty: &Type, param: &str) {
        if !self.is_const(default) {
            let e = self
                .error(default.span, format!("default for `{param}` must be a constant"))
                .with_help("defaults are fixed before audio starts; use literals, built-in constants and built-in functions");
            self.report(e);
            return;
        }
        let saved = std::mem::replace(&mut self.place, Place::Fn);
        let t = self.expr(default);
        self.place = saved;
        if !coerces(&t, ty) {
            let e = mismatch(default.span, &format!("default for `{param}`"), ty, &t);
            self.report(e);
        }
    }

    fn resolve_type(&mut self, te: &TypeExpr, generics: &[String]) -> Type {
        match te {
            TypeExpr::Named(id) => match id.name.as_str() {
                "sample" => Type::Sample,
                "f32" => Type::F32,
                "i32" => Type::I32,
                "bool" => Type::Bool,
                "Hz" => Type::Hz,
                "Time" => Type::Time,
                "Interval" => Type::Interval,
                "Pitch" | "Chord" | "Tuning" => {
                    let e = self.error(id.span, format!("`{}` is not supported yet", id.name));
                    self.report(e);
                    Type::Error
                }
                other => {
                    const KNOWN: [&str; 7] =
                        ["sample", "f32", "i32", "bool", "Hz", "Time", "Interval"];
                    let mut e = self.error(id.span, format!("unknown type `{other}`"));
                    if let Some(s) = suggest(other, KNOWN) {
                        e = e.with_help(format!("did you mean `{s}`?"));
                    }
                    self.report(e);
                    Type::Error
                }
            },
            TypeExpr::Frame { elem, size, span } => {
                let elem_ty = self.resolve_type(elem, generics);
                if matches!(elem_ty, Type::Frame(..)) {
                    let e = self.error(*span, "frames cannot contain frames");
                    self.report(e);
                    return Type::Error;
                }
                let size = match size {
                    SizeExpr::Lit(n, _) => Size::Const(*n),
                    SizeExpr::Var(id) => {
                        if !generics.contains(&id.name) {
                            let e = self
                                .error(id.span, format!("unknown size `{}`", id.name))
                                .with_help(format!(
                                    "declare it after the name, as in `rill f<{}>(...)`",
                                    id.name
                                ));
                            self.report(e);
                            return Type::Error;
                        }
                        Size::Var(id.name.clone())
                    }
                };
                Type::Frame(Box::new(elem_ty), size)
            }
        }
    }

    // ---- bodies -------------------------------------------------------

    fn check_def(&mut self, d: &Def, index: usize) {
        let sig = self.signatures[index].clone();
        self.place = if sig.kind == DefKind::Rill {
            Place::Rill
        } else {
            Place::Fn
        };
        self.current = Some(d.name.name.clone());
        self.ret = sig.ret.clone();

        let mut scope = HashMap::new();
        for g in &sig.generics {
            scope.insert(
                g.clone(),
                Var {
                    ty: Type::Num,
                    kind: VarKind::Size,
                },
            );
        }
        for p in &sig.params {
            scope.insert(
                p.name.clone(),
                Var {
                    ty: p.ty.clone(),
                    kind: VarKind::Param,
                },
            );
        }
        self.scopes = vec![scope];

        let (ty, diverges) = self.block(&d.body, sig.kind == DefKind::Rill);
        let name = &d.name.name;
        if sig.kind == DefKind::Rill && !diverges {
            let ends_in_expr = matches!(d.body.stmts.last(), Some(Stmt::Expr(_)));
            let e = self
                .error(d.name.span, format!("not every path through rill `{name}` returns"))
                .with_help(if ends_in_expr {
                    "a rill produces its output with `return`; add `return` before the final expression"
                } else {
                    "every tick must produce exactly one output; end each path with `return`"
                });
            self.report(e);
        } else if sig.kind == DefKind::Fn && !diverges && !coerces(&ty, &sig.ret) {
            let at = d.body.stmts.last().map_or(d.body.span, Stmt::span);
            let e = self.error(
                at,
                format!(
                    "fn `{name}` should return `{}`, but its body produces `{ty}`",
                    sig.ret
                ),
            );
            self.report(e);
        }
    }

    /// Check a block. Returns its value type and whether every path through
    /// it returns.
    fn block(&mut self, b: &Block, allow_state: bool) -> (Type, bool) {
        self.scopes.push(HashMap::new());
        let mut value = Type::Unit;
        let mut diverged = false;
        for (i, s) in b.stmts.iter().enumerate() {
            if diverged {
                let e = self
                    .error(s.span(), "unreachable code")
                    .with_help("every path before this has already returned");
                self.report(e);
                break;
            }
            let last = i + 1 == b.stmts.len();
            let (t, div) = self.stmt(s, allow_state, last);
            diverged |= div;
            if last && matches!(s, Stmt::Expr(_)) {
                value = t;
            }
        }
        self.scopes.pop();
        if diverged {
            (Type::Never, true)
        } else {
            (value, false)
        }
    }

    fn bind(&mut self, name: &str, ty: Type, kind: VarKind) {
        self.scopes
            .last_mut()
            .expect("a scope is always open")
            .insert(name.to_owned(), Var { ty, kind });
    }

    fn lookup(&self, name: &str) -> Option<&Var> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn stmt(&mut self, s: &Stmt, allow_state: bool, last: bool) -> (Type, bool) {
        match s {
            Stmt::Let {
                name, ty, value, ..
            } => {
                let t = self.expr(value);
                let bound = match ty {
                    Some(te) => {
                        let declared = self.resolve_type(te, &self.generics());
                        if !coerces(&t, &declared) {
                            let e =
                                mismatch(value.span, &format!("`{}`", name.name), &declared, &t);
                            self.report(e);
                        }
                        declared
                    }
                    None => t.clone(),
                };
                self.bind(&name.name, bound, VarKind::Let);
                (Type::Unit, t == Type::Never)
            }
            Stmt::State {
                name,
                ty,
                init,
                span,
            } => {
                if self.place != Place::Rill {
                    let mut e = self.error(*span, "`state` is only allowed in rills");
                    if self.place == Place::Fn {
                        e = e.with_help(
                            "fns are pure; make this a rill to keep values between ticks",
                        );
                    }
                    self.report(e);
                } else if !allow_state {
                    let e = self
                        .error(
                            *span,
                            "`state` must be declared at the top level of the rill body",
                        )
                        .with_help("state exists for every tick, so it cannot depend on a branch");
                    self.report(e);
                }
                if !self.is_const(init) {
                    let e = self
                        .error(init.span, "the initial value of `state` must be a constant")
                        .with_help("state is allocated and initialised before audio starts");
                    self.report(e);
                }
                let t = self.expr(init);
                let bound = match ty {
                    Some(te) => {
                        let declared = self.resolve_type(te, &self.generics());
                        if !coerces(&t, &declared) {
                            let e = mismatch(init.span, &format!("`{}`", name.name), &declared, &t);
                            self.report(e);
                        }
                        declared
                    }
                    None => t.settle(),
                };
                self.bind(&name.name, bound, VarKind::State);
                (Type::Unit, false)
            }
            Stmt::Assign { target, value, .. } => {
                let t = self.expr(value);
                match self.lookup(&target.name).cloned() {
                    Some(var)
                        if var.kind == VarKind::State
                            || (self.in_event && var.kind == VarKind::Param) =>
                    {
                        if !coerces(&t, &var.ty) {
                            let e =
                                mismatch(value.span, &format!("`{}`", target.name), &var.ty, &t);
                            self.report(e);
                        }
                    }
                    Some(_) => {
                        let e = self
                            .error(target.span, format!("cannot assign to `{}`", target.name))
                            .with_help("only `state` variables change between ticks; `let` bindings and parameters are fixed");
                        self.report(e);
                    }
                    None => {
                        let e = self.unknown_name(&target.name, target.span);
                        self.report(e);
                    }
                }
                (Type::Unit, t == Type::Never)
            }
            Stmt::Return { value, .. } => {
                let t = self.expr(value);
                if !coerces(&t, &self.ret) {
                    let what = format!("`{}`", self.current.as_deref().unwrap_or("?"));
                    let ret = self.ret.clone();
                    let e = mismatch(value.span, &format!("return value of {what}"), &ret, &t);
                    self.report(e);
                }
                (Type::Never, true)
            }
            Stmt::EventHandler {
                name: _,
                params,
                body,
                ..
            } => {
                if self.place != Place::Rill {
                    let e = self.error(s.span(), "event handlers are only allowed in rills");
                    self.report(e);
                    return (Type::Unit, false);
                }
                self.scopes.push(HashMap::new());
                for param in params {
                    let ty = if param.name == "cc" {
                        Type::Sample
                    } else {
                        Type::Event
                    };
                    self.bind(&param.name, ty, VarKind::Let);
                }
                let saved = std::mem::replace(&mut self.in_event, true);
                let (_, diverged) = self.block(body, false);
                self.in_event = saved;
                self.scopes.pop();
                (Type::Unit, diverged)
            }
            Stmt::Expr(e) => {
                let t = self.expr(e);
                let has_value = !matches!(t, Type::Unit | Type::Error | Type::Never);
                if has_value && !last {
                    self.report(Diagnostic::warning(e.span, "this value is never used"));
                }
                (t.clone(), t == Type::Never)
            }
        }
    }

    fn generics(&self) -> Vec<String> {
        self.scopes
            .first()
            .map(|s| {
                s.iter()
                    .filter(|(_, v)| v.kind == VarKind::Size)
                    .map(|(n, _)| n.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---- expressions --------------------------------------------------

    fn expr(&mut self, e: &Expr) -> Type {
        let t = self.expr_inner(e);
        self.types[e.id as usize] = t.clone();
        t
    }

    fn expr_inner(&mut self, e: &Expr) -> Type {
        match &e.kind {
            ExprKind::Number { unit, .. } => {
                unit.map_or(Type::Num, |u| Type::from_dimension(u.dimension()))
            }
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Name(name) => {
                if let Some(var) = self.lookup(name) {
                    return var.ty.clone();
                }
                if let Some(t) = builtins::constant(name) {
                    return t.clone();
                }
                let kind = match self.defs.get(name) {
                    Some(&i) => Some(self.signatures[i].kind),
                    None if !builtins::lookup(name).is_empty() => Some(DefKind::Builtin),
                    None => None,
                };
                let d = match kind {
                    Some(k) => self
                        .error(
                            e.span,
                            format!("`{name}` is a {} and must be called", k.word()),
                        )
                        .with_help(format!(
                            "call it, as in `{name}(x)`, or pipe into it with `x |> {name}`"
                        )),
                    None => self.unknown_name(name, e.span),
                };
                self.report(d);
                Type::Error
            }
            ExprKind::Unary(op, x) => {
                let t = self.expr(x);
                if t.is_wild() {
                    return Type::Error;
                }
                let ok = match op {
                    UnOp::Not => t == Type::Bool,
                    UnOp::Neg | UnOp::Plus => match &t {
                        Type::Frame(elem, _) => elem.is_plain() || elem.is_dimensioned(),
                        t => t.is_plain() || t.is_dimensioned(),
                    },
                };
                if ok {
                    t
                } else {
                    let verb = if *op == UnOp::Not {
                        "apply `!` to"
                    } else {
                        "negate"
                    };
                    let d = self.error(e.span, format!("cannot {verb} `{t}`"));
                    self.report(d);
                    Type::Error
                }
            }
            ExprKind::Binary(op, a, b) => {
                let ta = self.expr(a);
                let tb = self.expr(b);
                let result = match op {
                    BinOp::And | BinOp::Or => logic(*op, &ta, &tb),
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne => {
                        compare(*op, &ta, &tb)
                    }
                    _ => arith(*op, &ta, &tb),
                };
                match result {
                    Ok(t) => t,
                    Err((msg, help)) => {
                        let mut d = self.error(e.span, msg);
                        if let Some(h) = help {
                            d = d.with_help(h);
                        }
                        self.report(d);
                        Type::Error
                    }
                }
            }
            ExprKind::Call { callee, args, .. } => self.call(e.span, callee, args),
            ExprKind::If { cond, then, els } => {
                let tc = self.expr(cond);
                if !coerces(&tc, &Type::Bool) {
                    let mut d =
                        self.error(cond.span, format!("condition must be `bool`, found `{tc}`"));
                    if tc.is_plain() || tc.is_dimensioned() {
                        d = d.with_help("compare it, as in `x > 0`");
                    }
                    self.report(d);
                }
                let (t1, _) = self.block(then, false);
                let Some(els) = els else {
                    return Type::Unit;
                };
                let t2 = self.expr(els);
                match join(&t1, &t2) {
                    Some(t) => t,
                    None => {
                        let d = self.error(
                            e.span,
                            format!("`if` and `else` have different types: `{t1}` and `{t2}`"),
                        );
                        self.report(d);
                        Type::Error
                    }
                }
            }
            ExprKind::Block(b) => self.block(b, false).0,
            ExprKind::Frame(elems) => {
                let mut acc: Option<Type> = None;
                let mut bad = false;
                for el in elems {
                    let t = self.expr(el);
                    if t.is_wild() {
                        bad = true;
                        continue;
                    }
                    if !(t.is_plain() || t.is_dimensioned()) {
                        let d = self.error(
                            el.span,
                            format!("a frame channel must be a number, found `{t}`"),
                        );
                        self.report(d);
                        bad = true;
                        continue;
                    }
                    acc = match acc {
                        None => Some(t),
                        Some(prev) => {
                            match join(&prev, &t) {
                                Some(j) => Some(j),
                                None => {
                                    let d = self.error(
                                    el.span,
                                    format!("frame channels have different types: `{prev}` and `{t}`"),
                                );
                                    self.report(d);
                                    bad = true;
                                    Some(prev)
                                }
                            }
                        }
                    };
                }
                match acc {
                    Some(t) if !bad => Type::Frame(Box::new(t), Size::Const(elems.len() as u32)),
                    _ => Type::Error,
                }
            }
            ExprKind::Index(base, index) => {
                let tb = self.expr(base);
                let ti = self.expr(index);
                if !ti.is_wild() && !matches!(ti, Type::I32 | Type::Num) {
                    let d = self.error(
                        index.span,
                        format!("channel index must be a whole number, found `{ti}`"),
                    );
                    self.report(d);
                }
                match tb {
                    Type::Frame(elem, size) => {
                        if let ExprKind::Number {
                            value,
                            integral,
                            unit: None,
                        } = index.kind
                        {
                            if !integral {
                                let d =
                                    self.error(index.span, "channel index must be a whole number");
                                self.report(d);
                            } else if let Size::Const(n) = size
                                && value >= f64::from(n)
                            {
                                let d = self.error(
                                    index.span,
                                    format!("channel {value} is out of range for a frame of {n}"),
                                );
                                self.report(d);
                            }
                        }
                        *elem
                    }
                    t if t.is_wild() => Type::Error,
                    t => {
                        let d = self
                            .error(base.span, format!("cannot index `{t}`"))
                            .with_help("only frames have channels to index");
                        self.report(d);
                        Type::Error
                    }
                }
            }
            ExprKind::Field(base, field) => {
                let base_ty = self.expr(base);
                if base_ty == Type::Event {
                    event_field_type(&field.name).unwrap_or_else(|| {
                        let e =
                            self.error(field.span, format!("unknown event field `{}`", field.name));
                        self.report(e);
                        Type::Error
                    })
                } else if base_ty.is_wild() {
                    Type::Error
                } else {
                    let e = self.error(base.span, format!("cannot read fields from `{base_ty}`"));
                    self.report(e);
                    Type::Error
                }
            }
        }
    }

    fn unknown_name(&self, name: &str, span: Span) -> Diagnostic {
        let mut candidates: Vec<&str> = self
            .scopes
            .iter()
            .flat_map(|s| s.keys().map(String::as_str))
            .collect();
        candidates.extend(builtins::CONSTANTS.iter().map(|(n, _)| *n));
        let d = Diagnostic::error(span, format!("unknown name `{name}`"));
        match suggest(name, candidates) {
            Some(s) => d.with_help(format!("did you mean `{s}`?")),
            None => d,
        }
    }

    fn call(&mut self, span: Span, callee: &Ident, args: &[Arg]) -> Type {
        let arg_types: Vec<Type> = args.iter().map(|a| self.expr(&a.value)).collect();
        let name = callee.name.as_str();

        if let Some(var) = self.lookup(name) {
            let d = self.error(
                callee.span,
                format!(
                    "`{name}` is a {} of type `{}`, not a fn or rill",
                    var_word(var.kind),
                    var.ty
                ),
            );
            self.report(d);
            return Type::Error;
        }

        let sigs = match self.defs.get(name) {
            Some(&i) => vec![self.signatures[i].clone()],
            None => builtins::lookup(name),
        };
        if sigs.is_empty() {
            let mut d = self.error(callee.span, format!("unknown fn or rill `{name}`"));
            let candidates = self
                .def_order
                .iter()
                .map(String::as_str)
                .chain(builtins::FUNCTIONS.iter().copied());
            if let Some(s) = suggest(name, candidates) {
                d = d.with_help(format!("did you mean `{s}`?"));
            }
            self.report(d);
            return Type::Error;
        }

        let kind = sigs[0].kind;
        if kind == DefKind::Rill && self.place == Place::Fn {
            let me = self.current.clone().unwrap_or_default();
            let d = self
                .error(callee.span, format!("fn `{me}` cannot call rill `{name}`"))
                .with_help(format!(
                    "rills keep state between ticks, so fns must stay pure; make `{me}` a rill"
                ));
            self.report(d);
        }
        if kind != DefKind::Builtin
            && let Some(me) = &self.current
        {
            self.calls
                .entry(me.clone())
                .or_default()
                .push((name.to_owned(), callee.span));
        }

        let sig = match sigs.iter().find(|s| s.params.len() == args.len()) {
            Some(s) if kind == DefKind::Builtin => s.clone(),
            _ if kind == DefKind::Builtin => {
                let counts: Vec<String> = sigs.iter().map(|s| s.params.len().to_string()).collect();
                let d = self.error(
                    span,
                    format!(
                        "`{name}` takes {} argument(s), but {} were given",
                        counts.join(" or "),
                        args.len()
                    ),
                );
                self.report(d);
                return Type::Error;
            }
            _ => sigs[0].clone(),
        };

        // Match arguments to parameters.
        let mut slots: Vec<Option<usize>> = vec![None; sig.params.len()];
        let mut ok = true;
        let mut seen_named = false;
        for (ai, arg) in args.iter().enumerate() {
            let pi = match &arg.name {
                None => {
                    if seen_named {
                        let d = self.error(
                            arg.value.span,
                            "positional arguments must come before named ones",
                        );
                        self.report(d);
                        ok = false;
                        continue;
                    }
                    if ai >= sig.params.len() {
                        let d = self
                            .error(
                                arg.value.span,
                                format!(
                                    "`{name}` takes {} argument(s), but {} were given",
                                    sig.params.len(),
                                    args.len()
                                ),
                            )
                            .with_help(format!("`{sig}`"));
                        self.report(d);
                        ok = false;
                        break;
                    }
                    ai
                }
                Some(id) => {
                    seen_named = true;
                    match sig.params.iter().position(|p| p.name == id.name) {
                        Some(pi) => pi,
                        None => {
                            let mut d = self
                                .error(id.span, format!("`{name}` has no parameter `{}`", id.name));
                            if let Some(s) =
                                suggest(&id.name, sig.params.iter().map(|p| p.name.as_str()))
                            {
                                d = d.with_help(format!("did you mean `{s}`?"));
                            }
                            self.report(d);
                            ok = false;
                            continue;
                        }
                    }
                }
            };
            if slots[pi].is_some() {
                let d = self.error(
                    arg.value.span,
                    format!("argument `{}` is given more than once", sig.params[pi].name),
                );
                self.report(d);
                ok = false;
                continue;
            }
            slots[pi] = Some(ai);
        }
        for (p, slot) in sig.params.iter().zip(&slots) {
            if slot.is_none() && !p.has_default && ok {
                let d = self
                    .error(span, format!("missing argument `{}` for `{name}`", p.name))
                    .with_help(format!("`{sig}`"));
                self.report(d);
                ok = false;
            }
        }
        if !ok {
            return Type::Error;
        }

        // Unify argument types with parameter types. A rill whose scalar
        // parameter receives a frame runs once per channel (lifting); fns and
        // built-ins never lift.
        let mut subst = Subst::default();
        let mut lift: Option<(Size, Span)> = None;
        for (p, slot) in sig.params.iter().zip(&slots) {
            let Some(ai) = *slot else { continue };
            let at = &arg_types[ai];
            let arg_span = args[ai].value.span;
            if at.is_wild() {
                ok = false;
                continue;
            }
            let mut trial = subst.clone();
            if unify(&p.ty, at, &mut trial, &sig.generics) {
                subst = trial;
                continue;
            }
            if let Type::Frame(elem, n) = at
                && !matches!(p.ty, Type::Frame(..))
            {
                let mut trial = subst.clone();
                if unify(&p.ty, elem, &mut trial, &sig.generics) {
                    if kind != DefKind::Rill {
                        let help = if kind == DefKind::Fn {
                            format!(
                                "fns take exactly what they declare; give `{name}` a size parameter, \
                                 as in `fn {name}<N>(x: [sample; N])`, or make it a rill to run it per channel"
                            )
                        } else {
                            "built-in functions take one value; to run one per channel, call it from a \
                             rill and apply that rill to the frame"
                                .to_owned()
                        };
                        let d = self
                            .error(
                                arg_span,
                                format!("`{name}` takes one value, not a frame (`{at}`)"),
                            )
                            .with_help(help);
                        self.report(d);
                        ok = false;
                        continue;
                    }
                    match &lift {
                        Some((m, _)) if m != n => {
                            let d = self.error(
                                arg_span,
                                format!("channel counts differ: this has {n} channels, an earlier argument has {m}"),
                            );
                            self.report(d);
                            ok = false;
                        }
                        Some(_) => {}
                        None => lift = Some((n.clone(), arg_span)),
                    }
                    subst = trial;
                    continue;
                }
            }
            let what = format!("argument `{}` of `{name}`", p.name);
            let d = match &p.ty {
                Type::Param(c) => Diagnostic::error(
                    arg_span,
                    format!("{what} must be {}, found `{at}`", describe_param(c)),
                ),
                expected => mismatch(arg_span, &what, &substitute(expected, &subst), at),
            };
            self.report(d);
            ok = false;
        }
        if !ok {
            return Type::Error;
        }

        let ret = substitute(&sig.ret, &subst);
        match lift {
            None => ret,
            Some(_) if ret == Type::Unit => Type::Unit,
            Some((_, at)) if matches!(ret, Type::Frame(..)) => {
                let d = self.error(
                    at,
                    format!(
                        "cannot run `{name}` once per channel: it already returns a frame (`{ret}`)"
                    ),
                );
                self.report(d);
                Type::Error
            }
            Some((n, _)) => Type::Frame(Box::new(ret), n),
        }
    }

    /// Known before audio starts: literals, built-in constants, size
    /// parameters, and built-in functions of those.
    fn is_const(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Number { .. } | ExprKind::Bool(_) => true,
            ExprKind::Unary(_, x) => self.is_const(x),
            ExprKind::Binary(_, a, b) => self.is_const(a) && self.is_const(b),
            ExprKind::Frame(xs) => xs.iter().all(|x| self.is_const(x)),
            ExprKind::Name(n) => match self.lookup(n) {
                Some(v) => v.kind == VarKind::Size,
                None => builtins::constant(n).is_some(),
            },
            ExprKind::Call { callee, args, .. } => {
                let name = callee.name.as_str();
                self.lookup(name).is_none()
                    && !self.defs.contains_key(name)
                    && !builtins::lookup(name).is_empty()
                    && args.iter().all(|a| self.is_const(&a.value))
            }
            ExprKind::If { .. }
            | ExprKind::Block(_)
            | ExprKind::Index(..)
            | ExprKind::Field(..) => false,
        }
    }

    fn check_recursion(&mut self) {
        let mut reported: HashSet<Vec<String>> = HashSet::new();
        for start in self.def_order.clone() {
            // Depth-first search for a path back to `start`.
            let mut stack: Vec<(String, usize)> = vec![(start.clone(), 0)];
            let mut on_path: Vec<String> = vec![start.clone()];
            let mut visited: HashSet<String> = HashSet::new();
            while let Some((node, next)) = stack.last().cloned() {
                let edges = self.calls.get(&node).cloned().unwrap_or_default();
                if next >= edges.len() {
                    stack.pop();
                    on_path.pop();
                    continue;
                }
                stack.last_mut().unwrap().1 += 1;
                let (callee, at) = &edges[next];
                if *callee == start {
                    let mut key = on_path.clone();
                    key.sort();
                    if reported.insert(key) {
                        let mut path = on_path.clone();
                        path.push(start.clone());
                        let chain = path
                            .iter()
                            .map(|n| format!("`{n}`"))
                            .collect::<Vec<_>>()
                            .join(" -> ");
                        let d = Diagnostic::error(*at, format!("recursion is not allowed: {chain}"))
                            .with_help("the run stage has no unbounded loops, and every rill instance needs a fixed amount of state");
                        self.report(d);
                    }
                    continue;
                }
                if visited.insert(callee.clone()) {
                    stack.push((callee.clone(), 0));
                    on_path.push(callee.clone());
                }
            }
        }
    }
}

fn var_word(kind: VarKind) -> &'static str {
    match kind {
        VarKind::Param => "parameter",
        VarKind::Let => "value",
        VarKind::State => "state variable",
        VarKind::Size => "size",
    }
}

fn describe_param(c: &str) -> &'static str {
    match c {
        "T" => "a plain number (`sample`, `f32` or `i32`)",
        "S" => "a number",
        _ => "something else",
    }
}

fn unit_example(t: &Type) -> &'static str {
    match t {
        Type::Hz => "440Hz",
        Type::Time => "300ms",
        _ => "7st",
    }
}

/// "`what` expects `expected`, found `found`", with a hint for the common
/// mistakes.
fn mismatch(span: Span, what: &str, expected: &Type, found: &Type) -> Diagnostic {
    let d = Diagnostic::error(
        span,
        format!("{what} expects `{expected}`, found `{found}`"),
    );
    if *found == Type::Num && expected.is_dimensioned() {
        return d.with_help(format!(
            "give the number a unit, as in `{}`",
            unit_example(expected)
        ));
    }
    if found.is_plain() && *expected == Type::I32 {
        return d.with_help("convert it with `i32(...)`");
    }
    if *found == Type::I32 && expected.is_plain() {
        return d.with_help(format!("convert it with `{expected}(...)`"));
    }
    if let (Type::Frame(..), false) = (found, matches!(expected, Type::Frame(..))) {
        return d
            .with_help("reduce the channels first, e.g. with `sum(...)`, or pick one with `x[0]`");
    }
    d
}

fn mentions_size(t: &Type, name: &str) -> bool {
    matches!(t, Type::Frame(_, Size::Var(v)) if v == name)
}

fn unify(param: &Type, arg: &Type, subst: &mut Subst, generics: &[String]) -> bool {
    match param {
        Type::Param(c) => {
            if !builtins::satisfies(c, arg) {
                return false;
            }
            let joined = match subst.types.get(c) {
                Some(bound) => match join(bound, arg) {
                    Some(j) => j,
                    None => return false,
                },
                None => arg.clone(),
            };
            subst.types.insert(c, joined);
            true
        }
        Type::Frame(pe, psize) => {
            let Type::Frame(ae, asize) = arg else {
                return false;
            };
            let size_ok = match psize {
                Size::Var(v) if generics.contains(v) => match subst.sizes.get(v) {
                    Some(bound) => bound == asize,
                    None => {
                        subst.sizes.insert(v.clone(), asize.clone());
                        true
                    }
                },
                fixed => fixed == asize,
            };
            size_ok && unify(pe, ae, subst, generics)
        }
        _ => coerces(arg, param),
    }
}

fn substitute(t: &Type, subst: &Subst) -> Type {
    match t {
        Type::Param(c) => subst.types.get(c).cloned().unwrap_or(Type::Error),
        Type::Frame(elem, size) => {
            let size = match size {
                Size::Var(v) => subst.sizes.get(v).cloned().unwrap_or_else(|| size.clone()),
                s => s.clone(),
            };
            Type::Frame(Box::new(substitute(elem, subst)), size)
        }
        t => t.clone(),
    }
}

fn logic(op: BinOp, a: &Type, b: &Type) -> Result<Type, OpError> {
    if a.is_wild() || b.is_wild() {
        return Ok(Type::Error);
    }
    if *a == Type::Bool && *b == Type::Bool {
        Ok(Type::Bool)
    } else {
        Err((
            format!(
                "`{}` needs `bool` on both sides, found `{a}` and `{b}`",
                op.symbol()
            ),
            None,
        ))
    }
}

fn compare(op: BinOp, a: &Type, b: &Type) -> Result<Type, OpError> {
    if a.is_wild() || b.is_wild() {
        return Ok(Type::Error);
    }
    if matches!(a, Type::Frame(..)) || matches!(b, Type::Frame(..)) {
        return Err((
            "frames cannot be compared".into(),
            Some("compare channels one at a time, as in `x[0] > y[0]`".into()),
        ));
    }
    let equality = matches!(op, BinOp::Eq | BinOp::Ne);
    let ok = match (a, b) {
        (Type::Bool, Type::Bool) => equality,
        (a, b) if a.is_plain() && b.is_plain() => join(a, b).is_some(),
        (a, b) => a == b && a.is_dimensioned(),
    };
    if ok {
        return Ok(Type::Bool);
    }
    let help = if (*a == Type::Num && b.is_dimensioned()) || (*b == Type::Num && a.is_dimensioned())
    {
        let dim = if a.is_dimensioned() { a } else { b };
        Some(format!(
            "give the number a unit, as in `{}`",
            unit_example(dim)
        ))
    } else if *a == Type::Bool && *b == Type::Bool {
        Some("only `==` and `!=` work on `bool`".into())
    } else {
        None
    };
    Err((
        format!("cannot compare `{a}` and `{b}` with `{}`", op.symbol()),
        help,
    ))
}

fn arith(op: BinOp, a: &Type, b: &Type) -> Result<Type, OpError> {
    if a.is_wild() || b.is_wild() {
        return Ok(Type::Error);
    }
    match (a, b) {
        (Type::Frame(ea, na), Type::Frame(eb, nb)) => {
            if na != nb {
                return Err((
                    format!("channel counts differ: `{a}` and `{b}`"),
                    Some("operators on frames work channel by channel, so both sides need the same count".into()),
                ));
            }
            return Ok(Type::Frame(Box::new(arith(op, ea, eb)?), na.clone()));
        }
        (Type::Frame(ea, n), s) => return Ok(Type::Frame(Box::new(arith(op, ea, s)?), n.clone())),
        (s, Type::Frame(eb, n)) => return Ok(Type::Frame(Box::new(arith(op, s, eb)?), n.clone())),
        _ => {}
    }

    let verb = match op {
        BinOp::Add => "add",
        BinOp::Sub => "subtract",
        BinOp::Mul => "multiply",
        BinOp::Div => "divide",
        _ => "take the remainder of",
    };
    let fail = || {
        let help = if (*a == Type::Num && b.is_dimensioned())
            || (*b == Type::Num && a.is_dimensioned())
        {
            let dim = if a.is_dimensioned() { a } else { b };
            Some(format!(
                "give the number a unit, as in `{}`",
                unit_example(dim)
            ))
        } else if (*a == Type::I32 && b.is_plain()) || (*b == Type::I32 && a.is_plain()) {
            Some("convert the `i32` with `f32(...)` or `sample(...)`".into())
        } else if (a.is_plain() && b.is_dimensioned()) || (b.is_plain() && a.is_dimensioned()) {
            let (plain, dim) = if a.is_plain() { (a, b) } else { (b, a) };
            Some(format!(
                "`{plain}` has no unit; multiplying by a `{dim}` value gives it one, as in `x * {}`",
                unit_example(dim)
            ))
        } else {
            None
        };
        Err((format!("cannot {verb} `{a}` and `{b}`"), help))
    };

    let (pa, pb) = (a.is_plain(), b.is_plain());
    let (da, db) = (a.is_dimensioned(), b.is_dimensioned());
    if !(pa || da) || !(pb || db) {
        return fail();
    }
    if pa && pb {
        return join(a, b).map_or_else(fail, Ok);
    }
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Rem => {
            if a == b {
                Ok(a.clone())
            } else {
                fail()
            }
        }
        BinOp::Mul => match (a, b) {
            (d, p) | (p, d) if d.is_dimensioned() && p.is_plain() => Ok(d.clone()),
            (Type::Hz, Type::Time) | (Type::Time, Type::Hz) => Ok(Type::F32),
            _ => fail(),
        },
        BinOp::Div => match (a, b) {
            (d, p) if d.is_dimensioned() && p.is_plain() => Ok(d.clone()),
            (x, y) if x == y => Ok(Type::F32),
            (p, Type::Hz) if p.is_plain() => Ok(Type::Time),
            (p, Type::Time) if p.is_plain() => Ok(Type::Hz),
            _ => fail(),
        },
        _ => unreachable!("not an arithmetic operator"),
    }
}

fn event_field_type(name: &str) -> Option<Type> {
    match name {
        "pitch" => Some(Type::Hz),
        "velocity" | "release" => Some(Type::Sample),
        "channel" | "index" => Some(Type::I32),
        _ => None,
    }
}

/// Check that `entry` names a rill that can run as a whole program: it
/// takes no arguments that lack defaults, has no size parameters, does not
/// change rate, and returns audio.
pub fn check_entry(
    program: &Program,
    checked: &Checked,
    entry: &str,
) -> Result<(), Vec<Diagnostic>> {
    let found = program
        .items
        .iter()
        .zip(&checked.signatures)
        .find(|(item, _)| item.def().name.name == entry);
    let Some((item, sig)) = found else {
        let rills: Vec<&str> = program
            .items
            .iter()
            .filter(|i| matches!(i, Item::Rill(_)))
            .map(|i| i.def().name.name.as_str())
            .collect();
        let d = Diagnostic::error(
            Span::default(),
            format!("there is no rill named `{entry}` to run"),
        );
        let help = match suggest(entry, rills.iter().copied()) {
            Some(s) => format!("did you mean `{s}`?"),
            None if rills.is_empty() => {
                format!("add one, as in `rill {entry}() -> sample {{ return 0 }}`")
            }
            None => format!("pick one with `--entry`: {}", rills.join(", ")),
        };
        return Err(vec![d.with_help(help)]);
    };

    let def = item.def();
    let mut errors = Vec::new();
    if let Item::Fn(_) = item {
        errors.push(Diagnostic::error(
            def.name.span,
            format!("`{entry}` is a fn, but the program must start at a rill"),
        ));
    }
    if let Some(g) = def.generics.first() {
        errors.push(
            Diagnostic::error(g.span, "the entry rill cannot have size parameters").with_help(
                format!(
                    "nothing calls it, so there is nothing to infer `{}` from",
                    g.name
                ),
            ),
        );
    }
    for p in def.params.iter().filter(|p| p.default.is_none()) {
        errors.push(
            Diagnostic::error(p.name.span, format!("`{}` needs a default value", p.name.name)).with_help(
                "the entry rill's parameters are the program's controls, so each needs a starting value, \
                 as in `freq: Hz = 440Hz`",
            ),
        );
    }
    if let Some(rate) = &def.rate {
        errors.push(Diagnostic::error(
            rate.span,
            "the entry rill cannot change the sample rate",
        ));
    }
    let audio = |t: &Type| matches!(t, Type::Sample | Type::F32 | Type::Num);
    let ok_ret = match &sig.ret {
        Type::Frame(elem, Size::Const(_)) => audio(elem),
        t => audio(t),
    };
    if !ok_ret && !sig.ret.is_wild() {
        errors.push(Diagnostic::error(
            def.ret.span(),
            format!(
                "the entry rill must return audio (`sample` or `[sample; N]`), found `{}`",
                sig.ret
            ),
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
