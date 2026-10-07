//! Tree view of a checked program, for `rill check ast`.
//!
//! ```text
//! rill peak Sample
//! ├─ param x: Sample
//! ├─ param release: Time
//! │  └─ 300ms : Time
//! └─ body
//!    ├─ state level: Sample
//!    │  └─ 0 : number
//!    ...
//! ```
//!
//! Every expression is followed by the type the checker gave it.
//!
//! Colours use only the 16 base ANSI slots, so they follow the terminal
//! theme: keywords 13, tree labels such as `param` and `body` 14, names of
//! fns and rills 11, other identifiers 14, types 11, operators 9, and
//! constants 3.

use super::ast::*;
use super::check::Checked;
use super::diag::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Plain,
    /// A word of the Rill language itself, like `rill` or `return`.
    Keyword,
    /// A label the tree adds, like `param`, `body` or `binary`.
    Property,
    Ident,
    /// The name of a fn, rill or built-in function.
    Callable,
    Op,
    Type,
    Value,
}

impl Style {
    fn ansi(self) -> Option<&'static str> {
        match self {
            Style::Plain => None,
            Style::Keyword => Some("\x1b[95m"),
            Style::Property => Some("\x1b[96m"),
            Style::Ident => Some("\x1b[96m"),
            Style::Callable => Some("\x1b[93m"),
            Style::Op => Some("\x1b[91m"),
            Style::Type => Some("\x1b[93m"),
            Style::Value => Some("\x1b[33m"),
        }
    }
}

const RESET: &str = "\x1b[0m";

/// A line of the tree, as styled pieces.
#[derive(Default)]
struct Label(Vec<(Style, String)>);

impl Label {
    fn push(mut self, style: Style, text: impl Into<String>) -> Label {
        self.0.push((style, text.into()));
        self
    }

    fn kw(self, text: impl Into<String>) -> Label {
        self.push(Style::Keyword, text)
    }

    fn prop(self, text: impl Into<String>) -> Label {
        self.push(Style::Property, text)
    }

    fn callable(self, text: impl Into<String>) -> Label {
        self.push(Style::Callable, text)
    }

    fn ident(self, text: impl Into<String>) -> Label {
        self.push(Style::Ident, text)
    }

    fn op(self, text: impl Into<String>) -> Label {
        self.push(Style::Op, text)
    }

    fn ty(self, text: impl Into<String>) -> Label {
        self.push(Style::Type, text)
    }

    fn value(self, text: impl Into<String>) -> Label {
        self.push(Style::Value, text)
    }

    fn plain(self, text: impl Into<String>) -> Label {
        self.push(Style::Plain, text)
    }

    fn write(&self, color: bool, out: &mut String) {
        for (style, text) in &self.0 {
            match style.ansi().filter(|_| color) {
                Some(code) => {
                    out.push_str(code);
                    out.push_str(text);
                    out.push_str(RESET);
                }
                None => out.push_str(text),
            }
        }
    }
}

fn kw(text: &str) -> Label {
    Label::default().kw(text)
}

fn prop(text: &str) -> Label {
    Label::default().prop(text)
}

struct Node {
    label: Label,
    children: Vec<Node>,
}

impl Node {
    fn leaf(label: Label) -> Node {
        Node {
            label,
            children: Vec::new(),
        }
    }

    fn new(label: Label, children: Vec<Node>) -> Node {
        Node { label, children }
    }
}

/// Render `program` as a tree annotated with the types in `checked`, with
/// ANSI colours if `color` is set.
pub fn tree(src: &str, program: &Program, checked: &Checked, color: bool) -> String {
    let p = Printer { src, checked };
    // Definitions and event declarations, in source order.
    let mut nodes: Vec<(u32, Node)> = program
        .items
        .iter()
        .map(|item| (item.def().span.start, p.item(item)))
        .chain(program.events.iter().map(|e| (e.span.start, p.event(e))))
        .chain(program.seqs.iter().map(|s| (s.span.start, p.seq(s))))
        .collect();
    nodes.sort_by_key(|(start, _)| *start);
    let mut out = String::new();
    for (i, (_, node)) in nodes.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render(node, "", "", color, &mut out);
    }
    out
}

fn render(node: &Node, first: &str, rest: &str, color: bool, out: &mut String) {
    out.push_str(first);
    node.label.write(color, out);
    out.push('\n');
    let last = node.children.len().saturating_sub(1);
    for (i, child) in node.children.iter().enumerate() {
        let (branch, cont) = if i == last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        render(
            child,
            &format!("{rest}{branch}"),
            &format!("{rest}{cont}"),
            color,
            out,
        );
    }
}

