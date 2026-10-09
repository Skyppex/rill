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
use super::types::{
    Align, DefKind, ParamSig, Signature, Size, Type, align, coerces, frame_shape, join,
};
use crate::event::{EventDecl as Declared, EventKind, Sender};

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
    /// The program's event declarations: the written ones in source order
    /// (the same as [`Program::events`]), then the events every sequence
    /// makes ([`Checked::seq_event`]). `None` where a declaration has an
    /// unknown kind.
    pub events: Vec<Option<Declared>>,
    /// How many of `events` are written in the program.
    pub written_events: usize,
    /// Per sequence, what is fixed about it.
    pub seq_facts: Vec<SeqFacts>,
    /// Values worked out while checking, by expression id: sequence fields
    /// (`riff.step_count`) and sizes written as expressions.
    pub const_values: HashMap<u32, f64>,
    /// The program's top-level `const`s, as [`Program::consts`].
    pub consts: Vec<ConstInfo>,
    /// The top-level `const`s in an order where each comes after the ones
    /// it uses, by index into [`Checked::consts`]. A `const` in a cycle is
    /// left out.
    pub const_order: Vec<usize>,
    /// Per module, the top-level names it can use (one, for a program from
    /// one string).
    pub scopes: Vec<Scope>,
}

/// What a top-level name stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Global {
    /// A fn or rill, by index into [`Checked::signatures`].
    Def(usize),
    /// An event, by index into [`Checked::events`].
    Event(usize),
    /// A sequence, by index into the program's sequences.
    Seq(usize),
    /// A top-level `const`, by index into [`Checked::consts`].
    Const(usize),
}

/// The top-level names one module can use.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    /// What each name stands for: the module's own declarations by that
    /// name if it has any, otherwise what its imports export.
    pub names: HashMap<String, Vec<Global>>,
    /// The names that come from imports. One of these standing for more
    /// than one thing is ambiguous, and an error where it is used.
    pub imported: HashSet<String>,
}

impl Scope {
    /// What `name` stands for, unless it is unknown or ambiguous.
    pub fn get(&self, name: &str) -> &[Global] {
        match self.names.get(name) {
            Some(gs) if !(gs.len() > 1 && self.imported.contains(name)) => gs,
            _ => &[],
        }
    }

    /// The fn or rill `name` stands for.
    pub fn def(&self, name: &str) -> Option<usize> {
        self.get(name).iter().find_map(|g| match g {
            Global::Def(i) => Some(*i),
            _ => None,
        })
    }
}

/// What the checker found out about a top-level `const`.
#[derive(Clone, Debug, PartialEq)]
pub struct ConstInfo {
    pub ty: Type,
    /// The value, if it is a number known while checking.
    pub value: Option<f64>,
}

impl Checked {
    /// For one of the events a sequence makes, by index into
    /// [`Checked::events`]: the sequence's index and the kind.
    pub fn seq_event(&self, i: usize) -> Option<(usize, EventKind)> {
        let k = i.checked_sub(self.written_events)?;
        let n = EventKind::SEQ.len();
        (i < self.events.len()).then(|| (k / n, EventKind::SEQ[k % n]))
    }
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
    /// The payload of an `on` handler.
    EventParam,
    /// A `const` in a block.
    Const,
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

impl Checked {
    /// Whether the program calls or names the built-in `name` anywhere.
    pub fn uses_builtin(&self, name: &str) -> bool {
        self.resolutions
            .iter()
            .any(|(_, r)| matches!(r, Resolution::Builtin(n) if n == name))
    }
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
    /// A declared event, by index into [`Checked::events`].
    Event(usize),
    /// A sequence, by index into the program's sequences.
    Seq(usize),
    /// A top-level `const`, by index into [`Checked::consts`].
    Const(usize),
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
    let (own, module_scopes, exported) = build_scopes(program);
    let mut c = Checker {
        own,
        module_scopes,
        exported,
        modules: program.modules.clone(),
        module: 0,
        used_from: HashSet::new(),
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
        bindings: Vec::new(),
        resolutions: Vec::new(),
        def_bindings: Vec::new(),
        scope_spans: Vec::new(),
        sizes: Vec::new(),
        current_def: 0,
        events: Vec::new(),
        written_events: 0,
        seq_facts: Vec::new(),
        const_values: HashMap::new(),
        event_used: Vec::new(),
        seq_names: program.seqs.iter().map(|s| s.name.name.clone()).collect(),
        seq_steps: program.seqs.iter().map(|s| s.steps.len()).collect(),
        seq_used: vec![false; program.seqs.len()],
        handler: None,
        invokes: Vec::new(),
        const_types: vec![Type::Error; program.consts.len()],
        const_vals: vec![Err(None); program.consts.len()],
        local_const_vals: HashMap::new(),
        const_order: Vec::new(),
    };

    c.declare_events(&program.events, &program.seqs);
    c.generate_seq_events(program);
    c.check_imports(program);
    // Facts first, as `const`s can use them; `instances` can use a `const`
    // in turn, so it is worked out again once they are known.
    c.seq_facts = program.seqs.iter().map(|s| SeqFacts::of(s, None)).collect();
    c.check_consts(&program.consts, &program.items);
    for (j, seq) in program.seqs.iter().enumerate() {
        let instances = seq
            .settings
            .iter()
            .find(|s| s.name.name == "instances")
            .and_then(|s| c.const_value(&s.value).ok());
        c.seq_facts[j] = SeqFacts::of(seq, instances);
    }
    for (i, item) in program.items.iter().enumerate() {
        c.module = program.modules.item(i);
        match item {
            Item::Fn(d) => c.declare(d, DefKind::Fn),
            Item::Rill(d) => c.declare(d, DefKind::Rill),
        }
    }
    c.const_clashes(program);
    c.check_seqs(&program.seqs, &program.events);
    for (i, decl) in program.events.iter().enumerate() {
        let m = program.modules.event(i);
        if c.events[i].is_some() && c.own_has(m, &decl.name.name, |g| matches!(g, Global::Def(_))) {
            let e = c.error(
                decl.name.span,
                format!("`{}` is already the name of a fn or rill", decl.name.name),
            );
            c.report(e);
        }
    }
    for i in c.written_events..c.events.len() {
        let name = c.events[i]
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_default();
        let j = (i - c.written_events) / EventKind::SEQ.len();
        if c.own_has(program.modules.seq(j), &name, |g| {
            matches!(g, Global::Def(_))
        }) {
            let e = c
                .error(
                    program.seqs[j].name.span,
                    format!(
                        "sequence `{}` makes event `{name}`, which is already the name of a fn or rill",
                        program.seqs[j].name.name
                    ),
                )
                .with_help("rename the fn or rill, or the sequence");
            c.report(e);
        }
    }
    for (index, item) in program.items.iter().enumerate() {
        c.module = program.modules.item(index);
        c.check_def(item.def(), index);
    }

