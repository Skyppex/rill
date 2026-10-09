//! Syntax tree produced by the parser.

use super::diag::Span;
use super::lexer::Unit;

#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub items: Vec<Item>,
    /// Event declarations, in source order.
    pub events: Vec<EventDecl>,
    /// Sequences, in source order.
    pub seqs: Vec<SeqDecl>,
    /// Number of expressions; every [`Expr::id`] is below this.
    pub expr_count: u32,
}

/// A top-level definition. A program has no statements outside them; it
/// runs by instantiating its entry rill (`main` by default).
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Fn(Def),
    Rill(Def),
}

impl Item {
    pub fn def(&self) -> &Def {
        match self {
            Item::Fn(d) | Item::Rill(d) => d,
        }
    }
}

/// `event keys note_on(sender: 5, channel: 1)`: a name, a kind and
/// optional filters. The kind is checked by the checker.
#[derive(Clone, Debug, PartialEq)]
pub struct EventDecl {
    pub name: Ident,
    pub kind: Ident,
    pub filters: Vec<Filter>,
    pub span: Span,
}

/// `sender: 5` in an event declaration, or `tempo: 120bpm` in a sequence's
/// settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub name: Ident,
    pub value: Expr,
}

/// `seq riff(step: 1/8) { C4, _, E4@0.5, [G4, B4] }`
#[derive(Clone, Debug, PartialEq)]
pub struct SeqDecl {
    pub name: Ident,
    pub settings: Vec<Filter>,
    pub steps: Vec<Step>,
    pub span: Span,
}

/// One step of a sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// `None` for a rest (`_`); otherwise a pitch or a chord of pitches.
    pub notes: Option<Expr>,
    /// `@0.5`
    pub velocity: Option<Expr>,
    pub span: Span,
}

/// How an `on` handler shares events out over the copies of a voice pool.
#[derive(Clone, Debug, PartialEq)]
pub enum HandlerMode {
    /// Runs in every copy.
    Plain,
    /// `claim` or `claim(tail: 2s)`: runs in one copy, which holds the note.
    Claim { tail: Option<Expr> },
    /// `release`: runs in the copy holding the note.
    Release,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// A `fn` or `rill` definition. Which one is recorded by the [`Item`].
#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub name: Ident,
    /// Compile-time size parameters, as in `mix_down<N>`.
    pub generics: Vec<Ident>,
    pub params: Vec<Param>,
    pub ret: TypeExpr,
    /// `@ rate / 2`. Only meaningful on rills.
    pub rate: Option<RateSpec>,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: Ident,
    pub ty: TypeExpr,
    pub default: Option<Expr>,
}

/// Output rate relative to the input rate: `rate * num / den`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateSpec {
    pub num: u32,
    pub den: u32,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    Named(Ident),
    /// `[elem; size]`
    Frame {
        elem: Box<TypeExpr>,
        size: SizeExpr,
        span: Span,
    },
    /// `fn(params) ret`
    Fn {
        params: Vec<TypeExpr>,
        ret: Box<TypeExpr>,
        span: Span,
    },
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named(id) => id.span,
            TypeExpr::Frame { span, .. } | TypeExpr::Fn { span, .. } => *span,
        }
    }
}

/// A parameter of an anonymous fn. The type may be left out when the
/// surroundings say what it is.
#[derive(Clone, Debug, PartialEq)]
pub struct FnParam {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SizeExpr {
    Lit(u32, Span),
    Var(Ident),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Let {
        name: Ident,
        ty: Option<TypeExpr>,
        value: Option<Expr>,
        span: Span,
    },
    State {
        name: Ident,
        ty: Option<TypeExpr>,
        init: Expr,
        span: Span,
    },
    Assign {
        target: AssignTarget,
        value: Expr,
        span: Span,
    },
    Return {
        value: Expr,
        span: Span,
    },
    EventHandler {
        name: Ident,
        params: Vec<Ident>,
        mode: HandlerMode,
        body: Block,
        span: Span,
    },
    For {
        name: Ident,
        iter: Expr,
        body: Block,
        span: Span,
    },
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AssignTarget {
    Name(Ident),
    Index {
        base: Ident,
        index: Expr,
        span: Span,
    },
}

impl AssignTarget {
    pub fn span(&self) -> Span {
        match self {
            AssignTarget::Name(id) => id.span,
            AssignTarget::Index { span, .. } => *span,
        }
    }

    pub fn name(&self) -> &Ident {
        match self {
            AssignTarget::Name(id) => id,
            AssignTarget::Index { base, .. } => base,
        }
    }
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let { span, .. }
            | Stmt::State { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::EventHandler { span, .. }
            | Stmt::For { span, .. } => *span,
            Stmt::Expr(e) => e.span,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    /// Dense index, unique within a [`Program`]. The checker records each
    /// expression's type under it.
    pub id: u32,
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Number {
        value: f64,
        unit: Option<Unit>,
        integral: bool,
    },
    Bool(bool),
    Name(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
    },
    /// `f(a, b: c)`. `x |> f(a)` is sugar for `f(x, a)` and parses to this
    /// with `piped` set.
    Call {
        callee: Ident,
        sizes: Vec<SizeExpr>,
        args: Vec<Arg>,
        piped: bool,
    },
    If {
        cond: Box<Expr>,
        then: Block,
        /// Either a [`ExprKind::Block`] or, for `else if`, an [`ExprKind::If`].
        els: Option<Box<Expr>>,
    },
    Block(Block),
    /// `[a, b]`
    Frame(Vec<Expr>),
    /// `[synth(); 8]`: the expression evaluated that many times.
    Repeat(Box<Expr>, u32),
    Index(Box<Expr>, Box<Expr>),
    Field(Box<Expr>, Ident),
    /// `x as Float`
    Cast(Box<Expr>, TypeExpr),
    /// An anonymous fn: `fn(p) { ... }` or `fn(p: Pitch) Freq { ... }`.
    Fn {
        params: Vec<FnParam>,
        ret: Option<TypeExpr>,
        body: Block,
    },
    /// `invoke riff(tempo: 90bpm)`, `invoke id riff`, `trigger 3 id riff`,
    /// or `invoke keys(pitch: C4)` for a declared event. `step` is set for
    /// `trigger`.
    Invoke {
        step: Option<Box<Expr>>,
        id: Option<Box<Expr>>,
        target: Ident,
        args: Vec<Arg>,
    },
    /// `halt riff` or `halt id riff`
    Halt {
        id: Option<Box<Expr>>,
        target: Ident,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub name: Option<Ident>,
    /// The span of `each` before the value: evaluate it once per copy of a
    /// call that runs per element.
    pub each: Option<Span>,
    pub value: Expr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Plus,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}