struct Printer<'a> {
    src: &'a str,
    checked: &'a Checked,
}

fn size_span(size: &SizeExpr) -> Span {
    match size {
        SizeExpr::Lit(_, span) => *span,
        SizeExpr::Var(id) => id.span,
    }
}

impl Printer<'_> {
    fn text(&self, span: Span) -> &str {
        &self.src[span.start as usize..span.end as usize]
    }

    /// `event keys note_on(sender: 5, channel: 1)`
    fn event(&self, e: &EventDecl) -> Node {
        let mut label = kw("event")
            .plain(" ")
            .callable(&e.name.name)
            .plain(" ")
            .ty(&e.kind.name);
        if !e.filters.is_empty() {
            label = label.plain("(");
            for (i, f) in e.filters.iter().enumerate() {
                if i > 0 {
                    label = label.plain(", ");
                }
                label = label
                    .ident(&f.name.name)
                    .plain(": ")
                    .value(self.text(f.value.span));
            }
            label = label.plain(")");
        }
        Node::leaf(label)
    }

    fn item(&self, item: &Item) -> Node {
        match item {
            Item::Fn(d) => self.def("fn", d),
            Item::Rill(d) => self.def("rill", d),
        }
    }

    fn def(&self, keyword: &str, d: &Def) -> Node {
        let mut label = kw(keyword).plain(" ").callable(&d.name.name);
        if !d.generics.is_empty() {
            label = label.plain("<");
            for (i, g) in d.generics.iter().enumerate() {
                if i > 0 {
                    label = label.plain(", ");
                }
                label = label.ty(&g.name);
            }
            label = label.plain(">");
        }
        label = label.plain(" ").ty(self.text(d.ret.span()));
        if let Some(rate) = &d.rate {
            label = label.plain(" ").op("@").plain(" ").kw("rate");
            if rate.den != 1 {
                label = label
                    .plain(" ")
                    .op("/")
                    .plain(" ")
                    .value(rate.den.to_string());
            } else if rate.num != 1 {
                label = label
                    .plain(" ")
                    .op("*")
                    .plain(" ")
                    .value(rate.num.to_string());
            }
        }

        let mut children: Vec<Node> = d
            .params
            .iter()
            .map(|p| {
                let label = prop("param")
                    .plain(" ")
                    .ident(&p.name.name)
                    .plain(": ")
                    .ty(self.text(p.ty.span()));
                match &p.default {
                    Some(e) => Node::new(label, vec![self.expr(e)]),
                    None => Node::leaf(label),
                }
            })
            .collect();
        children.push(self.block(prop("body"), &d.body));
        Node::new(label, children)
    }

    fn block(&self, label: Label, b: &Block) -> Node {
        Node::new(label, b.stmts.iter().map(|s| self.stmt(s)).collect())
    }

    fn stmt(&self, s: &Stmt) -> Node {
        let annotated = |keyword: &str, name: &Ident, ty: &Option<TypeExpr>| {
            let label = kw(keyword).plain(" ").ident(&name.name);
            match ty {
                Some(t) => label.plain(": ").ty(self.text(t.span())),
                None => label,
            }
        };
        match s {
            Stmt::Let {
                name, ty, value, ..
            } => Node::new(
                annotated("let", name, ty),
                value.iter().map(|e| self.expr(e)).collect(),
            ),
            Stmt::State { name, ty, init, .. } => {
                Node::new(annotated("state", name, ty), vec![self.expr(init)])
            }
            Stmt::Assign { target, value, .. } => Node::new(
                prop("assign").plain(" ").ident(&target.name().name),
                vec![self.expr(value)],
            ),
            Stmt::Return { value, .. } => Node::new(kw("return"), vec![self.expr(value)]),
            Stmt::EventHandler {
                name,
                params,
                mode,
                body,
                ..
            } => {
                let mut label = kw("on").plain(" ").callable(&name.name).plain("(");
                for (i, param) in params.iter().enumerate() {
                    if i > 0 {
                        label = label.plain(", ");
                    }
                    label = label.ident(&param.name);
                }
                label = label.plain(")");
                label = match mode {
                    HandlerMode::Plain => label,
                    HandlerMode::Claim { tail: None } => label.plain(" ").kw("claim"),
                    HandlerMode::Claim { tail: Some(t) } => label
                        .plain(" ")
                        .kw("claim")
                        .plain("(tail: ")
                        .value(self.text(t.span))
                        .plain(")"),
                    HandlerMode::Release => label.plain(" ").kw("release"),
                };
                Node::new(label, vec![self.block(prop("body"), body)])
            }
            Stmt::For {
                name, iter, body, ..
            } => Node::new(
                kw("for").plain(" ").ident(&name.name),
                vec![
                    Node::new(prop("in"), vec![self.expr(iter)]),
                    self.block(prop("body"), body),
                ],
            ),
            Stmt::Expr(e) => self.expr(e),
        }
    }

    fn expr(&self, e: &Expr) -> Node {
        let ty = self.checked.types[e.id as usize].to_string();
        let typed = |label: Label| label.plain(" : ").ty(&ty);
        match &e.kind {
            ExprKind::Number { .. } | ExprKind::Bool(_) => {
                Node::leaf(typed(Label::default().value(self.text(e.span))))
            }
            ExprKind::Name(name) if super::builtins::constant(name).is_some() => {
                Node::leaf(typed(Label::default().value(name)))
            }
            ExprKind::Name(name) => Node::leaf(typed(Label::default().ident(name))),
            ExprKind::Unary(op, x) => {
                let symbol = match op {
                    UnOp::Neg => "-",
                    UnOp::Plus => "+",
                    UnOp::Not => "!",
                };
                Node::new(
                    typed(prop("unary").plain(" ").op(symbol)),
                    vec![self.expr(x)],
                )
            }
            ExprKind::Binary(op, a, b) => Node::new(
                typed(prop("binary").plain(" ").op(op.symbol())),
                vec![self.expr(a), self.expr(b)],
            ),
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => Node::new(
                typed(
                    prop("range")
                        .plain(" ")
                        .op(if *inclusive { "..=" } else { ".." }),
                ),
                vec![self.expr(start), self.expr(end)],
            ),
            ExprKind::Call {
                callee,
                sizes,
                args,
                piped,
            } => {
                let mut label = prop("call").plain(" ").callable(&callee.name);
                if !sizes.is_empty() {
                    label = label.plain("<");
                    for (i, size) in sizes.iter().enumerate() {
                        if i > 0 {
                            label = label.plain(", ");
                        }
                        label = label.ty(self.text(size_span(size)));
                    }
                    label = label.plain(">");
                }
                if *piped {
                    label = label.plain(" (").prop("piped").plain(")");
                }
                let children = args
                    .iter()
                    .map(|a| {
                        let mut node = self.expr(&a.value);
                        if let Some(name) = &a.name {
                            let mut prefixed = Label::default().ident(&name.name).plain(": ");
                            prefixed.0.append(&mut node.label.0);
                            node.label = prefixed;
                        }
                        node
                    })
                    .collect();
                Node::new(typed(label), children)
            }
            ExprKind::If { cond, then, els } => {
                let mut children = vec![
                    Node::new(prop("cond"), vec![self.expr(cond)]),
                    self.block(prop("then"), then),
                ];
                if let Some(els) = els {
                    children.push(match &els.kind {
                        ExprKind::Block(b) => self.block(kw("else"), b),
                        _ => Node::new(kw("else"), vec![self.expr(els)]),
                    });
                }
                Node::new(typed(kw("if")), children)
            }
            ExprKind::Block(b) => self.block(typed(prop("block")), b),
            ExprKind::Frame(elems) => Node::new(
                typed(prop("frame")),
                elems.iter().map(|x| self.expr(x)).collect(),
            ),
            ExprKind::Index(base, index) => Node::new(
                typed(prop("index")),
                vec![self.expr(base), self.expr(index)],
            ),
            ExprKind::Field(base, field) => Node::new(
                typed(prop("field").plain(" .").ident(&field.name)),
                vec![self.expr(base)],
            ),
            ExprKind::Cast(x, to) => Node::new(
                typed(
                    prop("cast")
                        .plain(" ")
                        .kw("as")
                        .plain(" ")
                        .ty(self.text(to.span())),
                ),
                vec![self.expr(x)],
            ),
            ExprKind::Fn { params, body, .. } => {
                // Parameter types are shown as checked, including ones the
                // source leaves out.
                let param_types = match &self.checked.types[e.id as usize] {
                    super::types::Type::Fn(ps, _) => ps.clone(),
                    _ => Vec::new(),
                };
                let mut children: Vec<Node> = params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let ty = param_types
                            .get(i)
                            .map_or_else(|| "?".to_owned(), |t| t.to_string());
                        Node::leaf(
                            prop("param")
                                .plain(" ")
                                .ident(&p.name.name)
                                .plain(": ")
                                .ty(ty),
                        )
                    })
                    .collect();
                children.push(self.block(prop("body"), body));
                Node::new(typed(kw("fn")), children)
            }
            ExprKind::Repeat(x, n) => Node::new(
                typed(prop("repeat").plain(" ").value(n.to_string())),
                vec![self.expr(x)],
            ),
            ExprKind::Invoke {
                step,
                id,
                target,
                args,
            } => {
                let word = if step.is_some() { "trigger" } else { "invoke" };
                let mut children = Vec::new();
                if let Some(step) = step {
                    children.push(Node::new(prop("step"), vec![self.expr(step)]));
                }
                if let Some(id) = id {
                    children.push(Node::new(prop("id"), vec![self.expr(id)]));
                }
                for a in args {
                    let label = match &a.name {
                        Some(n) => prop("arg").plain(" ").ident(&n.name),
                        None => prop("arg"),
                    };
                    children.push(Node::new(label, vec![self.expr(&a.value)]));
                }
                Node::new(typed(kw(word).plain(" ").callable(&target.name)), children)
            }
            ExprKind::Halt { id, target } => Node::new(
                typed(kw("halt").plain(" ").callable(&target.name)),
                id.iter()
                    .map(|id| Node::new(prop("id"), vec![self.expr(id)]))
                    .collect(),
            ),
        }
    }

    /// `seq riff(step: 1/8)` with one child per step.
    fn seq(&self, s: &SeqDecl) -> Node {
        let mut label = kw("seq").plain(" ").callable(&s.name.name);
        if !s.settings.is_empty() {
            label = label.plain("(");
            for (i, f) in s.settings.iter().enumerate() {
                if i > 0 {
                    label = label.plain(", ");
                }
                label = label
                    .ident(&f.name.name)
                    .plain(": ")
                    .value(self.text(f.value.span));
            }
            label = label.plain(")");
        }
        let steps = s
            .steps
            .iter()
            .map(|st| {
                let mut l = prop("step").plain(" ");
                l = match &st.notes {
                    Some(n) => l.value(self.text(n.span)),
                    None => l.plain("_"),
                };
                if let Some(v) = &st.velocity {
                    l = l.plain(" ").op("@").value(self.text(v.span));
                }
                Node::leaf(l)
            })
            .collect();
        Node::new(label, steps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
rill peak(x: Sample, release: Time = 300ms) Sample {
    state level: Sample = 0
    level = if abs(x) > level { abs(x) } else { level * decay(release) }
    return level
}
rill main() Sample {
    let y = [0.5, 1] |> peak(release: 10ms)
    return y[0]
}
";

    #[test]
    fn renders_a_typed_tree() {
        let (program, checked) = crate::lang::compile(SRC).unwrap();
        let expected = "\
rill peak Sample
├─ param x: Sample
├─ param release: Time
│  └─ 300ms : Time
└─ body
   ├─ state level: Sample
   │  └─ 0 : number
   ├─ assign level
   │  └─ if : Sample
   │     ├─ cond
   │     │  └─ binary > : Bool
   │     │     ├─ call abs : Sample
   │     │     │  └─ x : Sample
   │     │     └─ level : Sample
   │     ├─ then
   │     │  └─ call abs : Sample
   │     │     └─ x : Sample
   │     └─ else
   │        └─ binary * : Sample
   │           ├─ level : Sample
   │           └─ call decay : Sample
   │              └─ release : Time
   └─ return
      └─ level : Sample

rill main Sample
└─ body
   ├─ let y
   │  └─ call peak (piped) : [Sample; 2]
   │     ├─ frame : [number; 2]
   │     │  ├─ 0.5 : number
   │     │  └─ 1 : number
   │     └─ release: 10ms : Time
   └─ return
      └─ index : Sample
         ├─ y : [Sample; 2]
         └─ 0 : number
";
        assert_eq!(tree(SRC, &program, &checked, false), expected);
    }

    #[test]
    fn anonymous_fns_show_their_checked_types() {
        // `p` has no type in the source; the tree shows the one it was given.
        let src = "
rill main() Sample {
    let f: fn(Pitch) Freq = fn(p) { equal(p) }
    return (A4 |> f) / 1kHz
}
";
        let (program, checked) = crate::lang::compile(src).unwrap();
        let out = tree(src, &program, &checked, false);
        let expected = "\
   ├─ let f: fn(Pitch) Freq
   │  └─ fn : fn(Pitch) Freq
   │     ├─ param p: Pitch
   │     └─ body
   │        └─ call equal : Freq
   │           └─ p : Pitch
";
        assert!(out.contains(expected), "{out}");
    }
}