    c.check_recursion();
    c.check_invoke_loops(program);
    c.unused_imports(program);
    for (seq, used) in program.seqs.iter().zip(c.seq_used.clone()) {
        // What a file exports is for others to use.
        if !used && seq.export.is_none() {
            c.report(Diagnostic::warning(
                seq.name.span,
                format!("sequence `{}` is never invoked", seq.name.name),
            ));
        }
    }
    for (decl, used) in program.events.iter().zip(c.event_used.clone()) {
        if !used && decl.export.is_none() {
            c.report(Diagnostic::warning(
                decl.name.span,
                format!("event `{}` is declared but never handled", decl.name.name),
            ));
        }
    }

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
        events: c.events,
        written_events: c.written_events,
        seq_facts: c.seq_facts,
        const_values: c.const_values,
        consts: c
            .const_types
            .into_iter()
            .zip(c.const_vals)
            .map(|(ty, value)| ConstInfo {
                ty,
                value: value.ok(),
            })
            .collect(),
        const_order: c.const_order,
        scopes: c.module_scopes,
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
    /// A `for` loop variable.
    Loop,
    /// The payload of an `on` handler.
    EventParam,
    /// A `const` in a block.
    Const,
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

pub(crate) type OpError = (String, Option<String>);

/// What [`Checker::const_value`] gives.
type ConstVal = Result<f64, Option<String>>;

struct Checker {
    /// Per module, its own top-level declarations by name, for spotting
    /// names declared twice.
    own: Vec<OwnNames>,
    /// Per module, the top-level names it can use.
    module_scopes: Vec<Scope>,
    /// The declarations other modules can import.
    exported: HashSet<Global>,
    modules: Modules,
    /// The module being checked.
    module: usize,
    /// (user, declarer): a module uses something another module declares.
    used_from: HashSet<(usize, usize)>,
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
    /// Caller -> (callee, call site), by index into `signatures`, user
    /// definitions only. Mentioning a fn as a value counts as a call.
    calls: HashMap<usize, Vec<(usize, Span)>>,
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
    /// Event declarations, as in [`Checked::events`].
    events: Vec<Option<Declared>>,
    /// Per declaration: whether a handler uses it.
    event_used: Vec<bool>,
    /// Names of the program's sequences, by index.
    seq_names: Vec<String>,
    /// Per sequence: its number of steps.
    seq_steps: Vec<usize>,
    /// Per sequence: whether anything invokes it.
    seq_used: Vec<bool>,
    /// The `on` handler being checked: what it handles.
    handler: Option<Node>,
    /// Every `invoke` and `trigger`: from the handler it is in, to what it
    /// starts, for the loop check.
    invokes: Vec<(Node, Node, Span)>,
    /// Top-level `const`s: their types and values.
    const_types: Vec<Type>,
    const_vals: Vec<ConstVal>,
    /// The values of `const`s in blocks, by binding.
    local_const_vals: HashMap<BindingId, ConstVal>,
    const_order: Vec<usize>,
    /// How many of `events` are written in the program; the rest are made
    /// by sequences.
    written_events: usize,
    seq_facts: Vec<SeqFacts>,
    const_values: HashMap<u32, f64>,
}

/// Something that runs handlers, for the invoke-loop check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Node {
    Start,
    Event(usize),
    /// Starting an instance of a sequence: its start and its first step
    /// happen at once.
    SeqStarts(usize),
    /// Stopping one early: its note-offs, `halted` and `end` happen at once.
    SeqStops(usize),
}

impl Checker {
    fn resolve(&mut self, span: Span, r: Resolution) {
        let g = match r {
            Resolution::Def(i) => Some(Global::Def(i)),
            Resolution::Event(i) => Some(Global::Event(i)),
            Resolution::Seq(j) => Some(Global::Seq(j)),
            Resolution::Const(i) => Some(Global::Const(i)),
            _ => None,
        };
        if let Some(g) = g {
            let from = self.module_of(g);
            if from != self.module {
                self.used_from.insert((self.module, from));
            }
        }
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

    // ---- names across modules ------------------------------------------

    /// What `name` stands for in the module being checked, unless it is
    /// unknown or ambiguous.
    fn scope_get(&self, name: &str) -> &[Global] {
        self.module_scopes[self.module].get(name)
    }

    fn def_named(&self, name: &str) -> Option<usize> {
        self.module_scopes[self.module].def(name)
    }

    fn event_named(&self, name: &str) -> Option<usize> {
        self.scope_get(name).iter().find_map(|g| match g {
            Global::Event(i) => Some(*i),
            _ => None,
        })
    }

    fn seq_named(&self, name: &str) -> Option<usize> {
        self.scope_get(name).iter().find_map(|g| match g {
            Global::Seq(j) => Some(*j),
            _ => None,
        })
    }

    fn const_named(&self, name: &str) -> Option<usize> {
        self.scope_get(name).iter().find_map(|g| match g {
            Global::Const(i) => Some(*i),
            _ => None,
        })
    }

    /// The first of module `m`'s own declarations named `name` that `is`
    /// accepts.
    fn own_first(&self, m: usize, name: &str, is: impl Fn(&Global) -> bool) -> Option<Global> {
        self.own[m].get(name)?.iter().copied().find(|g| is(g))
    }

    fn own_has(&self, m: usize, name: &str, is: impl Fn(&Global) -> bool) -> bool {
        self.own_first(m, name, is).is_some()
    }

    /// The names the module being checked can use, of the kinds `is`
    /// accepts, for suggestions.
    fn visible(&self, is: impl Fn(&Global) -> bool) -> Vec<&str> {
        let scope = &self.module_scopes[self.module];
        let mut names: Vec<&str> = scope
            .names
            .iter()
            .filter(|(n, gs)| gs.iter().any(&is) && !scope.get(n).is_empty())
            .map(|(n, _)| n.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    /// Why `name` is not usable here when another file has it: two imports
    /// both export it, it is private to its file, or its file is not
    /// imported. `None` if no file has it.
    fn why_missing(&self, name: &str, span: Span) -> Option<Diagnostic> {
        let scope = &self.module_scopes[self.module];
        let module_name = |g: &Global| self.modules.name(self.module_of(*g)).to_owned();
        if let Some(gs) = scope.names.get(name)
            && gs.len() > 1
            && scope.imported.contains(name)
        {
            let mut from: Vec<String> = gs
                .iter()
                .map(|g| format!("\"{}\"", module_name(g)))
                .collect();
            from.dedup();
            let from = match from.split_last() {
                Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
                _ => from.concat(),
            };
            let both = if gs.len() == 2 { "both " } else { "" };
            return Some(
                Diagnostic::error(span, format!("`{name}` is exported by {both}{from}"))
                .with_help(format!("they are different things; import only one of these files here, or define `{name}` in this file")),
            );
        }
        if self.modules.count() < 2 {
            return None;
        }
        let sees = self.modules.sees(self.module);
        let (m, g) = (0..self.own.len())
            .filter(|&m| m != self.module)
            .find_map(|m| {
                self.own[m]
                    .get(name)
                    .and_then(|gs| gs.first())
                    .map(|g| (m, *g))
            })?;
        let there = self.modules.name(m);
        let seen = sees.contains(&(m as u32));
        let exported = self.exported.contains(&g);
        let path = relative_import(self.modules.name(self.module), there);
        Some(match (seen, exported) {
            (true, _) => Diagnostic::error(span, format!("`{name}` is private to \"{there}\""))
                .with_help(format!(
                    "put `export` before it in \"{there}\" to use it in other files"
                )),
            (false, true) => Diagnostic::error(
                span,
                format!("`{name}` is in \"{there}\", which this file does not import"),
            )
            .with_help(format!("add `import \"{path}\"` at the top of this file")),
            (false, false) => Diagnostic::error(
                span,
                format!("`{name}` is private to \"{there}\", which this file does not import"),
            )
            .with_help(format!(
                "put `export` before it in \"{there}\", and add `import \"{path}\"` here"
            )),
        })
    }

    /// The module a declaration is written in.
    fn module_of(&self, g: Global) -> usize {
        global_module(&self.modules, self.written_events, g)
    }

    /// Plain imports that nothing in the file uses. (`export import` is
    /// there to pass things on, so it counts as used.)
    fn unused_imports(&mut self, program: &Program) {
        // What each module passes on, itself included.
        let n = program.modules.count();
        let mut gives: Vec<HashSet<usize>> = (0..n).map(|m| HashSet::from([m])).collect();
        loop {
            let mut changed = false;
            for (k, import) in program.imports.iter().enumerate() {
                let (Some(t), true) = (import.module, import.export.is_some()) else {
                    continue;
                };
                let from = program.modules.import(k);
                let more: Vec<usize> = gives[t as usize].iter().copied().collect();
                for x in more {
                    changed |= gives[from].insert(x);
                }
            }
            if !changed {
                break;
            }
        }
        for (k, import) in program.imports.iter().enumerate() {
            let Some(t) = import.module else { continue };
            let m = program.modules.import(k);
            let used = gives[t as usize]
                .iter()
                .any(|&x| self.used_from.contains(&(m, x)));
            // One that exports nothing is reported as that instead.
            let exports = self
                .exported
                .iter()
                .any(|&g| gives[t as usize].contains(&self.module_of(g)));
            if import.export.is_none() && !used && exports {
                self.report(
                    Diagnostic::warning(
                        import.path_span,
                        format!("nothing from \"{}\" is used here", import.path),
                    )
                    .with_help("remove this import"),
                );
            }
        }
    }

    /// Imports that bring nothing: the file exports nothing and passes
    /// nothing on.
    fn check_imports(&mut self, program: &Program) {
        for import in &program.imports {
            let Some(target) = import.module.map(|t| t as usize) else {
                continue;
            };
            let exports = self.exported.iter().any(|&g| self.module_of(g) == target);
            let passes_on = program.imports.iter().enumerate().any(|(j, i)| {
                program.modules.import(j) == target && i.export.is_some() && i.module.is_some()
            });
            if !exports && !passes_on {
                self.report(
                    Diagnostic::warning(
                        import.path_span,
                        format!("\"{}\" exports nothing", import.path),
                    )
                    .with_help("put `export` before what it should share, or remove this import"),
                );
            }
        }
    }

    fn error(&mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error(span, message)
    }

    fn report(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    // ---- declarations -------------------------------------------------

    fn declare_events(&mut self, decls: &[EventDecl], seqs: &[SeqDecl]) {
        for (i, d) in decls.iter().enumerate() {
            self.module = self.modules.event(i);
            let m = self.module;
            let first = self.own_first(m, &d.name.name, |g| matches!(g, Global::Event(_)));
            let _ = seqs;
            if first != Some(Global::Event(i)) {
                let e = self.error(
                    d.name.span,
                    format!("event `{}` is declared more than once", d.name.name),
                );
                self.report(e);
            } else if d.name.name == "start" {
                let e = self
                    .error(d.name.span, "`start` is a built-in event")
                    .with_help(
                        "handle it with `on start { ... }`; it runs once, before the first sample",
                    );
                self.report(e);
            } else if self.own_has(m, &d.name.name, |g| matches!(g, Global::Seq(_))) {
                let e = self.error(
                    d.name.span,
                    format!("`{}` is already the name of a sequence", d.name.name),
                );
                self.report(e);
            }
            let kind = EventKind::from_name(&d.kind.name);
            if kind.is_none() {
                let kinds = EventKind::ALL.map(EventKind::name);
                let mut e =
                    self.error(d.kind.span, format!("unknown event kind `{}`", d.kind.name));
                e = match suggest(&d.kind.name, kinds) {
                    Some(k) => e.with_help(format!("did you mean `{k}`?")),
                    None => e.with_help("the kinds are `note_on`, `note_off` and `control_change`"),
                };
                self.report(e);
            }
            let (mut sender, mut channel) = (None, None);
            for f in &d.filters {
                self.types[f.value.id as usize] = Type::Int;
                let number = match &f.value.kind {
                    ExprKind::Number {
                        value,
                        unit: None,
                        integral: true,
                    } if *value <= f64::from(u32::MAX) => Some(*value as u32),
                    _ => None,
                };
                match f.name.name.as_str() {
                    "sender" => {
                        let value = match (&f.value.kind, number) {
                            (_, Some(n)) => Sender::Host(n),
                            (ExprKind::Name(n), _) => match self.seq_named(n) {
                                Some(j) => {
                                    self.resolve(f.value.span, Resolution::Seq(j));
                                    Sender::Seq(j as u16)
                                }
                                None => {
                                    let e = self.why_missing(n, f.value.span).unwrap_or_else(|| Diagnostic::error(f.value.span, format!("unknown sequence `{n}`"))
                                            .with_help("a sender is a whole number from the host, or a sequence's name"));
                                    self.report(e);
                                    continue;
                                }
                            },
                            _ => {
                                let e = self
                                    .error(f.value.span, "a sender is a whole number ≥ 0 or a sequence's name")
                                    .with_help("filters are fixed when the program is built, as in `sender: 1` or `sender: riff`");
                                self.report(e);
                                continue;
                            }
                        };
                        if sender.replace(value).is_some() {
                            let e =
                                self.error(f.name.span, "filter `sender` is given more than once");
                            self.report(e);
                        }
                    }
                    "channel" => {
                        let Some(value) = number else {
                            let e = self
                                .error(f.value.span, "a filter is a whole number ≥ 0")
                                .with_help(
                                    "filters are fixed when the program is built, as in `channel: 1`",
                                );
                            self.report(e);
                            continue;
                        };
                        if channel.replace(value).is_some() {
                            let e =
                                self.error(f.name.span, "filter `channel` is given more than once");
                            self.report(e);
                        }
                    }
                    other => {
                        let e = self
                            .error(f.name.span, format!("unknown filter `{other}`"))
                            .with_help("events can be filtered by `sender` and `channel`");
                        self.report(e);
                    }
                }
            }
            self.events.push(kind.map(|kind| Declared {
                name: d.name.name.clone(),
                kind,
                sender,
                channel,
            }));
            self.event_used.push(false);
        }
    }

    /// The events every sequence makes, `<seq>_<kind>` for each of
    /// [`EventKind::SEQ`], after the written declarations.
    fn generate_seq_events(&mut self, program: &Program) {
        self.written_events = self.events.len();
        // Two sequences never make the same name: no suffix is another
        // suffix with a word in front.
        for (j, seq) in program.seqs.iter().enumerate() {
            for kind in EventKind::SEQ {
                let name = format!("{}_{}", seq.name.name, kind.suffix());
                let m = program.modules.seq(j);
                let clash = self
                    .own_first(
                        m,
                        &name,
                        |g| matches!(g, Global::Event(i) if *i < program.events.len()),
                    )
                    .and_then(|g| match g {
                        Global::Event(i) => program.events.get(i),
                        _ => None,
                    });
                if let Some(d) = clash {
                    let e = self
                        .error(
                            d.name.span,
                            format!("sequence `{}` already makes an event `{name}`", seq.name.name),
                        )
                        .with_help(format!(
                            "every sequence makes its own events, such as `{0}_note_on` and `{0}_step`; rename this one",
                            seq.name.name
                        ));
                    self.report(e);
                }
                self.events.push(Some(Declared {
                    name,
                    kind,
                    sender: Some(Sender::Seq(j as u16)),
                    channel: None,
                }));
                self.event_used.push(true);
            }
        }
    }

    /// `invoke`, `trigger` and `halt` start and stop things at a moment, so
    /// they belong in handlers; a rill body runs every sample.
    fn in_handler_only(&mut self, span: Span, word: &str) {
        if !self.in_event {
            let e = self
                .error(span, format!("`{word}` only works inside an `on` handler"))
                .with_help("a rill body runs every sample, so it would start again every sample; react to an event instead, as in `on start { invoke riff }`");
            self.report(e);
        }
    }

    /// An instance id: a whole number, ≥ 1 if written out.
    fn instance_id(&mut self, id: &Expr) {
        let t = self.expr(id);
        if !t.is_wild() && !matches!(t, Type::Int | Type::Num) {
            let e = self
                .error(id.span, format!("an instance id is an `Int`, not `{t}`"))
                .with_help("ids you pick are whole numbers ≥ 1; `invoke` returns the id it used");
            self.report(e);
        } else if let ExprKind::Number { value, .. } = id.kind
            && (value < 1.0 || value.fract() != 0.0)
        {
            let e = self
                .error(id.span, "an id you pick is a whole number ≥ 1")
                .with_help(
                    "0 marks notes that did not come from a sequence, and fresh ids are negative",
                );
            self.report(e);
        }
    }

    fn unknown_seq(&mut self, target: &Ident) -> Diagnostic {
        if let Some(d) = self.why_missing(&target.name, target.span) {
            return d;
        }
        let near =
            suggest(&target.name, self.visible(|g| matches!(g, Global::Seq(_)))).map(str::to_owned);
        let e = self.error(target.span, format!("unknown sequence `{}`", target.name));
        match near {
            Some(n) => e.with_help(format!("did you mean `{n}`?")),
            None => e.with_help(format!(
                "declare it at the top level, as in `seq {} {{ C4, E4, G4 }}`",
                target.name
            )),
        }
    }

    fn invoke(
        &mut self,
        span: Span,
        step: Option<&Expr>,
        id: Option<&Expr>,
        target: &Ident,
        args: &[Arg],
    ) -> Type {
        let word = if step.is_some() { "trigger" } else { "invoke" };
        self.in_handler_only(span, word);
        self.no_each(args, &format!("`{word}` runs once"));
        if let Some(j) = self.seq_named(&target.name) {
            self.resolve(target.span, Resolution::Seq(j));
            self.seq_used[j] = true;
            if let Some(from) = self.handler {
                self.invokes.push((from, Node::SeqStarts(j), target.span));
                // `trigger` on a playing instance restarts it.
                if step.is_some() && id.is_some() {
                    self.invokes.push((from, Node::SeqStops(j), target.span));
                }
            }
            if let Some(id) = id {
                self.instance_id(id);
            }
            if let Some(step) = step {
                let t = self.expr(step);
                if !t.is_wild() && !matches!(t, Type::Int | Type::Num) {
                    let e = self.error(step.span, format!("a step is an `Int`, not `{t}`"));
                    self.report(e);
                } else if let ExprKind::Number { value, .. } = step.kind {
                    let steps = self.seq_steps[j];
                    if value < 1.0 || value.fract() != 0.0 || value > steps as f64 {
                        let e = self
                            .error(
                                step.span,
                                format!(
                                    "step {value} is outside `{}`, which has {steps} steps",
                                    target.name
                                ),
                            )
                            .with_help("steps count from 1");
                        self.report(e);
                    }
                }
            }
            let mut seen: Vec<String> = Vec::new();
            for a in args {
                let Some(name) = &a.name else {
                    let e = self
                        .error(a.value.span, "settings are given by name")
                        .with_help(format!("as in `{word} {}(tempo: 90bpm)`", target.name));
                    self.report(e);
                    self.expr(&a.value);
                    continue;
                };
                if seen.contains(&name.name) {
                    let e = self.error(
                        name.span,
                        format!("setting `{}` is given more than once", name.name),
                    );
                    self.report(e);
                }
                seen.push(name.name.clone());
                let fixed = matches!(name.name.as_str(), "meter" | "step" | "instances");
                match seq_setting(&name.name) {
                    _ if fixed => {
                        let e = self
                            .error(
                                name.span,
                                format!(
                                    "`{}` is fixed when `{}` is declared",
                                    name.name, target.name
                                ),
                            )
                            .with_help("only `tempo`, `gate`, `velocity`, `repeat` and `loop` can be given when invoking");
                        self.report(e);
                        self.expr(&a.value);
                    }
                    Some((ty, _)) => {
                        let t = self.expr_expect(&a.value, Some(&ty));
                        if !coerces(&t, &ty) {
                            let e = mismatch(
                                a.value.span,
                                &format!("setting `{}`", name.name),
                                &ty,
                                &t,
                            );
                            self.report(e);
                        } else {
                            self.check_setting_range(&name.name, &a.value);
                        }
                    }
                    None => {
                        let e = self
                            .error(name.span, format!("unknown setting `{}`", name.name))
                            .with_help("when invoking, a sequence takes `tempo`, `gate`, `velocity`, `repeat` and `loop`");
                        self.report(e);
                        self.expr(&a.value);
                    }
                }
            }
            return Type::Int;
        }
        let event = self
            .event_named(&target.name)
            .filter(|&i| self.events[i].is_some());
        let Some(i) = event else {
            let e = if target.name == "start" {
                self.error(target.span, "`start` cannot be invoked")
                    .with_help("it runs once, before the first sample")
            } else {
                self.unknown_seq(target)
            };
            self.report(e);
            for a in args {
                self.expr(&a.value);
            }
            return Type::Error;
        };
        self.resolve(target.span, Resolution::Event(i));
        if i >= self.written_events {
            let seq = self.seq_names[(i - self.written_events) / EventKind::SEQ.len()].clone();
            let e = self
                .error(
                    target.span,
                    format!(
                        "`{}` is made by sequence `{seq}`, so it cannot be invoked",
                        target.name
                    ),
                )
                .with_help(format!("invoke the sequence instead, as in `invoke {seq}`"));
            self.report(e);
            for a in args {
                self.expr(&a.value);
            }
            return Type::Error;
        }
        if let Some(from) = self.handler {
            self.invokes.push((from, Node::Event(i), target.span));
        }
        if step.is_some() || id.is_some() {
            let e = self
                .error(
                    span,
                    format!(
                        "`{}` is an event; only sequences have steps and instances",
                        target.name
                    ),
                )
                .with_help(format!("send it with `invoke {}(...)`", target.name));
            self.report(e);
        }
        let kind = self.events[i].as_ref().expect("found above").kind;
        for a in args {
            let Some(n) = &a.name else {
                let e = self
                    .error(a.value.span, "an event's fields are given by name")
                    .with_help(format!(
                        "as in `invoke {}(pitch: C4, velocity: 1)`",
                        target.name
                    ));
                self.report(e);
                self.expr(&a.value);
                continue;
            };
            let ty = match (kind, n.name.as_str()) {
                (EventKind::ControlChange, "value") => Some(Type::Float),
                _ => event_field_type(kind, &n.name),
            };
            match ty {
                Some(ty) => {
                    let t = self.expr_expect(&a.value, Some(&ty));
                    if !coerces(&t, &ty) {
                        let e = mismatch(a.value.span, &format!("field `{}`", n.name), &ty, &t);
                        self.report(e);
                    }
                }
                None => {
                    let fields = kind
                        .fields()
                        .iter()
                        .map(|f| format!("`{f}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let e = self
                        .error(n.span, format!("a {kind} event has no field `{}`", n.name))
                        .with_help(format!("it has {fields}"));
                    self.report(e);
                    self.expr(&a.value);
                }
            }
        }
        Type::Unit
    }

    /// Reject handlers that can invoke, through events and sequences, the
    /// event that runs them. A sequence's notes reach every declaration
    /// that accepts them.
    fn check_invoke_loops(&mut self, program: &Program) {
        let mut edges: HashMap<Node, Vec<(Node, Span)>> = HashMap::new();
        for &(from, to, at) in &self.invokes {
            edges.entry(from).or_default().push((to, at));
        }
        // What happens in the same sample as starting or stopping an
        // instance. Later events (later steps, repeats, finishing) cannot
        // loop within one sample; replacing an instance can, but never one
        // started in the same sample, so it ends.
        const AT_START: [EventKind; 6] = [
            EventKind::Start,
            EventKind::Bar,
            EventKind::Beat,
            EventKind::Step,
            EventKind::Rest,
            EventKind::NoteOn,
        ];
        const AT_STOP: [EventKind; 3] = [EventKind::NoteOff, EventKind::Halted, EventKind::End];
        for j in 0..program.seqs.len() {
            let from = Sender::Seq(j as u16);
            let span = program.seqs[j].name.span;
            for (i, d) in self.events.iter().enumerate() {
                let Some(d) = d else { continue };
                for (node, kinds) in [
                    (Node::SeqStarts(j), &AT_START[..]),
                    (Node::SeqStops(j), &AT_STOP[..]),
                ] {
                    if kinds.iter().any(|&k| d.accepts(from, 0, k)) {
                        edges.entry(node).or_default().push((Node::Event(i), span));
                    }
                }
            }
        }
        let events = &self.events;
        let name = |n: Node| match n {
            Node::Start => "start".to_owned(),
            Node::Event(i) => events[i]
                .as_ref()
                .map_or_else(String::new, |d| d.name.clone()),
            Node::SeqStarts(j) | Node::SeqStops(j) => program.seqs[j].name.name.clone(),
        };
        let mut reported: HashSet<Vec<String>> = HashSet::new();
        let mut found = Vec::new();
        for start in (0..self.events.len()).map(Node::Event) {
            // Depth-first search for a path back to `start`. `path` holds
            // the nodes after `start`, each with the invoke that led there.
            let mut path: Vec<(Node, Span)> = Vec::new();
            let mut stack: Vec<(Node, usize)> = vec![(start, 0)];
            while let Some((node, next)) = stack.pop() {
                let out = edges.get(&node).map_or(&[][..], Vec::as_slice);
                if next >= out.len() {
                    if node != start {
                        path.pop();
                    }
                    continue;
                }
                stack.push((node, next + 1));
                let (to, at) = out[next];
                if to == start {
                    let mut chain: Vec<String> = vec![name(start)];
                    chain.extend(path.iter().map(|(n, _)| name(*n)));
                    chain.push(name(start));
                    let mut key = chain.clone();
                    key.sort();
                    key.dedup();
                    if reported.insert(key) {
                        // Point at the first invoke on the way round.
                        let site = path.first().map_or(at, |(_, s)| *s);
                        let chain = chain
                            .iter()
                            .map(|n| format!("`{n}`"))
                            .collect::<Vec<_>>()
                            .join(" -> ");
                        found.push((site, chain));
                    }
                    continue;
                }
                if to == start || path.iter().any(|(n, _)| *n == to) {
                    continue;
                }
                path.push((to, at));
                stack.push((to, 0));
            }
        }
        for (site, chain) in found {
            let e = self
                .error(site, format!("invoking here can lead back to the same handler: {chain}"))
                .with_help("a sequence's start and first step, a halt, and invoked events all happen in the same sample, so this would never end");
            self.report(e);
        }
    }

    /// Check every sequence's settings and steps.
    fn check_seqs(&mut self, seqs: &[SeqDecl], events: &[EventDecl]) {
        let saved = std::mem::replace(&mut self.place, Place::Fn);
        self.scopes.push(HashMap::new());
        for (i, seq) in seqs.iter().enumerate() {
            let name = &seq.name.name;
            self.module = self.modules.seq(i);
            let m = self.module;
            if self.own_first(m, name, |g| matches!(g, Global::Seq(_))) != Some(Global::Seq(i)) {
                let e = self.error(
                    seq.name.span,
                    format!("sequence `{name}` is declared more than once"),
                );
                self.report(e);
            } else if self.own_has(m, name, |g| matches!(g, Global::Def(_))) {
                let e = self.error(
                    seq.name.span,
                    format!("`{name}` is already the name of a fn or rill"),
                );
                self.report(e);
            } else if name == "start" {
                let e = self.error(seq.name.span, "`start` is a built-in event");
                self.report(e);
            }
            let _ = events;
            if seq.steps.is_empty() {
                let e = self
                    .error(seq.name.span, format!("sequence `{name}` has no steps"))
                    .with_help("add steps between the braces, as in `{ C4, _, E4 }`");
                self.report(e);
            }
            for (j, setting) in seq.settings.iter().enumerate() {
                let sname = setting.name.name.as_str();
                if seq.settings[..j].iter().any(|p| p.name.name == sname) {
                    let e = self.error(
                        setting.name.span,
                        format!("setting `{sname}` is given more than once"),
                    );
                    self.report(e);
                }
                match sname {
                    "meter" | "step" => {
                        self.types[setting.value.id as usize] = Type::Num;
                        let ok = match fraction(&setting.value) {
                            Some((n, d)) => n >= 1 && d >= 1,
                            None => false,
                        };
                        if !ok {
                            let example = if sname == "meter" { "4/4" } else { "1/8" };
                            let what = if sname == "meter" {
                                "a time signature"
                            } else {
                                "a note value"
                            };
                            let e = self
                                .error(
                                    setting.value.span,
                                    format!("`{sname}` is {what}, written as two whole numbers"),
                                )
                                .with_help(format!("as in `{sname}: {example}`"));
                            self.report(e);
                        }
                    }
                    _ => {
                        let Some((ty, _)) = seq_setting(sname) else {
                            let e = self
                                .error(setting.name.span, format!("unknown setting `{sname}`"))
                                .with_help(format!("a sequence's settings are {}", SEQ_SETTINGS));
                            self.report(e);
                            self.expr(&setting.value);
                            continue;
                        };
                        let t = self.expr_expect(&setting.value, Some(&ty));
                        if !coerces(&t, &ty) {
                            let e = mismatch(
                                setting.value.span,
                                &format!("setting `{sname}`"),
                                &ty,
                                &t,
                            );
                            self.report(e);
                        } else if !self.is_const(&setting.value) {
                            let e = self
                                .error(setting.value.span, format!("setting `{sname}` must be a constant"))
                                .with_help("these are defaults, fixed when the program is built; pass a value that changes when invoking, as in `invoke riff(tempo: speed)`");
                            self.report(e);
                        } else {
                            self.check_setting_range(sname, &setting.value);
                        }
                    }
                }
            }
            for step in &seq.steps {
                if let Some(notes) = &step.notes {
                    let t = self.expr(notes);
                    let ok = match &t {
                        Type::Pitch => true,
                        Type::Frame(elem, _) => **elem == Type::Pitch,
                        t => t.is_wild(),
                    };
                    if !ok {
                        let e = self
                            .error(
                                notes.span,
                                format!("a step is a pitch, a chord or `_`, not `{t}`"),
                            )
                            .with_help("as in `C4`, `[C4, E4, G4]` or `_` for a rest");
                        self.report(e);
                    } else if !self.is_const(notes) {
                        let e = self.error(notes.span, "a step must be a constant");
                        self.report(e);
                    }
                }
                if let Some(v) = &step.velocity {
                    let t = self.expr_expect(v, Some(&Type::Float));
                    if !coerces(&t, &Type::Float) {
                        let e = mismatch(v.span, "the velocity", &Type::Float, &t);
                        self.report(e);
                    } else if !self.is_const(v) {
                        let e = self.error(v.span, "a step's velocity must be a constant");
                        self.report(e);
                    } else {
                        self.check_setting_range("velocity", v);
                    }
                    if step.notes.is_none() {
                        let e = self.error(v.span, "a rest has no velocity");
                        self.report(e);
                    }
                }
            }
        }
        self.scopes.pop();
        self.place = saved;
    }

    /// Range checks for literal setting values.
    fn check_setting_range(&mut self, name: &str, value: &Expr) {
        let v = match &value.kind {
            ExprKind::Number { value: v, .. } => *v,
            ExprKind::Unary(UnOp::Neg, x) => match x.kind {
                ExprKind::Number { value: v, .. } => -v,
                _ => return,
            },
            _ => return,
        };
        let problem = match name {
            "tempo" if v <= 0.0 => Some("a tempo is above 0bpm"),
            "gate" if !(v > 0.0 && v <= 1.0) => {
                Some("`gate` is a fraction of the step, above 0 and at most 1")
            }
            "velocity" if !(0.0..=1.0).contains(&v) => Some("a velocity is from 0 to 1"),
            "repeat" | "instances" if v < 1.0 || v.fract() != 0.0 => {
                Some("this is a whole number, at least 1")
            }
            "instances" if v > 65_535.0 => Some("at most 65535 instances"),
            _ => None,
        };
        if let Some(p) = problem {
            let e = self.error(value.span, p);
            self.report(e);
        }
    }

    /// The declared event an `on` handler names, if any; reports the
    /// problem otherwise.
    fn handled_event(&mut self, name: &Ident) -> Option<usize> {
        let found = self
            .event_named(&name.name)
            .filter(|&i| self.events[i].is_some());
        if let Some(i) = found {
            self.event_used[i] = true;
            self.resolve(name.span, Resolution::Event(i));
            return Some(i);
        }
        // A declaration with an unknown kind is already reported.
        let broken = self.events.iter().any(Option::is_none);
        if broken {
            return None;
        }
        if let Some(d) = self.why_missing(&name.name, name.span) {
            self.report(d);
            return None;
        }
        let e = if EventKind::from_name(&name.name).is_some() {
            self.error(
                name.span,
                format!("`{}` is a kind of event, not a declared event", name.name),
            )
            .with_help(format!(
                "declare one at the top level and handle it by name: `event keys {}(channel: 1)`, then `on keys(...)`",
                name.name
            ))
        } else {
            let near = suggest(&name.name, self.visible(|g| matches!(g, Global::Event(_))))
                .map(str::to_owned);
            let e = self.error(name.span, format!("unknown event `{}`", name.name));
            match near {
                Some(n) => e.with_help(format!("did you mean `{n}`?")),
                None => e.with_help(format!(
                    "declare it at the top level, as in `event {} note_on(channel: 1)`",
                    name.name
                )),
            }
        };
        self.report(e);
        None
    }

    fn declare(&mut self, d: &Def, kind: DefKind) {
        let name = &d.name.name;
        let index = self.signatures.len();
        let m = self.module;
        if self.own_first(m, name, |g| matches!(g, Global::Def(_))) != Some(Global::Def(index)) {
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

        // A type that failed to resolve may well have used it.
        let broken = params.iter().any(|p| p.ty.leaf().is_wild()) || ret.leaf().is_wild();
        for g in &d.generics {
            let used = broken
                || params.iter().any(|p| mentions_size(&p.ty, &g.name))
                || mentions_size(&ret, &g.name);
            if !used {
                let e = self
                    .error(g.span, format!("size `{}` is not used", g.name))
                    .with_help("use it in a parameter or return type");
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
                match self.size(size, generics) {
                    Some(size) => Type::Frame(Box::new(elem_ty), size),
                    None => Type::Error,
                }
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
            VarKind::Loop => BindingKind::Let,
            VarKind::EventParam => BindingKind::EventParam,
            VarKind::Const => BindingKind::Const,
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
                let t = match value {
                    Some(value) => self.expr_expect(value, declared.as_ref()),
                    None => declared.clone().unwrap_or(Type::Error),
                };
                let bound = match declared {
                    Some(declared) => {
                        if value.is_some() && !coerces(&t, &declared) {
                            let value = value.as_ref().expect("checked above");
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
            Stmt::Const(decl) => {
                let (t, value) = self.const_decl(decl);
                self.bind(&decl.name, t, VarKind::Const, decl.span.end);
                let id = self.lookup(&decl.name.name).expect("just bound").id;
                self.local_const_vals.insert(id, value);
                (Type::Unit, false)
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
                let name = target.name();
                if let AssignTarget::Index { index, .. } = target {
                    let ti = self.expr(index);
                    if !ti.is_wild() && !matches!(ti, Type::Int | Type::Num) {
                        let d = self.error(
                            index.span,
                            format!("channel index must be a whole number, found `{ti}`"),
                        );
                        self.report(d);
                    }
                }
                if let Some(var) = self.lookup(&name.name) {
                    let id = var.id;
                    self.resolve(name.span, Resolution::Binding(id));
                }
                let captured = matches!(
                    (self.lambda_floor, self.lookup_depth(&name.name)),
                    (Some(floor), Some(depth)) if depth < floor
                );
                match self.lookup(&name.name).cloned() {
                    Some(_) if captured => {
                        let e = self
                            .error(name.span, format!("an anonymous fn cannot change `{}`", name.name))
                            .with_help("fns are pure, including anonymous ones: they can read what they capture, but not change it");
                        self.report(e);
                    }
                    Some(var)
                        if var.kind == VarKind::State
                            || var.kind == VarKind::Let
                            || (self.in_event && var.kind == VarKind::Param) =>
                    {
                        let expected = match (target, &var.ty) {
                            (AssignTarget::Index { .. }, Type::Frame(elem, _)) => *elem.clone(),
                            (AssignTarget::Index { .. }, other) => {
                                let e = self
                                    .error(target.span(), format!("cannot index `{other}`"))
                                    .with_help("only frames have channels to index");
                                self.report(e);
                                Type::Error
                            }
                            _ => var.ty.clone(),
                        };
                        if !coerces(&t, &expected) {
                            let e =
                                mismatch(value.span, &format!("`{}`", name.name), &expected, &t);
                            self.report(e);
                        }
                    }
                    Some(var) if var.kind == VarKind::Const => {
                        let e = self
                            .error(name.span, format!("`{}` is a `const`, so it cannot change", name.name))
                            .with_help("use `let` for a value that changes, or `state` for one kept between ticks");
                        self.report(e);
                    }
                    None if self.const_named(&name.name).is_some() => {
                        let i = self.const_named(&name.name).expect("checked");
                        self.resolve(name.span, Resolution::Const(i));
                        let e = self
                            .error(name.span, format!("`{}` is a `const`, so it cannot change", name.name))
                            .with_help("use `let` for a value that changes, or `state` for one kept between ticks");
                        self.report(e);
                    }
                    Some(_) => {
                        let e = self
                            .error(name.span, format!("cannot assign to `{}`", name.name))
                            .with_help("only local `let` bindings and `state` variables can be assigned to");
                        self.report(e);
                    }
                    None => {
                        let e = self.unknown_name(&name.name, name.span);
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
                name,
                params,
                mode,
                body,
                ..
            } => {
                if self.place != Place::Rill {
                    let e = self.error(s.span(), "event handlers are only allowed in rills");
                    self.report(e);
                    return (Type::Unit, false);
                }
                let (node, kind) = if name.name == "start" {
                    if let Some(p) = params.first() {
                        let e = self
                            .error(p.span, "`start` has no payload")
                            .with_help("write `on start { ... }`");
                        self.report(e);
                    }
                    if *mode != HandlerMode::Plain {
                        let e = self.error(
                            name.span,
                            "`on start` runs in every copy; it cannot `claim` or `release`",
                        );
                        self.report(e);
                    }
                    (Some(Node::Start), None)
                } else {
                    let i = self.handled_event(name);
                    (
                        i.map(Node::Event),
                        i.and_then(|i| self.events[i].as_ref()).map(|d| d.kind),
                    )
                };
                match mode {
                    HandlerMode::Claim { tail } => {
                        if kind.is_some_and(|k| k != EventKind::NoteOn) {
                            let e = self
                                .error(name.span, "only a `note_on` event can be claimed")
                                .with_help("a voice claims a note when it starts, and releases it on a `note_off` event");
                            self.report(e);
                        }
                        if let Some(tail) = tail {
                            let t = self.expr(tail);
                            if !coerces(&t, &Type::Time) {
                                let e = mismatch(tail.span, "`tail`", &Type::Time, &t);
                                self.report(e);
                            } else if !self.is_const(tail) {
                                let e = self.error(tail.span, "`tail` must be a constant");
                                self.report(e);
                            }
                        }
                    }
                    HandlerMode::Release if kind.is_some_and(|k| k != EventKind::NoteOff) => {
                        let e = self
                            .error(name.span, "only a `note_off` event can release a voice")
                            .with_help("use `claim` on the `note_on` event and `release` on its `note_off`");
                        self.report(e);
                    }
                    _ => {}
                }
                if let Some(extra) = params.get(1) {
                    let e = self
                        .error(
                            extra.span,
                            "an event handler takes one parameter: the event",
                        )
                        .with_help(match kind {
                            Some(EventKind::ControlChange) => {
                                "a control change's parameter is its value".to_owned()
                            }
                            _ => "read its fields, as in `note.pitch`".to_owned(),
                        });
                    self.report(e);
                }
                self.scopes.push(HashMap::new());
                self.scope_spans.push(body.span);
                if let Some(param) = params.first() {
                    let ty = match kind {
                        Some(EventKind::ControlChange) => Type::Float,
                        Some(kind) => Type::Event(kind),
                        None => Type::Error,
                    };
                    self.bind(param, ty, VarKind::EventParam, param.span.end);
                }
                let saved = std::mem::replace(&mut self.in_event, true);
                let saved_handler = std::mem::replace(&mut self.handler, node);
                let (_, diverged) = self.block(body, false);
                self.in_event = saved;
                self.handler = saved_handler;
                self.scopes.pop();
                self.scope_spans.pop();
                (Type::Unit, diverged)
            }
            Stmt::For {
                name, iter, body, ..
            } => {
                let iter_ty = self.expr(iter);
                let elem_ty = match iter_ty {
                    Type::Range => Type::Int,
                    Type::Frame(elem, _) => *elem,
                    Type::Error | Type::Never => Type::Error,
                    other => {
                        let e = self
                            .error(iter.span, format!("cannot loop over `{other}`"))
                            .with_help("loop over a range like `0..4` or a frame like `[1, 2]`");
                        self.report(e);
                        Type::Error
                    }
                };
                self.scopes.push(HashMap::new());
                self.scope_spans.push(body.span);
                self.bind(name, elem_ty, VarKind::Loop, name.span.end);
                let (_, diverged) = self.block(body, false);
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
                if let Some(i) = self.const_named(name) {
                    self.resolve(e.span, Resolution::Const(i));
                    return self.const_types[i].clone();
                }
                if pitch_literal(name).is_some() {
                    self.resolve(e.span, Resolution::Note);
                    return Type::Pitch;
                }
                if let Some(t) = builtins::constant(name) {
                    self.resolve(e.span, Resolution::Constant(name.clone()));
                    return t.clone();
                }
                if self.def_named(name).is_some() || !builtins::lookup(name).is_empty() {
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
            ExprKind::Range {
                start,
                end,
                inclusive: _,
            } => {
                let ta = self.expr(start);
                let tb = self.expr(end);
                for (expr, ty, name) in [(start, &ta, "start"), (end, &tb, "end")] {
                    if !ty.is_wild() && !matches!(ty, Type::Int | Type::Num) {
                        let d = self.error(
                            expr.span,
                            format!("range {name} must be a whole number, found `{ty}`"),
                        );
                        self.report(d);
                    }
                    if !self.is_const(expr) {
                        let d = self
                            .error(
                                expr.span,
                                format!("range {name} must be known before audio starts"),
                            )
                            .with_help("use literals, built-in constants, size parameters and pure built-in functions");
                        self.report(d);
                    }
                }
                Type::Range
            }
            ExprKind::Call {
                callee,
                sizes,
                args,
                ..
            } => self.call(e.span, callee, sizes, args),
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
                        Some(prev) => match join(&prev, &t) {
                            Some(j) => Some(j),
                            None => {
                                let msg = if prev.depth() > 0 || t.depth() > 0 {
                                    format!(
                                        "frame elements have different shapes: `{prev}` and `{t}`"
                                    )
                                } else {
                                    format!(
                                        "frame channels have different types: `{prev}` and `{t}`"
                                    )
                                };
                                let d = self.error(el.span, msg);
                                self.report(d);
                                bad = true;
                                Some(prev)
                            }
                        },
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
            ExprKind::Repeat(x, n) => {
                let t = self.expr(x);
                let generics = self.generics();
                let size = self.size(n, &generics);
                if t.is_wild() {
                    Type::Error
                } else if !t.leaf().is_quantity() {
                    let d = self.error(
                        x.span,
                        format!("a frame channel must be a number or a frame, found `{t}`"),
                    );
                    self.report(d);
                    Type::Error
                } else if size == Some(Size::Const(0)) {
                    let d = self.error(e.span, "a frame needs at least one channel");
                    self.report(d);
                    Type::Error
                } else {
                    size.map_or(Type::Error, |n| Type::Frame(Box::new(t), n))
                }
            }
            ExprKind::Invoke {
                step,
                id,
                target,
                args,
            } => self.invoke(e.span, step.as_deref(), id.as_deref(), target, args),
            ExprKind::Halt { id, target } => {
                self.in_handler_only(e.span, "halt");
                if let Some(id) = id {
                    self.instance_id(id);
                }
                match self.seq_named(&target.name) {
                    Some(j) => {
                        self.resolve(target.span, Resolution::Seq(j));
                        if let Some(from) = self.handler {
                            self.invokes.push((from, Node::SeqStops(j), target.span));
                        }
                    }
                    None => {
                        let d = self.unknown_seq(target);
                        self.report(d);
                    }
                }
                Type::Unit
            }
            ExprKind::Cast(x, te) => {
                let from = self.expr(x);
                let to = self.resolve_type(te, &self.generics());
                self.cast(e.span, &from, to)
            }
            ExprKind::Field(base, field) if self.seq_of(base).is_some() => {
                let j = self.seq_of(base).expect("matched");
                self.resolve(base.span, Resolution::Seq(j));
                match seq_field(&self.seq_facts[j], &field.name) {
                    Some((ty, value)) => {
                        self.const_values.insert(e.id, value);
                        ty
                    }
                    None => {
                        let fields =
                            listing(&SEQ_FIELDS.iter().map(|(n, _)| *n).collect::<Vec<_>>());
                        let mut d = self.error(
                            field.span,
                            format!(
                                "sequence `{}` has no field `{}`",
                                self.seq_names[j], field.name
                            ),
                        );
                        d = match suggest(&field.name, SEQ_FIELDS.iter().map(|(n, _)| *n)) {
                            Some(s) => d.with_help(format!("did you mean `{s}`?")),
                            None => d.with_help(format!("a sequence has {fields}")),
                        };
                        self.report(d);
                        Type::Error
                    }
                }
            }
            ExprKind::Field(base, field) => {
                let base_ty = self.expr(base);
                if let Some(kind) = base_ty.event_kind() {
                    match event_field_type(kind, &field.name) {
                        Some(t) => t,
                        None => {
                            let has = listing(kind.fields());
                            let mut e = self.error(
                                field.span,
                                format!("a `{base_ty}` has no field `{}`", field.name),
                            );
                            let elsewhere = EventKind::ALL
                                .into_iter()
                                .find(|k| *k != kind && k.fields().contains(&field.name.as_str()));
                            e = match elsewhere {
                                Some(k) => e.with_help(format!(
                                    "`{}` belongs to `{k}` events; a `{base_ty}` has {has}",
                                    field.name
                                )),
                                None => e.with_help(format!("a `{base_ty}` has {has}")),
                            };
                            self.report(e);
                            Type::Error
                        }
                    }
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
        if let Some(d) = self.why_missing(name, span) {
            return d;
        }
        let mut candidates: Vec<&str> = self
            .scopes
            .iter()
            .flat_map(|s| s.keys().map(String::as_str))
            .collect();
        candidates.extend(builtins::CONSTANTS.iter().map(|(n, _)| *n));
        candidates.extend(self.visible(|g| matches!(g, Global::Const(_))));
        let d = Diagnostic::error(span, format!("unknown name `{name}`"));
        match suggest(name, candidates) {
            Some(s) => d.with_help(format!("did you mean `{s}`?")),
            None => d,
        }
    }

    fn call(&mut self, span: Span, callee: &Ident, sizes: &[SizeExpr], args: &[Arg]) -> Type {
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

        let sigs = match self.def_named(name) {
            Some(i) => vec![self.signatures[i].clone()],
            None => builtins::lookup(name),
        };
        if sigs.is_empty() {
            self.exprs(args);
            if let Some(d) = self.why_missing(name, callee.span) {
                self.report(d);
                return Type::Error;
            }
            let mut d = self.error(callee.span, format!("unknown fn or rill `{name}`"));
            if let Some(help) = renamed_conversion(name) {
                self.report(d.with_help(help));
                return Type::Error;
            }
            let candidates = self
                .visible(|g| matches!(g, Global::Def(_)))
                .into_iter()
                .chain(builtins::FUNCTIONS.iter().copied());
            if let Some(s) = suggest(name, candidates) {
                d = d.with_help(format!("did you mean `{s}`?"));
            }
            self.report(d);
            return Type::Error;
        }

        let def = self.def_named(name);
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
        let explicit_sizes = self.explicit_sizes(sizes, &sig);

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
        if let Some(explicit_sizes) = explicit_sizes {
            for (name, size) in sig.generics.iter().zip(explicit_sizes) {
                subst.sizes.insert(name.clone(), size);
            }
        }
        let mut lifts: Vec<(Vec<Size>, Span)> = Vec::new();
        for (pi, (p, slot)) in sig.params.iter().zip(&slots).enumerate() {
            let Some(ai) = *slot else { continue };
            let at = &arg_types[ai];
            let arg_span = args[ai].value.span;
            if at.is_wild() {
                ok = false;
                continue;
            }
            // Peel as few outer layers as it takes for the argument to fit.
            // A value given with `each` is made per copy, so it fits whole.
            let each = args[ai].each.is_some();
            let mut peeled = Vec::new();
            let mut inner = at;
            let fitted = loop {
                let mut trial = subst.clone();
                if unify(&p.ty, inner, &mut trial, &sig.generics) {
                    break Some(trial);
                }
                match inner {
                    Type::Frame(elem, n) if !each => {
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
                let lifts_here = kind == DefKind::Rill
                    || (kind == DefKind::Builtin && pi == 0 && builtins::takes_frames(name));
                if !lifts_here {
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
                lifts.push((peeled, arg_span));
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
        // The copies take the shape with the most layers; a shorter one lines
        // up with its outer or inner end and is reused across the rest.
        let lift = lifts
            .iter()
            .fold(None::<&Vec<Size>>, |best, (l, _)| match best {
                Some(b) if b.len() >= l.len() => Some(b),
                _ => Some(l),
            })
            .cloned();
        if let Some(longest) = &lift {
            for (peeled, at) in &lifts {
                let d = if peeled.len() == longest.len() {
                    if peeled == longest {
                        continue;
                    }
                    let msg = match (longest.as_slice(), peeled.as_slice()) {
                        ([m], [n]) => format!(
                            "channel counts differ: this has {n} channels, another argument has {m}"
                        ),
                        _ => format!(
                            "this runs `{name}` over shape `{}`, another argument over shape `{}`",
                            shape(peeled),
                            shape(longest)
                        ),
                    };
                    self.error(*at, msg)
                        .with_help("arguments that run per element need the same layers, or fewer that line up")
                } else {
                    match align(longest, peeled) {
                        Align::Outer | Align::Inner => continue,
                        Align::Both => self
                            .error(
                                *at,
                                format!(
                                    "shape `{}` could line up with the outer or the inner layers of shape `{}`",
                                    shape(peeled),
                                    shape(longest)
                                ),
                            )
                            .with_help(ambiguous_help(longest)),
                        Align::Neither => self
                            .error(
                                *at,
                                format!(
                                    "this runs `{name}` over shape `{}`, another argument over shape `{}`",
                                    shape(peeled),
                                    shape(longest)
                                ),
                            )
                            .with_help(ALIGN_HELP),
                    }
                };
                self.report(d);
                ok = false;
            }
        }
        // `each` makes a value per copy, so there must be copies.
        for arg in args {
            let Some(at) = arg.each else { continue };
            if lift.is_some() && kind == DefKind::Rill {
                continue;
            }
            let d = if kind == DefKind::Rill {
                self.error(at, format!("`each` needs copies, but `{name}` runs once here"))
                    .with_help("`each` makes a value for every copy of a rill that runs per element; remove it")
            } else {
                self.error(at, format!("`each` only works on rills, and `{name}` is a {}", kind_word(kind)))
                    .with_help("`each` makes a value for every copy of a rill that runs per element; remove it")
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
            Some(layers) => layers
                .into_iter()
                .rev()
                .fold(ret, |t, n| Type::Frame(Box::new(t), n)),
        }
    }

    fn explicit_sizes(&mut self, sizes: &[SizeExpr], sig: &Signature) -> Option<Vec<Size>> {
        if sizes.is_empty() {
            return Some(Vec::new());
        }
        if sizes.len() != sig.generics.len() {
            let d = self.error(
                sizes.first().map_or(Span::new(0, 0), size_span),
                format!(
                    "`{}` takes {} size argument(s), but {} were given",
                    sig.name,
                    sig.generics.len(),
                    sizes.len()
                ),
            );
            self.report(d);
            return None;
        }
        let generics = self.generics();
        let mut out = Vec::new();
        for size in sizes {
            out.push(self.size(size, &generics).unwrap_or(Size::Const(0)));
        }
        Some(out)
    }

    /// A size: a whole number, one of `generics`, or a constant worked out
    /// here (`riff.step_count * 2`). `None` after reporting a problem.
    fn size(&mut self, size: &SizeExpr, generics: &[String]) -> Option<Size> {
        match size {
            SizeExpr::Lit(n, _) => Some(Size::Const(*n)),
            SizeExpr::Var(id) => {
                if !generics.contains(&id.name) {
                    let local = self.lookup(&id.name).map(|v| (v.id, v.kind));
                    let value = match (local, self.const_named(&id.name)) {
                        (Some((b, VarKind::Const)), _) => {
                            self.resolve(id.span, Resolution::Binding(b));
                            Some(self.local_const_vals.get(&b).cloned().unwrap_or(Err(None)))
                        }
                        (None, Some(i)) => {
                            self.resolve(id.span, Resolution::Const(i));
                            match self.const_types[i].is_wild() {
                                true => return None,
                                false => Some(self.const_vals[i].clone()),
                            }
                        }
                        _ => None,
                    };
                    if let Some(value) = value {
                        return self.size_of(id.span, value);
                    }
                }
                if builtins::constant(&id.name).is_some() && !generics.contains(&id.name) {
                    let e = self
                        .error(id.span, format!("`{}` cannot be a size", id.name))
                        .with_help("a size is a whole number known while checking, as in `8` or `riff.step_count`");
                    self.report(e);
                    return None;
                }
                if !generics.contains(&id.name) {
                    let e = self
                        .error(id.span, format!("unknown size `{}`", id.name))
                        .with_help(format!(
                            "a size is a whole number, a constant such as `riff.step_count`, or a size parameter declared after the name, as in `rill f<{}>(...)`",
                            id.name
                        ));
                    self.report(e);
                    return None;
                }
                let binding = self
                    .sizes
                    .iter()
                    .find(|(n, _)| *n == id.name)
                    .map(|&(_, b)| b)
                    .or_else(|| self.lookup(&id.name).map(|v| v.id));
                if let Some(b) = binding {
                    self.resolve(id.span, Resolution::Binding(b));
                }
                Some(Size::Var(id.name.clone()))
            }
            SizeExpr::Expr(e) => {
                let mut names = Vec::new();
                expr_names(e, &mut names);
                if let Some(name) = names.into_iter().find(|n| generics.contains(n)) {
                    let d = self
                        .error(
                            e.span,
                            format!("a size cannot be worked out from size parameter `{name}` yet"),
                        )
                        .with_help(format!(
                            "take the size as a size parameter of its own, next to `{name}`"
                        ));
                    self.report(d);
                    return None;
                }
                let t = self.expr(e);
                if t.is_wild() {
                    return None;
                }
                let value = self.const_value(e);
                let size = self.size_of(e.span, value);
                if let Some(Size::Const(n)) = size {
                    self.const_values.insert(e.id, f64::from(n));
                }
                size
            }
        }
    }

    /// The size a constant's `value` makes, or an error at `span`.
    fn size_of(&mut self, span: Span, value: ConstVal) -> Option<Size> {
        let problem = match value {
            Ok(v) if v >= 1.0 && v.fract() == 0.0 && v <= f64::from(u32::MAX) => {
                return Some(Size::Const(v as u32));
            }
            Ok(v) => self.error(span, format!("a size must be a whole number of at least 1; this is {v}")),
            Err(Some(name)) => self
                .error(span, format!("a size cannot be worked out from size parameter `{name}` yet"))
                .with_help(format!("take the size as a size parameter of its own, next to `{name}`")),
            Err(None) => self
                .error(span, "a size must be a constant whole number")
                .with_help("as in `8`, `VOICES` or `riff.step_count * 2`; values known only while playing, and `RATE`, cannot be sizes"),
        };
        self.report(problem);
        None
    }

    /// Check the top-level `const`s, each after the ones it uses.
    fn check_consts(&mut self, consts: &[ConstDecl], defs: &[Item]) {
        for (i, c) in consts.iter().enumerate() {
            let m = self.modules.konst(i);
            if self.own_first(m, &c.name.name, |g| matches!(g, Global::Const(_)))
                != Some(Global::Const(i))
            {
                let e = self.error(
                    c.name.span,
                    format!("`{}` is already a `const`", c.name.name),
                );
                self.report(e);
            }
        }
        let deps: Vec<Vec<usize>> = consts
            .iter()
            .enumerate()
            .map(|(i, c)| {
                self.module = self.modules.konst(i);
                let mut names = Vec::new();
                const_names_in(&c.value, &mut names);
                let mut deps: Vec<usize> =
                    names.iter().filter_map(|n| self.const_named(n)).collect();
                deps.dedup();
                deps
            })
            .collect();

        // Depth first, so each comes after what it uses; a way back to one
        // still being visited is a cycle.
        let mut state = vec![0u8; consts.len()];
        let mut in_cycle = vec![false; consts.len()];
        let mut path = Vec::new();
        fn visit(
            i: usize,
            deps: &[Vec<usize>],
            state: &mut [u8],
            in_cycle: &mut [bool],
            path: &mut Vec<usize>,
            order: &mut Vec<usize>,
            cycles: &mut Vec<Vec<usize>>,
        ) {
            state[i] = 1;
            path.push(i);
            for &j in &deps[i] {
                match state[j] {
                    0 => visit(j, deps, state, in_cycle, path, order, cycles),
                    1 => {
                        let from = path.iter().position(|&k| k == j).expect("on the path");
                        let cycle = path[from..].to_vec();
                        for &k in &cycle {
                            in_cycle[k] = true;
                        }
                        cycles.push(cycle);
                    }
                    _ => {}
                }
            }
            path.pop();
            state[i] = 2;
            if !in_cycle[i] {
                order.push(i);
            }
        }
        let mut order = Vec::new();
        let mut cycles = Vec::new();
        for i in 0..consts.len() {
            if state[i] == 0 {
                visit(
                    i,
                    &deps,
                    &mut state,
                    &mut in_cycle,
                    &mut path,
                    &mut order,
                    &mut cycles,
                );
            }
        }
        for cycle in cycles {
            let names: Vec<&str> = cycle
                .iter()
                .chain(cycle.first())
                .map(|&k| consts[k].name.name.as_str())
                .collect();
            let first = &consts[cycle[0]];
            let e = self
                .error(
                    first.name.span,
                    format!(
                        "`{}` is worked out from itself: {}",
                        first.name.name,
                        names.join(" → ")
                    ),
                )
                .with_help("a `const` can use other `const`s, but not in a circle");
            self.report(e);
        }

        let _ = defs;
        for &i in &order {
            let c = &consts[i];
            self.module = self.modules.konst(i);
            self.scopes = vec![HashMap::new()];
            self.scope_spans = vec![c.span];
            self.place = Place::Fn;
            self.current = None;
            self.sizes.clear();
            let mut names = Vec::new();
            const_names_in(&c.value, &mut names);
            if let Some(n) = names.iter().find(|n| self.def_named(n).is_some()) {
                let e = self
                    .error(c.value.span, format!("`{n}` cannot be used in a `const`"))
                    .with_help("a `const` is worked out before any fn or rill runs: from numbers, pitches, other `const`s and built-in functions of them");
                self.report(e);
                continue;
            }
            let (ty, value) = self.const_decl(c);
            self.const_types[i] = ty;
            self.const_vals[i] = value;
        }
        self.scopes.clear();
        self.scope_spans.clear();
        self.const_order = order;
    }

    /// Check a `const`: its type and that its value is a constant.
    fn const_decl(&mut self, c: &ConstDecl) -> (Type, ConstVal) {
        let declared =
            c.ty.as_ref()
                .map(|te| self.resolve_type(te, &self.generics()));
        let t = self.expr_expect(&c.value, declared.as_ref());
        let mut ok = !t.is_wild();
        if ok && let Some(part) = self.non_const_part(&c.value) {
            let e = self
                .error(part.span, format!("the value of `{}` must be a constant", c.name.name))
                .with_help("a `const` is worked out once, before audio starts: from numbers, pitches, other `const`s and built-in functions of them; use `let` for a value worked out while playing");
            self.report(e);
            ok = false;
        }
        if matches!(t, Type::Fn(..)) {
            let e = self
                .error(c.span, "a `const` cannot hold a function")
                .with_help("define it as a `fn`, or bind it with `let`");
            self.report(e);
            ok = false;
        }
        let bound = match declared {
            Some(declared) => {
                if !coerces(&t, &declared) {
                    let e = mismatch(c.value.span, &format!("`{}`", c.name.name), &declared, &t);
                    self.report(e);
                    ok = false;
                }
                declared
            }
            None => t,
        };
        let value = match ok {
            true => self.const_value(&c.value),
            false => Err(None),
        };
        (bound, value)
    }

    /// The innermost part of `e` that is not a constant, if any.
    fn non_const_part<'e>(&self, e: &'e Expr) -> Option<&'e Expr> {
        if self.is_const(e) {
            return None;
        }
        let inner = match &e.kind {
            ExprKind::Unary(_, x) | ExprKind::Cast(x, _) | ExprKind::Repeat(x, _) => {
                self.non_const_part(x)
            }
            ExprKind::Binary(_, a, b) => self.non_const_part(a).or_else(|| self.non_const_part(b)),
            ExprKind::Frame(xs) => xs.iter().find_map(|x| self.non_const_part(x)),
            ExprKind::Call { args, .. } => args.iter().find_map(|a| self.non_const_part(&a.value)),
            _ => None,
        };
        inner.or(Some(e))
    }

    /// Top-level `const`s whose names are taken by something else.
    fn const_clashes(&mut self, program: &Program) {
        for (i, c) in program.consts.iter().enumerate() {
            let n = c.name.name.as_str();
            let m = program.modules.konst(i);
            let what = if self.own_has(m, n, |g| matches!(g, Global::Def(_))) {
                "a fn or rill"
            } else if self.own_has(m, n, |g| matches!(g, Global::Seq(_))) {
                "a sequence"
            } else if self.own_has(m, n, |g| matches!(g, Global::Event(_))) {
                "an event"
            } else if builtins::constant(n).is_some() {
                "a built-in constant"
            } else if pitch_literal(n).is_some() {
                "a note"
            } else if !builtins::lookup(n).is_empty() {
                "a built-in function"
            } else {
                continue;
            };
            let e = self
                .error(c.name.span, format!("`{n}` is already the name of {what}"))
                .with_help("give the `const` a name of its own");
            self.report(e);
        }
    }

    /// The value of `e` if it is known while checking: numbers, arithmetic,
    /// casts and the fixed facts of sequences. `Err(Some(n))` if it depends
    /// on size parameter `n`, `Err(None)` if it is not known.
    fn const_value(&self, e: &Expr) -> Result<f64, Option<String>> {
        match &e.kind {
            ExprKind::Number {
                value, unit: None, ..
            } => Ok(*value),
            ExprKind::Unary(UnOp::Neg, x) => Ok(-self.const_value(x)?),
            ExprKind::Unary(UnOp::Plus, x) => self.const_value(x),
            ExprKind::Binary(op, a, b) => {
                let (a, b) = (self.const_value(a)?, self.const_value(b)?);
                match op {
                    BinOp::Add => Ok(a + b),
                    BinOp::Sub => Ok(a - b),
                    BinOp::Mul => Ok(a * b),
                    BinOp::Div => Ok(a / b),
                    BinOp::Rem => Ok(a % b),
                    _ => Err(None),
                }
            }
            ExprKind::Cast(x, TypeExpr::Named(t)) if t.name == "Int" => {
                Ok(self.const_value(x)?.trunc())
            }
            ExprKind::Cast(x, TypeExpr::Named(t))
                if matches!(t.name.as_str(), "Float" | "Sample") =>
            {
                self.const_value(x)
            }
            ExprKind::Field(base, field) => match self.seq_of(base) {
                Some(j) => seq_field(&self.seq_facts[j], &field.name)
                    .map(|(_, v)| v)
                    .ok_or(None),
                None => Err(None),
            },
            ExprKind::Name(n) => match self.lookup(n) {
                Some(v) if v.kind == VarKind::Size => Err(Some(n.clone())),
                Some(v) if v.kind == VarKind::Const => self
                    .local_const_vals
                    .get(&v.id)
                    .cloned()
                    .unwrap_or(Err(None)),
                Some(_) => Err(None),
                None => match self.const_named(n) {
                    Some(i) => self.const_vals[i].clone(),
                    None => Err(None),
                },
            },
            _ => Err(None),
        }
    }

    /// The sequence `e` names, if it is a name that is not shadowed.
    fn seq_of(&self, e: &Expr) -> Option<usize> {
        match &e.kind {
            ExprKind::Name(n) if self.lookup(n).is_none() => self.seq_named(n),
            _ => None,
        }
    }

    /// Report every `each` in `args`, where `why` says there are no copies.
    /// Returns whether there were none.
    fn no_each(&mut self, args: &[Arg], why: &str) -> bool {
        let mut ok = true;
        for at in args.iter().filter_map(|a| a.each) {
            let d = self
                .error(at, format!("`each` needs copies, but {why}"))
                .with_help("`each` makes a value for every copy of a rill that runs per element; remove it");
            self.report(d);
            ok = false;
        }
        ok
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
        if self.current.is_some()
            && let Some(callee) = self.def_named(name)
        {
            self.calls
                .entry(self.current_def)
                .or_default()
                .push((callee, at));
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
        let mut ok = self.no_each(
            args,
            &format!("`{name}` is a function value, which never runs per element"),
        );
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
                    && (self.def_named(name).is_some() || !builtins::lookup(name).is_empty()) =>
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
        let sigs = match self.def_named(name) {
            Some(i) => vec![self.signatures[i].clone()],
            None => builtins::lookup(name),
        };
        let r = match self.def_named(name) {
            Some(i) => Resolution::Def(i),
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
            ExprKind::Repeat(x, _) => self.is_const(x),
            ExprKind::Name(n) => match self.lookup(n) {
                Some(v) => matches!(v.kind, VarKind::Size | VarKind::Const),
                None => {
                    self.const_named(n).is_some()
                        || builtins::constant(n).is_some()
                        || pitch_literal(n).is_some()
                        // A named fn is a fixed value.
                        || self.def_named(n).is_some_and(|i| self.signatures[i].kind == DefKind::Fn)
                        || !builtins::lookup(n).is_empty()
                }
            },
            ExprKind::Call { callee, args, .. } => {
                let name = callee.name.as_str();
                self.lookup(name).is_none()
                    && self.def_named(name).is_none()
                    && !builtins::lookup(name).is_empty()
                    && args.iter().all(|a| self.is_const(&a.value))
            }
            ExprKind::Field(base, _) => self.seq_of(base).is_some(),
            ExprKind::If { .. }
            | ExprKind::Block(_)
            | ExprKind::Index(..)
            | ExprKind::Range { .. }
            | ExprKind::Fn { .. }
            | ExprKind::Invoke { .. }
            | ExprKind::Halt { .. } => false,
        }
    }

    fn check_recursion(&mut self) {
        let mut reported: HashSet<Vec<usize>> = HashSet::new();
        for start in 0..self.signatures.len() {
            // Depth-first search for a path back to `start`.
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            let mut on_path: Vec<usize> = vec![start];
            let mut visited: HashSet<usize> = HashSet::new();
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
                        path.push(start);
                        let chain = path
                            .iter()
                            .map(|&n| format!("`{}`", self.signatures[n].name))
                            .collect::<Vec<_>>()
                            .join(" -> ");
                        let d = Diagnostic::error(*at, format!("recursion is not allowed: {chain}"))
                            .with_help("the run stage has no unbounded loops, and every rill instance needs a fixed amount of state");
                        self.report(d);
                    }
                    continue;
                }
                if visited.insert(*callee) {
                    stack.push((*callee, 0));
                    on_path.push(*callee);
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

fn kind_word(kind: DefKind) -> &'static str {
    match kind {
        DefKind::Fn => "fn",
        DefKind::Rill => "rill",
        DefKind::Builtin => "built-in function",
    }
}

fn var_word(kind: VarKind) -> &'static str {
    match kind {
        VarKind::Param => "parameter",
        VarKind::Let => "value",
        VarKind::State => "state variable",
        VarKind::Size => "size",
        VarKind::Loop => "loop variable",
        VarKind::EventParam => "event",
        VarKind::Const => "constant",
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
    match t {
        Type::Frame(elem, Size::Var(v)) => v == name || mentions_size(elem, name),
        Type::Frame(elem, _) => mentions_size(elem, name),
        Type::Fn(params, ret) => {
            params.iter().any(|p| mentions_size(p, name)) || mentions_size(ret, name)
        }
        _ => false,
    }
}

/// The module declaration `g` is written in. Events from `written` on are
/// made by sequences, twelve each.
fn global_module(modules: &Modules, written: usize, g: Global) -> usize {
    match g {
        Global::Def(i) => modules.item(i),
        Global::Event(i) if i < written => modules.event(i),
        Global::Event(i) => modules.seq((i - written) / EventKind::SEQ.len()),
        Global::Seq(j) => modules.seq(j),
        Global::Const(i) => modules.konst(i),
    }
}

/// One module's own top-level declarations, by name.
type OwnNames = HashMap<String, Vec<Global>>;

/// Per module: its own declarations by name, the names it can use, and the
/// declarations that are exported.
fn build_scopes(program: &Program) -> (Vec<OwnNames>, Vec<Scope>, HashSet<Global>) {
    let modules = &program.modules;
    let n = modules.count();
    let written = program.events.len();
    let mut own: Vec<OwnNames> = vec![HashMap::new(); n];
    let mut exported = HashSet::new();
    let mut add = |name: &str, g: Global, export: bool| {
        let m = global_module(modules, written, g);
        own[m].entry(name.to_owned()).or_default().push(g);
        if export {
            exported.insert(g);
        }
    };
    for (i, item) in program.items.iter().enumerate() {
        let d = item.def();
        add(&d.name.name, Global::Def(i), d.export.is_some());
    }
    for (i, e) in program.events.iter().enumerate() {
        add(&e.name.name, Global::Event(i), e.export.is_some());
    }
    for (j, seq) in program.seqs.iter().enumerate() {
        add(&seq.name.name, Global::Seq(j), seq.export.is_some());
        for (k, kind) in EventKind::SEQ.iter().enumerate() {
            let name = format!("{}_{}", seq.name.name, kind.suffix());
            let i = written + j * EventKind::SEQ.len() + k;
            add(&name, Global::Event(i), seq.export.is_some());
        }
    }
    for (i, c) in program.consts.iter().enumerate() {
        add(&c.name.name, Global::Const(i), c.export.is_some());
    }

    let scopes = (0..n)
        .map(|m| {
            let mut scope = Scope {
                names: own[m].clone(),
                imported: HashSet::new(),
            };
            for &t in modules.sees(m) {
                let t = t as usize;
                let mut names: Vec<(&String, &Vec<Global>)> = own[t].iter().collect();
                names.sort_by_key(|(n, _)| n.as_str());
                for (name, gs) in names {
                    if own[m].contains_key(name) {
                        continue;
                    }
                    for g in gs.iter().filter(|g| exported.contains(g)) {
                        let entry = scope.names.entry(name.clone()).or_default();
                        if !entry.contains(g) {
                            entry.push(*g);
                        }
                        scope.imported.insert(name.clone());
                    }
                }
            }
            scope
        })
        .collect();
    (own, scopes, exported)
}

/// The path module `from` imports module `to` by: both are paths from the
/// root file's folder.
fn relative_import(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = from.split('/').collect::<Vec<_>>();
    let from_dir = &from_dir[..from_dir.len().saturating_sub(1)];
    let to: Vec<&str> = to.split('/').collect();
    let common = from_dir.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; from_dir.len() - common];
    parts.extend(&to[common..]);
    parts.join("/")
}

/// The names a `const`'s value uses, for working out the order to check
/// them in.
fn const_names_in(e: &Expr, out: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Name(n) => out.push(n.clone()),
        ExprKind::Unary(_, x)
        | ExprKind::Cast(x, _)
        | ExprKind::Field(x, _)
        | ExprKind::Repeat(x, _) => const_names_in(x, out),
        ExprKind::Binary(_, a, b) | ExprKind::Index(a, b) => {
            const_names_in(a, out);
            const_names_in(b, out);
        }
        ExprKind::Frame(xs) => {
            for x in xs {
                const_names_in(x, out);
            }
        }
        ExprKind::Call { callee, args, .. } => {
            out.push(callee.name.clone());
            for a in args {
                const_names_in(&a.value, out);
            }
        }
        _ => {}
    }
}

/// The plain names `e` reads, for spotting size parameters in sizes.
fn expr_names(e: &Expr, out: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Name(n) => out.push(n.clone()),
        ExprKind::Unary(_, x) | ExprKind::Cast(x, _) | ExprKind::Field(x, _) => expr_names(x, out),
        ExprKind::Binary(_, a, b) => {
            expr_names(a, out);
            expr_names(b, out);
        }
        _ => {}
    }
}

fn size_span(size: &SizeExpr) -> Span {
    size.span()
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

/// Whether the operands of an operator on frames line up with their inner
/// layers (`true`) or their outer ones. A side with fewer layers lines up
/// with whichever end of the other it matches.
pub(crate) fn operand_alignment(a: &Type, b: &Type) -> Result<bool, OpError> {
    let (sa, sb) = (frame_shape(a), frame_shape(b));
    if sa.len() == sb.len() {
        if sa != sb {
            return Err((
                format!("channel counts differ: `{a}` and `{b}`"),
                Some("operators on frames work channel by channel, so both sides need the same count".into()),
            ));
        }
        return Ok(false);
    }
    let (long, short, lt, st) = if sa.len() > sb.len() {
        (&sa, &sb, a, b)
    } else {
        (&sb, &sa, b, a)
    };
    match align(long, short) {
        Align::Outer => Ok(false),
        Align::Inner => Ok(true),
        Align::Both => Err((
            format!("`{st}` could line up with the outer or the inner layers of `{lt}`"),
            Some(ambiguous_help(long)),
        )),
        Align::Neither => Err((
            format!("shapes do not line up: `{a}` and `{b}`"),
            Some(ALIGN_HELP.into()),
        )),
    }
}

const ALIGN_HELP: &str =
    "a value with fewer layers must match the outer or the inner layers of the other";

fn ambiguous_help(long: &[Size]) -> String {
    format!(
        "say which: to reuse it for every outer element, repeat it, as in `[x; {}]`; for one \
         value per outer element, build the full shape",
        long[0]
    )
}

/// Apply `leaf` to the scalars of `a` and `b`, pairing frame layers up. With
/// `inner`, the side with more layers is taken apart first, so the other
/// lines up with its inner layers.
fn zip_types(
    a: &Type,
    b: &Type,
    inner: bool,
    leaf: &dyn Fn(&Type, &Type) -> Result<Type, OpError>,
) -> Result<Type, OpError> {
    let (da, db) = (a.depth(), b.depth());
    let split_a = matches!(a, Type::Frame(..)) && !(inner && da < db);
    let split_b = matches!(b, Type::Frame(..)) && !(inner && db < da);
    let t = match (a, b) {
        (Type::Frame(ea, n), Type::Frame(eb, _)) if split_a && split_b => {
            Type::Frame(Box::new(zip_types(ea, eb, inner, leaf)?), n.clone())
        }
        (Type::Frame(ea, n), _) if split_a => {
            Type::Frame(Box::new(zip_types(ea, b, inner, leaf)?), n.clone())
        }
        (_, Type::Frame(eb, n)) if split_b => {
            Type::Frame(Box::new(zip_types(a, eb, inner, leaf)?), n.clone())
        }
        _ => leaf(a, b)?,
    };
    Ok(t)
}

fn arith(op: BinOp, a: &Type, b: &Type) -> Result<Type, OpError> {
    if a.is_wild() || b.is_wild() {
        return Ok(Type::Error);
    }
    if matches!(a, Type::Frame(..)) || matches!(b, Type::Frame(..)) {
        let inner = operand_alignment(a, b)?;
        return zip_types(a, b, inner, &|x, y| arith(op, x, y));
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
    if matches!(
        (op, a, b),
        (BinOp::Add | BinOp::Sub, Type::Freq, Type::Interval)
    ) {
        return Ok(Type::Freq);
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

/// `a`, `b` and `c`
/// What is fixed about a sequence when the program is built, and the same
/// for every instance: what `riff.step_count` and the others read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeqFacts {
    pub step_count: u32,
    /// The meter, as in 3/4.
    pub beats_per_bar: u32,
    pub beat_unit: u32,
    /// A step, as a fraction of a whole note.
    pub step_size: f64,
    pub instances: u32,
}

impl SeqFacts {
    fn of(seq: &SeqDecl, instances: Option<f64>) -> SeqFacts {
        let setting = |name: &str| {
            seq.settings
                .iter()
                .find(|s| s.name.name == name)
                .and_then(|s| fraction(&s.value))
                .filter(|&(n, d)| n >= 1 && d >= 1)
        };
        let (beats_per_bar, beat_unit) = setting("meter").unwrap_or((4, 4));
        let (n, d) = setting("step").unwrap_or((1, 8));
        SeqFacts {
            step_count: seq.steps.len() as u32,
            beats_per_bar,
            beat_unit,
            step_size: f64::from(n) / f64::from(d),
            instances: instances
                .filter(|v| *v >= 1.0 && v.fract() == 0.0 && *v <= 65_535.0)
                .map_or(64, |v| v as u32),
        }
    }

    /// How many beats one step lasts.
    pub fn step_beats(&self) -> f64 {
        self.step_size * f64::from(self.beat_unit)
    }
}

/// The fields of a sequence, with what each means.
pub const SEQ_FIELDS: &[(&str, &str)] = &[
    ("step_count", "How many steps it has, rests included."),
    ("beats_per_bar", "The top of its meter: 3 in 3/4."),
    (
        "beat_unit",
        "The bottom of its meter: 4 in 3/4, so a beat is a quarter note.",
    ),
    (
        "step_size",
        "How long a step is, as a fraction of a whole note: 0.125 for `step: 1/8`.",
    ),
    (
        "steps_per_beat",
        "How many steps make a beat: 2 for eighth-note steps in 4/4.",
    ),
    ("beat_count", "How long it is, in beats."),
    (
        "bar_count",
        "How long it is, in bars; not a whole number when the last bar is short.",
    ),
    ("instances", "How many instances can play at once."),
];

/// The type and value of field `name` of a sequence with `facts`.
pub fn seq_field(facts: &SeqFacts, name: &str) -> Option<(Type, f64)> {
    let beats = f64::from(facts.step_count) * facts.step_beats();
    Some(match name {
        "step_count" => (Type::Int, f64::from(facts.step_count)),
        "beats_per_bar" => (Type::Int, f64::from(facts.beats_per_bar)),
        "beat_unit" => (Type::Int, f64::from(facts.beat_unit)),
        "step_size" => (Type::Float, facts.step_size),
        "steps_per_beat" => (Type::Float, 1.0 / facts.step_beats()),
        "beat_count" => (Type::Float, beats),
        "bar_count" => (Type::Float, beats / f64::from(facts.beats_per_bar)),
        "instances" => (Type::Int, f64::from(facts.instances)),
        _ => return None,
    })
}

fn listing(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => quoted.join(""),
    }
}

/// `4/4` or `1/8`: two whole numbers divided, as a time signature or a note
/// value.
pub fn fraction(e: &Expr) -> Option<(u32, u32)> {
    let ExprKind::Binary(BinOp::Div, a, b) = &e.kind else {
        return None;
    };
    let whole = |x: &Expr| match x.kind {
        ExprKind::Number {
            value,
            unit: None,
            integral: true,
        } if value <= f64::from(u32::MAX) => Some(value as u32),
        _ => None,
    };
    Some((whole(a)?, whole(b)?))
}

/// The settings of a sequence apart from `meter` and `step`: their type,
/// and whether they are fixed when the sequence is declared.
pub fn seq_setting(name: &str) -> Option<(Type, bool)> {
    Some(match name {
        "tempo" => (Type::Freq, false),
        "gate" | "velocity" => (Type::Float, false),
        "repeat" => (Type::Int, false),
        "loop" => (Type::Bool, false),
        "instances" => (Type::Int, true),
        _ => return None,
    })
}

const SEQ_SETTINGS: &str =
    "`meter`, `step`, `tempo`, `gate`, `velocity`, `repeat`, `loop` and `instances`";

/// The type of field `name` of an event payload of kind `kind`.
fn event_field_type(kind: EventKind, name: &str) -> Option<Type> {
    match (kind, name) {
        (EventKind::NoteOn | EventKind::NoteOff, "pitch") => Some(Type::Pitch),
        (EventKind::NoteOn | EventKind::NoteOff, "instance") => Some(Type::Int),
        (EventKind::NoteOn, "velocity") | (EventKind::NoteOff, "release") => Some(Type::Float),
        // The events sequences make carry whole numbers.
        (kind, name) if kind.is_seq_only() && kind.fields().contains(&name) => Some(Type::Int),
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
    // The entry is looked up in the root file, along with what it imports.
    let found = match checked.scopes.first() {
        Some(scope) => scope.def(entry),
        None => program
            .items
            .iter()
            .position(|i| i.def().name.name == entry),
    }
    .map(|i| (&program.items[i], &checked.signatures[i]));
    let Some((item, sig)) = found else {
        // The rills the root file can run: its own, then imported ones.
        let usable = |i: usize| match checked.scopes.first() {
            Some(scope) => scope.def(&program.items[i].def().name.name) == Some(i),
            None => true,
        };
        let mut rills: Vec<&str> = (0..program.items.len())
            .filter(|&i| matches!(program.items[i], Item::Rill(_)) && usable(i))
            .map(|i| program.items[i].def().name.name.as_str())
            .collect();
        rills.dedup();
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
