//! Name resolution and type checking.
//!
//! Rules enforced here, beyond plain type agreement:
//!
//! - Units never appear or vanish implicitly: `440 + 1Hz` is an error, and
//!   `Freq / Freq` is a plain number.
//! - A rill taking a scalar can be applied to `[T; N]`; it runs once per
//!   channel and returns a frame (lifting). A fn or built-in takes exactly
//!   what it declares and never lifts.
//! - `state` lives only at the top of a rill body; only `state` can be
//!   assigned to.
//! - Every path through a rill ends in exactly one `return`.
//! - A `fn` is pure: it cannot call a rill. Anonymous fns are fns too: they
//!   read what they capture but cannot change it.
//! - Fns are values. A named fn fits a function type with fewer parameters
//!   when the rest have defaults. Rills are not values.
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
    /// Every named value: parameters, size parameters, `let`, `state`, and
    /// the parameters of anonymous fns. Indexed by [`BindingId`].
    pub bindings: Vec<Binding>,
    /// What each name in the program refers to, by the span of the name.
    /// Covers names in expressions, callees, named arguments, assignment
    /// targets and types. Declarations are in `bindings` and the program's
    /// definitions instead.
    pub resolutions: Vec<(Span, Resolution)>,
}

/// Index into [`Checked::bindings`].
pub type BindingId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    Param,
    /// A size parameter like `N` in `mix_down<N>`.
    Size,
    Let,
    State,
    /// A parameter of an anonymous fn.
    FnParam,
}

/// A named value and where it can be used.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub name: String,
    /// The name where it is declared.
    pub span: Span,
    pub kind: BindingKind,
    pub ty: Type,
    /// Where the name can be used: from the end of its declaration to the
    /// end of the enclosing block, or the whole definition for parameters.
    pub scope: Span,
    /// Index of the fn or rill it belongs to, into [`Checked::signatures`]
    /// (the same as the program's items).
    pub def: usize,
}

/// What a name refers to.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    Binding(BindingId),
    /// A fn or rill, by index into [`Checked::signatures`].
    Def(usize),
    Builtin(String),
    Constant(String),
    /// A note name like `F#4`.
    Note,
    /// A built-in type like `Sample`.
    Type,
}

/// Check `program`. On failure the list holds the errors and any warnings.
pub fn check(program: &Program) -> Result<Checked, Vec<Diagnostic>> {
    let (checked, diags) = check_partial(program);
    if diags.iter().any(Diagnostic::is_error) {
        Err(diags)
    } else {
        Ok(checked)
    }
}

/// Check `program` and return everything found, errors or not, for tools
/// such as editors. Where something is wrong, types are
/// [`Type::Error`]. The list holds the errors and warnings, by position.
pub fn check_partial(program: &Program) -> (Checked, Vec<Diagnostic>) {
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
        lambda_floor: None,
        tail_expect: None,
        calls: HashMap::new(),
        def_order: Vec::new(),
        bindings: Vec::new(),
        resolutions: Vec::new(),
        def_bindings: Vec::new(),
        scope_spans: Vec::new(),
        sizes: Vec::new(),
        current_def: 0,
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

    let warnings = c.diags.iter().filter(|d| !d.is_error()).cloned().collect();
    let (mut errors, others): (Vec<_>, Vec<_>) = c.diags.into_iter().partition(|d| d.is_error());
    // Errors first, so a stable sort keeps them ahead of warnings at the
    // same place.
    errors.extend(others);
    errors.sort_by_key(|d| d.span.start);
    let checked = Checked {
        types: c.types,
        signatures: c.signatures,
        warnings,
        bindings: c.bindings,
        resolutions: c.resolutions,
    };
    (checked, errors)
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
    id: BindingId,
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
    /// Inside an anonymous fn: the first scope that belongs to it. Variables
    /// in scopes below are captured and cannot be assigned.
    lambda_floor: Option<usize>,
    /// The type a fn body's final expression should have, handed to the
    /// next block checked (the body itself) so an anonymous fn there can
    /// take its types from it.
    tail_expect: Option<Type>,
    /// Caller -> (callee, call site), user definitions only.
    /// Mentioning a fn as a value counts as a call.
    calls: HashMap<String, Vec<(String, Span)>>,
    def_order: Vec<String>,
    bindings: Vec<Binding>,
    resolutions: Vec<(Span, Resolution)>,
    /// Per definition: the bindings of its size parameters and parameters.
    def_bindings: Vec<(Vec<BindingId>, Vec<BindingId>)>,
    /// The span of each scope in `scopes`, for [`Binding::scope`].
    scope_spans: Vec<Span>,
    /// Size parameters of the definition being looked at, for resolving
    /// sizes in types.
    sizes: Vec<(String, BindingId)>,
    /// Index of the definition being checked.
    current_def: usize,
}

impl Checker {
    fn resolve(&mut self, span: Span, r: Resolution) {
        self.resolutions.push((span, r));
    }

    fn new_binding(
        &mut self,
        name: &Ident,
        kind: BindingKind,
        ty: Type,
        scope: Span,
        def: usize,
    ) -> BindingId {
        self.bindings.push(Binding {
            name: name.name.clone(),
            span: name.span,
            kind,
            ty,
            scope,
            def,
        });
        self.bindings.len() - 1
    }

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
        let index = self.signatures.len();
        let generic_ids: Vec<BindingId> = d
            .generics
            .iter()
            .map(|g| self.new_binding(g, BindingKind::Size, Type::Num, d.span, index))
            .collect();
        self.sizes = generics
            .iter()
            .cloned()
            .zip(generic_ids.iter().copied())
            .collect();
        let mut param_ids = Vec::new();

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
            param_ids.push(self.new_binding(
                &p.name,
                BindingKind::Param,
                ty.clone(),
                d.body.span,
                index,
            ));
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
        self.sizes.clear();
        self.def_bindings.push((generic_ids, param_ids));

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
        let t = self.expr_expect(default, Some(ty));
        self.place = saved;
        if !coerces(&t, ty) {
            let e = mismatch(default.span, &format!("default for `{param}`"), ty, &t);
            self.report(e);
        }
    }

    fn resolve_type(&mut self, te: &TypeExpr, generics: &[String]) -> Type {
        match te {
            TypeExpr::Named(id) => {
                let t = self.named_type(id);
                if t != Type::Error {
                    self.resolve(id.span, Resolution::Type);
                }
                t
            }
            TypeExpr::Frame { elem, size, span } => {
                let elem_ty = self.resolve_type(elem, generics);
                if matches!(elem_ty.leaf(), Type::Fn(..)) {
                    let e = self
                        .error(*span, "frames cannot hold functions")
                        .with_help("pass the functions separately");
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
                        if let Some(&(_, b)) = self.sizes.iter().find(|(n, _)| *n == id.name) {
                            self.resolve(id.span, Resolution::Binding(b));
                        }
                        Size::Var(id.name.clone())
                    }
                };
                Type::Frame(Box::new(elem_ty), size)
            }
            TypeExpr::Fn { params, ret, .. } => {
                let params = params
                    .iter()
                    .map(|p| self.resolve_type(p, generics))
                    .collect();
                let ret = self.resolve_type(ret, generics);
                Type::Fn(params, Box::new(ret))
            }
        }
    }

    /// A built-in type by name, or an error.
    fn named_type(&mut self, id: &Ident) -> Type {
        match id.name.as_str() {
            "Sample" => Type::Sample,
            "Float" => Type::Float,
            "Int" => Type::Int,
            "Bool" => Type::Bool,
            "Freq" => Type::Freq,
            "Pitch" => Type::Pitch,
            "Time" => Type::Time,
            "Interval" => Type::Interval,
            "Gain" => Type::Gain,
            other => {
                const KNOWN: [&str; 9] = [
                    "Sample", "Float", "Int", "Bool", "Freq", "Pitch", "Time", "Interval", "Gain",
                ];
                let mut e = self.error(id.span, format!("unknown type `{other}`"));
                if let Some(help) = renamed_type(other) {
                    e = e.with_help(help);
                } else if let Some(s) = suggest(other, KNOWN) {
                    e = e.with_help(format!("did you mean `{s}`?"));
                }
                self.report(e);
                Type::Error
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

        self.current_def = index;
        let (generic_ids, param_ids) = self.def_bindings[index].clone();
        let mut scope = HashMap::new();
        for (g, &id) in sig.generics.iter().zip(&generic_ids) {
            scope.insert(
                g.clone(),
                Var {
                    ty: Type::Num,
                    kind: VarKind::Size,
                    id,
                },
            );
        }
        for (p, &id) in sig.params.iter().zip(&param_ids) {
            scope.insert(
                p.name.clone(),
                Var {
                    ty: p.ty.clone(),
                    kind: VarKind::Param,
                    id,
                },
            );
        }
        self.scopes = vec![scope];
        self.scope_spans = vec![d.span];
        self.sizes = sig.generics.iter().cloned().zip(generic_ids).collect();

        if sig.kind == DefKind::Fn {
            self.tail_expect = Some(sig.ret.clone());
        }
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
        let tail = self.tail_expect.take();
        self.scopes.push(HashMap::new());
        self.scope_spans.push(b.span);
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
            let (t, div) = match (s, &tail) {
                (Stmt::Expr(e), Some(expected)) if last => {
                    let expected = expected.clone();
                    let t = self.expr_expect(e, Some(&expected));
                    let never = t == Type::Never;
                    (t, never)
                }
                _ => self.stmt(s, allow_state, last),
            };
            diverged |= div;
            if last && matches!(s, Stmt::Expr(_)) {
                value = t;
            }
        }
        self.scopes.pop();
        self.scope_spans.pop();
        if diverged {
            (Type::Never, true)
        } else {
            (value, false)
        }
    }

    /// Bind `name` in the innermost scope, usable from `visible_from` on.
    fn bind(&mut self, name: &Ident, ty: Type, kind: VarKind, visible_from: u32) {
        let end = self.scope_spans.last().map_or(visible_from, |s| s.end);
        let binding_kind = match kind {
            VarKind::Param => BindingKind::Param,
            VarKind::Let => BindingKind::Let,
            VarKind::State => BindingKind::State,
            VarKind::Size => BindingKind::Size,
        };
        let scope = Span {
            start: visible_from,
            end,
        };
        let id = self.new_binding(name, binding_kind, ty.clone(), scope, self.current_def);
        self.scopes
            .last_mut()
            .expect("a scope is always open")
            .insert(name.name.clone(), Var { ty, kind, id });
    }

    fn lookup(&self, name: &str) -> Option<&Var> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    /// Index of the scope that binds `name`.
    fn lookup_depth(&self, name: &str) -> Option<usize> {
        self.scopes.iter().rposition(|s| s.contains_key(name))
    }

    fn stmt(&mut self, s: &Stmt, allow_state: bool, last: bool) -> (Type, bool) {
        match s {
            Stmt::Let {
                name,
                ty,
                value,
                span,
            } => {
                let declared = ty
                    .as_ref()
                    .map(|te| self.resolve_type(te, &self.generics()));
                let t = self.expr_expect(value, declared.as_ref());
                let bound = match declared {
                    Some(declared) => {
                        if !coerces(&t, &declared) {
                            let e =
                                mismatch(value.span, &format!("`{}`", name.name), &declared, &t);
                            self.report(e);
                        }
                        declared
                    }
                    None => t.clone(),
                };
                self.bind(name, bound, VarKind::Let, span.end);
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
                if matches!(bound, Type::Fn(..)) {
                    let e = self.error(*span, "a function cannot be `state`").with_help(
                        "state holds numbers that change between ticks; bind functions with `let`",
                    );
                    self.report(e);
                }
                self.bind(name, bound, VarKind::State, span.end);
                (Type::Unit, false)
            }
            Stmt::Assign { target, value, .. } => {
                let t = self.expr(value);
                if let Some(var) = self.lookup(&target.name) {
                    let id = var.id;
                    self.resolve(target.span, Resolution::Binding(id));
                }
                let captured = matches!(
                    (self.lambda_floor, self.lookup_depth(&target.name)),
                    (Some(floor), Some(depth)) if depth < floor
                );
                match self.lookup(&target.name).cloned() {
                    Some(_) if captured => {
                        let e = self
                            .error(target.span, format!("an anonymous fn cannot change `{}`", target.name))
                            .with_help("fns are pure, including anonymous ones: they can read what they capture, but not change it");
                        self.report(e);
                    }
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
                let expected = self.ret.clone();
                let t = self.expr_expect(value, Some(&expected));
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
                self.scope_spans.push(body.span);
                for param in params {
                    let ty = if param.name == "cc" {
                        Type::Sample
                    } else {
                        Type::Event
                    };
                    self.bind(param, ty, VarKind::Let, param.span.end);
                }
                let saved = std::mem::replace(&mut self.in_event, true);
                let (_, diverged) = self.block(body, false);
                self.in_event = saved;
                self.scopes.pop();
                self.scope_spans.pop();
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
                if let Some(var) = self.lookup(name).cloned() {
                    self.resolve(e.span, Resolution::Binding(var.id));
                    return var.ty;
                }
                if pitch_literal(name).is_some() {
                    self.resolve(e.span, Resolution::Note);
                    return Type::Pitch;
                }
                if let Some(t) = builtins::constant(name) {
                    self.resolve(e.span, Resolution::Constant(name.clone()));
                    return t.clone();
                }
                if self.defs.contains_key(name) || !builtins::lookup(name).is_empty() {
                    return self.fn_value(name, e.span, None);
                }
                let d = self.unknown_name(name, e.span);
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
                    UnOp::Neg | UnOp::Plus => {
                        let t = t.leaf();
                        t.is_plain() || t.is_dimensioned() || *t == Type::Gain
                    }
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
                        self.error(cond.span, format!("condition must be `Bool`, found `{tc}`"));
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
                    if !t.leaf().is_quantity() {
                        let d = self.error(
                            el.span,
                            format!("a frame channel must be a number or a frame, found `{t}`"),
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
                                    let msg = if prev.depth() > 0 || t.depth() > 0 {
                                        format!("frame elements have different shapes: `{prev}` and `{t}`")
                                    } else {
                                        format!("frame channels have different types: `{prev}` and `{t}`")
                                    };
                                    let d = self.error(el.span, msg);
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
                if !ti.is_wild() && !matches!(ti, Type::Int | Type::Num) {
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
            ExprKind::Fn { params, ret, body } => self.lambda(e, params, ret.as_ref(), body, None),
            ExprKind::Cast(x, te) => {
                let from = self.expr(x);
                let to = self.resolve_type(te, &self.generics());
                self.cast(e.span, &from, to)
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
        let name = callee.name.as_str();

        // A variable holding a function.
        if let Some(var) = self.lookup(name).cloned() {
            self.resolve(callee.span, Resolution::Binding(var.id));
            if let Type::Fn(params, ret) = &var.ty {
                return self.call_value(span, callee, args, params, ret);
            }
            self.exprs(args);
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
            self.exprs(args);
            let mut d = self.error(callee.span, format!("unknown fn or rill `{name}`"));
            if let Some(help) = renamed_conversion(name) {
                self.report(d.with_help(help));
                return Type::Error;
            }
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

        let def = self.defs.get(name).copied();
        self.resolve(
            callee.span,
            match def {
                Some(i) => Resolution::Def(i),
                None => Resolution::Builtin(name.to_owned()),
            },
        );
        let kind = sigs[0].kind;
        if kind == DefKind::Rill && self.place == Place::Fn {
            let d = if self.lambda_floor.is_some() {
                self.error(callee.span, format!("an anonymous fn cannot call rill `{name}`"))
                    .with_help("fns stay pure, including anonymous ones; call the rill outside and pass its value in")
            } else {
                let me = self.current.clone().unwrap_or_default();
                self.error(callee.span, format!("fn `{me}` cannot call rill `{name}`"))
                    .with_help(format!(
                        "rills keep state between ticks, so fns must stay pure; make `{me}` a rill"
                    ))
            };
            self.report(d);
        }
        if kind != DefKind::Builtin {
            self.note_use(name, callee.span);
        }

        // Built-ins overloaded by arity (like `min`) are picked by argument
        // count; everything else has one signature, with defaults.
        let sig = if sigs.len() == 1 {
            sigs[0].clone()
        } else if let Some(s) = sigs.iter().find(|s| s.params.len() == args.len()) {
            s.clone()
        } else {
            self.exprs(args);
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
                        Some(pi) => {
                            if let Some(i) = def {
                                let param = self.def_bindings[i].1[pi];
                                self.resolve(id.span, Resolution::Binding(param));
                            }
                            pi
                        }
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

        // Check every argument once, with its parameter's type as the
        // expectation when that is a function type.
        let mut arg_types = vec![Type::Error; args.len()];
        for (ai, arg) in args.iter().enumerate() {
            let expected = slots
                .iter()
                .position(|s| *s == Some(ai))
                .map(|pi| &sig.params[pi].ty)
                .filter(|t| matches!(t, Type::Fn(..)));
            arg_types[ai] = self.expr_expect(&arg.value, expected);
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

        // Unify argument types with parameter types. A rill whose parameter
        // receives a value with more frame layers runs once per element of
        // the extra layers (lifting); fns and built-ins never lift, except
        // the built-ins documented to take a frame in their first parameter.
        let mut subst = Subst::default();
        let mut lift: Option<(Vec<Size>, Span)> = None;
        for (pi, (p, slot)) in sig.params.iter().zip(&slots).enumerate() {
            let Some(ai) = *slot else { continue };
            let at = &arg_types[ai];
            let arg_span = args[ai].value.span;
            if at.is_wild() {
                ok = false;
                continue;
            }
            // Peel as few outer layers as it takes for the argument to fit.
            let mut peeled = Vec::new();
            let mut inner = at;
            let fitted = loop {
                let mut trial = subst.clone();
                if unify(&p.ty, inner, &mut trial, &sig.generics) {
                    break Some(trial);
                }
                match inner {
                    Type::Frame(elem, n) => {
                        peeled.push(n.clone());
                        inner = elem;
                    }
                    _ => break None,
                }
            };
            if let Some(trial) = fitted {
                if peeled.is_empty() {
                    subst = trial;
                    continue;
                }
                let lifts = kind == DefKind::Rill
                    || (kind == DefKind::Builtin && pi == 0 && builtins::takes_frames(name));
                if !lifts {
                    let msg = if matches!(p.ty, Type::Frame(..)) {
                        format!("`{name}` takes `{}`, not `{at}`", substitute(&p.ty, &trial))
                    } else {
                        format!("`{name}` takes one value, not a frame (`{at}`)")
                    };
                    let help = if kind == DefKind::Fn {
                        format!(
                            "fns take exactly what they declare; give `{name}` a size parameter, \
                             as in `fn {name}<N>(x: [Sample; N])`, or make it a rill to run it per channel"
                        )
                    } else {
                        "built-in functions take one value; to run one per channel, call it from a \
                         rill and apply that rill to the frame"
                            .to_owned()
                    };
                    let d = self.error(arg_span, msg).with_help(help);
                    self.report(d);
                    ok = false;
                    continue;
                }
                match &lift {
                    Some((earlier, _)) if *earlier != peeled => {
                        let msg = match (earlier.as_slice(), peeled.as_slice()) {
                            ([m], [n]) => format!(
                                "channel counts differ: this has {n} channels, an earlier argument has {m}"
                            ),
                            _ => format!(
                                "this runs `{name}` over shape `{}`, an earlier argument over shape `{}`",
                                shape(&peeled),
                                shape(earlier)
                            ),
                        };
                        let d = self
                            .error(arg_span, msg)
                            .with_help(
                                "every argument that runs per element needs the same extra layers",
                            );
                        self.report(d);
                        ok = false;
                    }
                    Some(_) => {}
                    None => lift = Some((peeled, arg_span)),
                }
                subst = trial;
                continue;
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
            Some((layers, _)) => layers
                .into_iter()
                .rev()
                .fold(ret, |t, n| Type::Frame(Box::new(t), n)),
        }
    }

    /// Check each argument with no expectation, so that errors inside them
    /// are still reported when the call itself is broken.
    fn exprs(&mut self, args: &[Arg]) {
        for a in args {
            self.expr(&a.value);
        }
    }

    /// Record that the current definition uses `name`, for the recursion
    /// check.
    fn note_use(&mut self, name: &str, at: Span) {
        if let Some(me) = &self.current {
            self.calls
                .entry(me.clone())
                .or_default()
                .push((name.to_owned(), at));
        }
    }

    /// A call through a variable holding a function.
    fn call_value(
        &mut self,
        span: Span,
        callee: &Ident,
        args: &[Arg],
        params: &[Type],
        ret: &Type,
    ) -> Type {
        let name = &callee.name;
        let mut ok = true;
        for (i, a) in args.iter().enumerate() {
            let expected = params.get(i).filter(|t| matches!(t, Type::Fn(..)));
            let t = self.expr_expect(&a.value, expected);
            if let Some(id) = &a.name {
                let d = self
                    .error(
                        id.span,
                        format!("`{name}` is a function value, so its arguments have no names"),
                    )
                    .with_help("pass the arguments in order");
                self.report(d);
                ok = false;
                continue;
            }
            let Some(p) = params.get(i) else { continue };
            if t.is_wild() || coerces(&t, p) {
                continue;
            }
            let d = if let (Type::Frame(elem, _), false) = (&t, matches!(p, Type::Frame(..)))
                && coerces(elem, p)
            {
                self.error(a.value.span, format!("`{name}` takes one value, not a frame (`{t}`)"))
                    .with_help("function values never run per channel; call it once per channel, or from a rill")
            } else {
                mismatch(
                    a.value.span,
                    &format!("argument {} of `{name}`", i + 1),
                    p,
                    &t,
                )
            };
            self.report(d);
            ok = false;
        }
        if args.len() != params.len() {
            let d = self.error(
                span,
                format!(
                    "`{name}` takes {} argument(s), but {} were given",
                    params.len(),
                    args.len()
                ),
            );
            self.report(d);
            ok = false;
        }
        if ok { ret.clone() } else { Type::Error }
    }

    /// Check `e`, letting an anonymous fn or the name of a fn take its types
    /// from `expected` when that is a function type.
    fn expr_expect(&mut self, e: &Expr, expected: Option<&Type>) -> Type {
        let expected = expected.filter(|t| matches!(t, Type::Fn(..)));
        let t = match (&e.kind, expected) {
            (ExprKind::Fn { params, ret, body }, _) => {
                self.lambda(e, params, ret.as_ref(), body, expected)
            }
            (ExprKind::Name(name), Some(exp))
                if self.lookup(name).is_none()
                    && (self.defs.contains_key(name) || !builtins::lookup(name).is_empty()) =>
            {
                self.fn_value(name, e.span, Some(exp))
            }
            _ => return self.expr(e),
        };
        self.types[e.id as usize] = t.clone();
        t
    }

    /// The type of `name` (a fn or built-in, not a variable) used as a
    /// value, fitted to `expected` if given.
    fn fn_value(&mut self, name: &str, span: Span, expected: Option<&Type>) -> Type {
        let sigs = match self.defs.get(name) {
            Some(&i) => vec![self.signatures[i].clone()],
            None => builtins::lookup(name),
        };
        let r = match self.defs.get(name) {
            Some(&i) => Resolution::Def(i),
            None => Resolution::Builtin(name.to_owned()),
        };
        self.resolve(span, r);
        let kind = sigs[0].kind;
        if kind == DefKind::Rill {
            let d = self
                .error(span, format!("rill `{name}` cannot be used as a value"))
                .with_help(format!(
                    "only fns can be passed around; call it, as in `{name}(x)` or `x |> {name}`"
                ));
            self.report(d);
            return Type::Error;
        }
        if kind == DefKind::Fn {
            self.note_use(name, span);
        }

        if let Some(Type::Fn(eps, eret)) = expected {
            if sigs.iter().any(|s| fits(s, eps, eret)) {
                return expected.cloned().unwrap();
            }
            let shown = sigs
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(" or ");
            let d = self
                .error(
                    span,
                    format!("`{name}` does not fit `{}`", expected.unwrap()),
                )
                .with_help(format!("`{name}` is {shown}"));
            self.report(d);
            return Type::Error;
        }

        let sig = &sigs[0];
        let generic = sigs.len() > 1
            || !sig.generics.is_empty()
            || sig.params.iter().any(|p| contains_param(&p.ty))
            || contains_param(&sig.ret);
        if generic {
            let help = if kind == DefKind::Fn {
                format!("`{name}` has size parameters, so it cannot be used as a value yet")
            } else {
                format!(
                    "`{name}` works on several types; say which one where it goes, as in `let f: fn(Sample) Sample = {name}`"
                )
            };
            let d = self
                .error(span, format!("cannot tell which `{name}` is meant here"))
                .with_help(help);
            self.report(d);
            return Type::Error;
        }
        Type::Fn(
            sig.params.iter().map(|p| p.ty.clone()).collect(),
            Box::new(sig.ret.clone()),
        )
    }

    /// An anonymous fn. Parameter types come from annotations or, failing
    /// that, from `expected`; the return type from an annotation, `expected`
    /// or the body.
    fn lambda(
        &mut self,
        e: &Expr,
        params: &[FnParam],
        ret: Option<&TypeExpr>,
        body: &Block,
        expected: Option<&Type>,
    ) -> Type {
        let (eps, eret) = match expected {
            Some(Type::Fn(ps, r)) => (Some(ps.clone()), Some((**r).clone())),
            _ => (None, None),
        };
        if let Some(eps) = &eps
            && eps.len() != params.len()
        {
            let d = self.error(
                e.span,
                format!(
                    "this fn takes {} parameter(s), but `{}` is expected here",
                    params.len(),
                    expected.unwrap()
                ),
            );
            self.report(d);
        }

        let generics = self.generics();
        let mut scope = HashMap::new();
        let mut param_types = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let ty = match (&p.ty, eps.as_ref().and_then(|ps| ps.get(i))) {
                (Some(te), _) => self.resolve_type(te, &generics),
                (None, Some(t)) => t.clone(),
                (None, None) => {
                    let d = self
                        .error(
                            p.name.span,
                            format!("cannot tell the type of `{}`", p.name.name),
                        )
                        .with_help(format!("annotate it, as in `{}: Pitch`", p.name.name));
                    self.report(d);
                    Type::Error
                }
            };
            let id = self.new_binding(
                &p.name,
                BindingKind::FnParam,
                ty.clone(),
                body.span,
                self.current_def,
            );
            scope.insert(
                p.name.name.clone(),
                Var {
                    ty: ty.clone(),
                    kind: VarKind::Param,
                    id,
                },
            );
            param_types.push(ty);
        }
        let declared = ret.map(|te| self.resolve_type(te, &generics)).or(eret);

        // The body is a fn body: pure, with its own return type.
        let saved_place = std::mem::replace(&mut self.place, Place::Fn);
        let saved_ret = std::mem::replace(&mut self.ret, declared.clone().unwrap_or(Type::Error));
        let saved_event = std::mem::replace(&mut self.in_event, false);
        let saved_floor = self.lambda_floor.replace(self.scopes.len());
        self.scopes.push(scope);
        self.scope_spans.push(body.span);
        self.tail_expect = declared.clone();
        let (t, diverges) = self.block(body, false);
        self.scopes.pop();
        self.scope_spans.pop();
        self.place = saved_place;
        self.ret = saved_ret;
        self.in_event = saved_event;
        self.lambda_floor = saved_floor;

        let ret = match declared {
            Some(r) => {
                if !diverges && !coerces(&t, &r) {
                    let at = body.stmts.last().map_or(body.span, Stmt::span);
                    let d = self.error(
                        at,
                        format!("this fn should return `{r}`, but its body produces `{t}`"),
                    );
                    self.report(d);
                }
                r
            }
            None if diverges => {
                let d = self
                    .error(e.span, "cannot tell what this fn returns")
                    .with_help("annotate it, as in `fn(p: Pitch) Freq { ... }`");
                self.report(d);
                Type::Error
            }
            None => t.settle(),
        };
        Type::Fn(param_types, Box::new(ret))
    }

    /// `x as to`. Plain numbers convert between `Sample`, `Float` and
    /// `Int`; units and levels never disappear by a cast.
    fn cast(&mut self, span: Span, from: &Type, to: Type) -> Type {
        if from.is_wild() || to.is_wild() {
            return Type::Error;
        }
        if !matches!(to, Type::Sample | Type::Float | Type::Int) {
            let d = self
                .error(span, format!("cannot cast to `{to}`"))
                .with_help("`as` converts between `Sample`, `Float` and `Int`");
            self.report(d);
            return Type::Error;
        }
        if from.is_plain() {
            return to;
        }
        let help = match from {
            Type::Freq => {
                Some("units never disappear on their own; divide by one, as in `x / 1Hz`")
            }
            Type::Time => Some("units never disappear on their own; divide by one, as in `x / 1s`"),
            Type::Interval => {
                Some("units never disappear on their own; divide by one, as in `x / 1st`")
            }
            Type::Gain => Some("use `amp(x)` for the amplitude factor of a level"),
            Type::Bool => Some("pick the numbers yourself, as in `if b { 1 } else { 0 }`"),
            Type::Frame(..) => Some("cast the channels one at a time, as in `x[0] as Float`"),
            _ => None,
        };
        let mut d = self.error(span, format!("cannot cast `{from}` to `{to}`"));
        if let Some(h) = help {
            d = d.with_help(h);
        }
        self.report(d);
        Type::Error
    }

    /// Known before audio starts: literals, built-in constants, size
    /// parameters, and built-in functions of those.
    fn is_const(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Number { .. } | ExprKind::Bool(_) => true,
            ExprKind::Unary(_, x) | ExprKind::Cast(x, _) => self.is_const(x),
            ExprKind::Binary(_, a, b) => self.is_const(a) && self.is_const(b),
            ExprKind::Frame(xs) => xs.iter().all(|x| self.is_const(x)),
            ExprKind::Name(n) => match self.lookup(n) {
                Some(v) => v.kind == VarKind::Size,
                None => {
                    builtins::constant(n).is_some()
                        || pitch_literal(n).is_some()
                        // A named fn is a fixed value.
                        || self.defs.get(n).is_some_and(|&i| self.signatures[i].kind == DefKind::Fn)
                        || !builtins::lookup(n).is_empty()
                }
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
            | ExprKind::Field(..)
            | ExprKind::Fn { .. } => false,
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

/// Help for a type written with its old name.
fn renamed_type(name: &str) -> Option<String> {
    Some(match name {
        "Hz" => "`Hz` is the unit for literals like `440Hz`; the type is `Freq`".to_owned(),
        "sample" | "f32" | "i32" | "bool" => {
            let new = match name {
                "sample" => "Sample",
                "f32" => "Float",
                "i32" => "Int",
                _ => "Bool",
            };
            format!("`{name}` is now called `{new}`")
        }
        _ => return None,
    })
}

/// Help for a conversion called by its old name.
fn renamed_conversion(name: &str) -> Option<&'static str> {
    Some(match name {
        "sample" | "Sample" => "convert with `as`, as in `x as Sample`",
        "f32" | "Float" => "convert with `as`, as in `x as Float`",
        "i32" | "Int" => "convert with `as`, as in `x as Int`",
        _ => return None,
    })
}

fn var_word(kind: VarKind) -> &'static str {
    match kind {
        VarKind::Param => "parameter",
        VarKind::Let => "value",
        VarKind::State => "state variable",
        VarKind::Size => "size",
    }
}

/// A frame shape for messages, outer layer first: `4 × 2`.
fn shape(sizes: &[Size]) -> String {
    sizes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" × ")
}

fn describe_param(c: &str) -> &'static str {
    match c {
        "T" => "a plain number (`Sample`, `Float` or `Int`)",
        "F" => "a plain number or a frame of them",
        "S" => "a number",
        _ => "something else",
    }
}

const NOTE_NAME_HELP: &str = "write a pitch as a note name, like `A4` or `F#3`";

const PITCH_ARITH_HELP: &str = "a pitch is a position, not an amount: add or subtract an interval \
     (`A4 + 7st`), or subtract two pitches to get the interval between them";

fn unit_example(t: &Type) -> &'static str {
    match t {
        Type::Freq => "440Hz",
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
    if *found == Type::Num && *expected == Type::Pitch {
        return d.with_help(NOTE_NAME_HELP);
    }
    if *found == Type::Num && expected.is_dimensioned() {
        return d.with_help(format!(
            "give the number a unit, as in `{}`",
            unit_example(expected)
        ));
    }
    if found.is_plain() && *expected == Type::Int {
        return d.with_help("convert it with `as`, as in `x as Int`");
    }
    if *found == Type::Int && expected.is_plain() {
        return d.with_help(format!("convert it with `{expected}(...)`"));
    }
    if let (Type::Frame(..), false) = (found, matches!(expected, Type::Frame(..))) {
        return d
            .with_help("reduce the channels first, e.g. with `sum(...)`, or pick one with `x[0]`");
    }
    d
}

/// Can a fn with signature `sig` be used as a value of type
/// `fn(params) ret`? Its extra parameters must have defaults.
fn fits(sig: &Signature, params: &[Type], ret: &Type) -> bool {
    if params.len() > sig.params.len() || sig.params[params.len()..].iter().any(|p| !p.has_default)
    {
        return false;
    }
    let mut subst = Subst::default();
    for (p, given) in sig.params.iter().zip(params) {
        if !unify(&p.ty, given, &mut subst, &sig.generics) {
            return false;
        }
    }
    let actual = substitute(&sig.ret, &subst);
    !matches!(actual, Type::Error) && coerces(&actual, ret)
}

/// Does `t` mention a built-in type parameter?
fn contains_param(t: &Type) -> bool {
    match t {
        Type::Param(_) => true,
        Type::Frame(e, _) => contains_param(e),
        Type::Fn(ps, r) => ps.iter().any(contains_param) || contains_param(r),
        _ => false,
    }
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
                "`{}` needs `Bool` on both sides, found `{a}` and `{b}`",
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
        (a, b) => a == b && (a.is_dimensioned() || matches!(a, Type::Pitch | Type::Gain)),
    };
    if ok {
        return Ok(Type::Bool);
    }
    let help = if (*a == Type::Num && *b == Type::Pitch) || (*b == Type::Num && *a == Type::Pitch) {
        Some(NOTE_NAME_HELP.to_owned())
    } else if *a == Type::Gain || *b == Type::Gain {
        Some("write the level in dB, as in `-6dB`".to_owned())
    } else if (*a == Type::Num && b.is_dimensioned()) || (*b == Type::Num && a.is_dimensioned()) {
        let dim = if a.is_dimensioned() { a } else { b };
        Some(format!(
            "give the number a unit, as in `{}`",
            unit_example(dim)
        ))
    } else if *a == Type::Bool && *b == Type::Bool {
        Some("only `==` and `!=` work on `Bool`".into())
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

    if *a == Type::Gain || *b == Type::Gain {
        return gain_arith(op, a, b);
    }

    let verb = match op {
        BinOp::Add => "add",
        BinOp::Sub => "subtract",
        BinOp::Mul => "multiply",
        BinOp::Div => "divide",
        _ => "take the remainder of",
    };
    let fail = || {
        let help = if *a == Type::Pitch || *b == Type::Pitch {
            Some(PITCH_ARITH_HELP.to_owned())
        } else if (*a == Type::Num && b.is_dimensioned()) || (*b == Type::Num && a.is_dimensioned())
        {
            let dim = if a.is_dimensioned() { a } else { b };
            Some(format!(
                "give the number a unit, as in `{}`",
                unit_example(dim)
            ))
        } else if (*a == Type::Int && b.is_plain()) || (*b == Type::Int && a.is_plain()) {
            Some("convert the `Int` with `as Float` or `as Sample`".into())
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
    if matches!(
        (op, a, b),
        (BinOp::Add | BinOp::Sub, Type::Pitch, Type::Interval)
    ) {
        return Ok(Type::Pitch);
    }
    if matches!((op, a, b), (BinOp::Add, Type::Interval, Type::Pitch)) {
        return Err((
            "cannot add `Interval` and `Pitch`".into(),
            Some("the interval comes after the pitch, as in `C4 + 7st`".into()),
        ));
    }
    if matches!((op, a, b), (BinOp::Sub, Type::Pitch, Type::Pitch)) {
        return Ok(Type::Interval);
    }
    if matches!(
        (op, a, b),
        (BinOp::Add | BinOp::Sub, Type::Interval, Type::Interval)
    ) {
        return Ok(Type::Interval);
    }
    if matches!(
        (op, a, b),
        (BinOp::Mul, Type::Interval, p) | (BinOp::Mul, p, Type::Interval) if p.is_plain()
    ) {
        return Ok(Type::Interval);
    }
    if matches!((op, a, b), (BinOp::Div, Type::Interval, p) if p.is_plain()) {
        return Ok(Type::Interval);
    }
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
            (Type::Freq, Type::Time) | (Type::Time, Type::Freq) => Ok(Type::Float),
            _ => fail(),
        },
        BinOp::Div => match (a, b) {
            (d, p) if d.is_dimensioned() && p.is_plain() => Ok(d.clone()),
            (x, y) if x == y => Ok(Type::Float),
            (p, Type::Freq) if p.is_plain() => Ok(Type::Time),
            (p, Type::Time) if p.is_plain() => Ok(Type::Freq),
            _ => fail(),
        },
        _ => unreachable!("not an arithmetic operator"),
    }
}

/// Arithmetic involving a `Gain`. A signal moves up or down in level with
/// `+` and `-`, the level always on the right; levels combine with `+`/`-`
/// and scale with `*`/`/` by plain numbers.
fn gain_arith(op: BinOp, a: &Type, b: &Type) -> Result<Type, OpError> {
    use Type::{Float, Gain, Int, Num, Sample};
    let verb = match op {
        BinOp::Add => "add",
        BinOp::Sub => "subtract",
        BinOp::Mul => "multiply",
        BinOp::Div => "divide",
        _ => "take the remainder of",
    };
    let err = |help: Option<&str>| {
        Err((
            format!("cannot {verb} `{a}` and `{b}`"),
            help.map(str::to_owned),
        ))
    };
    match (op, a, b) {
        (BinOp::Add | BinOp::Sub, Num | Sample | Float, Gain) => Ok(a.clone()),
        (BinOp::Add | BinOp::Sub, Gain, Gain) => Ok(Gain),
        (BinOp::Add | BinOp::Sub, Int, Gain) => err(Some(
            "integers have no level; convert with `as Float` first",
        )),
        (BinOp::Add | BinOp::Sub, Gain, _) => err(Some(
            "the level comes after the signal, as in `voice - 6dB`",
        )),
        (BinOp::Mul | BinOp::Div, Sample, Gain) | (BinOp::Mul, Gain, Sample) => err(Some(
            "to change a signal's level, add or subtract it, as in `voice - 6dB`",
        )),
        (BinOp::Mul, Gain, Num | Float | Int) | (BinOp::Mul, Num | Float | Int, Gain) => Ok(Gain),
        (BinOp::Div, Gain, Num | Float | Int) => Ok(Gain),
        (BinOp::Div, Gain, Gain) => Ok(Float),
        _ => err(None),
    }
}

fn event_field_type(name: &str) -> Option<Type> {
    match name {
        "pitch" => Some(Type::Pitch),
        "velocity" | "release" => Some(Type::Sample),
        "channel" | "index" => Some(Type::Int),
        _ => None,
    }
}

pub fn pitch_literal(name: &str) -> Option<f32> {
    let bytes = name.as_bytes();
    let letter = *bytes.first()? as char;
    let base = match letter {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let mut i = 1usize;
    let accidental = match bytes.get(i).copied() {
        Some(b's') | Some(b'#') => {
            i += 1;
            1
        }
        Some(b'b') => {
            i += 1;
            -1
        }
        _ => 0,
    };
    let octave = if i == bytes.len() {
        4
    } else {
        name[i..].parse::<i32>().ok()?
    };
    Some(((octave + 1) * 12 + base + accidental) as f32)
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
                format!("add one, as in `rill {entry}() Sample {{ return 0 }}`")
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
    for (p, ps) in def.params.iter().zip(&sig.params) {
        if matches!(ps.ty, Type::Fn(..)) {
            errors.push(
                Diagnostic::error(
                    p.name.span,
                    format!("`{}` cannot be a function", p.name.name),
                )
                .with_help("the entry rill's parameters are live controls, so they hold numbers"),
            );
        }
    }
    for (p, ps) in def.params.iter().zip(&sig.params) {
        if ps.ty.depth() > 1 {
            errors.push(
                Diagnostic::error(
                    p.name.span,
                    format!("`{}` cannot be a frame of frames", p.name.name),
                )
                .with_help("the entry rill's parameters are live controls; use a flat frame"),
            );
        }
    }
    for p in def.params.iter().filter(|p| p.default.is_none()) {
        errors.push(
            Diagnostic::error(p.name.span, format!("`{}` needs a default value", p.name.name)).with_help(
                "the entry rill's parameters are the program's controls, so each needs a starting value, \
                 as in `freq: Freq = 440Hz`",
            ),
        );
    }
    if let Some(rate) = &def.rate {
        errors.push(Diagnostic::error(
            rate.span,
            "the entry rill cannot change the sample rate",
        ));
    }
    let audio = |t: &Type| matches!(t, Type::Sample | Type::Float | Type::Num);
    let ok_ret = match &sig.ret {
        Type::Frame(elem, Size::Const(_)) => audio(elem),
        t => audio(t),
    };
    if sig.ret.depth() > 1 && audio(sig.ret.leaf()) {
        errors.push(
            Diagnostic::error(
                def.ret.span(),
                format!(
                    "the entry rill must return flat audio (`Sample` or `[Sample; N]`), found `{}`",
                    sig.ret
                ),
            )
            .with_help("mix the outer layer down with `sum`, as in `return sum(voices)`"),
        );
    } else if !ok_ret && !sig.ret.is_wild() {
        errors.push(Diagnostic::error(
            def.ret.span(),
            format!(
                "the entry rill must return audio (`Sample` or `[Sample; N]`), found `{}`",
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
