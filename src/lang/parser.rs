//! Tokens to syntax tree.
//!
//! Grammar, lowest precedence first:
//!
//! ```text
//! program := item*
//! item    := "fn" def | "rill" def | event
//! event   := "event" NAME NAME ("(" (NAME ":" expr),* ")")?
//! seq     := "seq" NAME ("(" (NAME ":" expr),* ")")? "{" (step ","?)* "}"
//! step    := ("_" | expr) ("@" expr)?
//! def     := NAME ("<" NAME ("," NAME)* ">")? "(" params ")" type
//!            ("@" "rate" (("*" | "/") INT)?)? block
//! param   := NAME ":" type ("=" expr)?
//! type    := NAME | "[" type ";" (INT | NAME) "]" | "fn" "(" types ")" type
//! block   := "{" stmt* "}"
//! stmt    := "let" NAME (":" type)? ("=" expr)?
//!          | "state" NAME (":" type)? "=" expr
//!          | "return" expr
//!          | "for" NAME "in" expr block
//!          | assignable ("=" | "+=") expr
//!          | "on" NAME ("(" NAME,* ")")? ("claim" ("(" args ")")? | "release")? block
//!          | expr
//! expr    := range ("|>" NAME size_args? ("(" args ")")?)*
//! range   := or ((".." | "..=") or)?
//! or      := and ("||" and)*
//! and     := cmp ("&&" cmp)*
//! cmp     := sum (("<" | "<=" | ">" | ">=" | "==" | "!=") sum)?
//! sum     := term (("+" | "-") term)*
//! term    := cast (("*" | "/" | "%") cast)*
//! cast    := unary ("as" type)*
//! unary   := ("-" | "+" | "!") unary | postfix
//! postfix := primary ("[" expr "]")*
//! primary := NUMBER UNIT? | "true" | "false" | NAME ("(" args ")")?
//!          | "(" expr ")" | "[" expr ("," expr)* "]" | "[" expr ";" INT "]"
//!          | if | block | lambda | invoke
//! invoke  := "invoke" id? NAME ("(" args ")")?
//!          | "trigger" id id? NAME ("(" args ")")?
//!          | "halt" id? NAME
//! id      := NUMBER | NAME ("." NAME)*
//! lambda  := "fn" "(" (NAME (":" type)?),* ")" type? block
//! if      := "if" expr block ("else" (if | block))?
//! args    := (NAME ":")? expr ("," (NAME ":")? expr)*
//! ```
//!
//! Statements end at a line break or `;`. Outside brackets, a binary
//! operator at the start of a line begins a new statement rather than
//! continuing the previous one; the exception is `|>`, so pipelines can be
//! written one stage per line. Inside `(...)` and `[...]` line breaks are
//! ignored.

use super::ast::*;
use super::diag::{Diagnostic, Span};
use super::lexer::{Token, TokenKind};

pub fn parse(src: &str, tokens: Vec<Token>) -> Result<Program, Vec<Diagnostic>> {
    let (program, errors) = Parser::new(src, tokens, false).program();
    if errors.is_empty() {
        Ok(program)
    } else {
        Err(errors)
    }
}

/// Parse as much as possible, for tools such as editors. Besides skipping
/// to the next definition after an error, a statement that fails to parse
/// is skipped on its own, and a block left open at the end of the file (or
/// where the next definition starts) is closed there. The program always
/// comes back, along with every error found.
pub fn parse_partial(src: &str, tokens: Vec<Token>) -> (Program, Vec<Diagnostic>) {
    Parser::new(src, tokens, true).program()
}

type PResult<T> = Result<T, Diagnostic>;

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    /// Depth of enclosing `(` / `[`. Line breaks only end statements at 0.
    nest: u32,
    next_id: u32,
    errors: Vec<Diagnostic>,
    /// Recover inside blocks too; see [`parse_partial`].
    recover: bool,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, tokens: Vec<Token>, recover: bool) -> Parser<'a> {
        Parser {
            src,
            tokens,
            pos: 0,
            nest: 0,
            next_id: 0,
            errors: Vec::new(),
            recover,
        }
    }

    fn program(mut self) -> (Program, Vec<Diagnostic>) {
        let mut items = Vec::new();
        let mut events = Vec::new();
        let mut seqs = Vec::new();
        while !self.at(TokenKind::Eof) {
            if self.at_ident("seq") && self.peek_at(1).kind == TokenKind::Ident {
                match self.seq_decl() {
                    Ok(s) => seqs.push(s),
                    Err(err) => {
                        self.errors.push(err);
                        self.recover();
                    }
                }
                continue;
            }
            if self.at_ident("event") {
                match self.event_decl() {
                    Ok(e) => events.push(e),
                    Err(err) => {
                        self.errors.push(err);
                        self.recover();
                    }
                }
                continue;
            }
            match self.item() {
                Ok(item) => items.push(item),
                Err(err) => {
                    self.errors.push(err);
                    self.recover();
                }
            }
        }
        let program = Program {
            items,
            events,
            seqs,
            expr_count: self.next_id,
        };
        (program, self.errors)
    }
}

impl Parser<'_> {
    fn peek(&self) -> Token {
        self.tokens[self.pos]
    }

    fn peek_at(&self, ahead: usize) -> Token {
        self.tokens[(self.pos + ahead).min(self.tokens.len() - 1)]
    }

    fn at(&self, kind: TokenKind) -> bool {
        same_kind(&self.peek().kind, &kind)
    }

    fn bump(&mut self) -> Token {
        let t = self.peek();
        if t.kind != TokenKind::Eof {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, kind: TokenKind) -> Option<Token> {
        self.at(kind).then(|| self.bump())
    }

    fn expect(&mut self, kind: TokenKind, context: &str) -> PResult<Token> {
        match self.eat(kind) {
            Some(t) => Ok(t),
            None => Err(self.unexpected(&format!("{} {context}", kind.describe()))),
        }
    }

    fn unexpected(&self, expected: &str) -> Diagnostic {
        let t = self.peek();
        let found = match t.kind {
            TokenKind::Ident => format!("`{}`", self.text(t)),
            TokenKind::Number { .. } => format!("`{}`", self.text(t)),
            k => k.describe().to_owned(),
        };
        Diagnostic::error(t.span, format!("expected {expected}, found {found}"))
    }

    fn text(&self, t: Token) -> &str {
        &self.src[t.span.start as usize..t.span.end as usize]
    }

    /// Return types follow the parameters directly; `->` is a leftover
    /// from the old syntax.
    fn no_arrow(&mut self, example: &str) -> PResult<()> {
        match self.at(TokenKind::Arrow) {
            true => Err(Diagnostic::error(
                self.peek().span,
                "return types come right after the parameters, without `->`",
            )
            .with_help(format!("write it as `{example}`"))),
            false => Ok(()),
        }
    }

    fn ident(&mut self, context: &str) -> PResult<Ident> {
        let t = self.expect(TokenKind::Ident, context)?;
        Ok(Ident {
            name: self.text(t).to_owned(),
            span: t.span,
        })
    }

    fn prev_end(&self) -> u32 {
        self.tokens[self.pos.saturating_sub(1)].span.end
    }

    fn span_from(&self, start: Span) -> Span {
        Span {
            start: start.start,
            end: self.prev_end().max(start.end),
        }
    }

    fn expr_node(&mut self, kind: ExprKind, span: Span) -> Expr {
        let id = self.next_id;
        self.next_id += 1;
        Expr { id, kind, span }
    }

    /// Skip to the next definition so one mistake does not hide the rest.
    fn recover(&mut self) {
        self.bump();
        while !self.at(TokenKind::Eof)
            && !(self.peek().newline_before
                && (self.at(TokenKind::Fn)
                    || self.at(TokenKind::Rill)
                    || self.at_ident("event")
                    || self.at_ident("seq")))
        {
            self.bump();
        }
        self.nest = 0;
    }

    fn item(&mut self) -> PResult<Item> {
        if self.eat(TokenKind::Fn).is_some() {
            return Ok(Item::Fn(self.def("fn")?));
        }
        if self.eat(TokenKind::Rill).is_some() {
            return Ok(Item::Rill(self.def("rill")?));
        }
        Err(self
            .unexpected("`fn` or `rill`")
            .with_help("statements must be inside a rill; the program starts at `rill main`"))
    }

    fn def(&mut self, keyword: &str) -> PResult<Def> {
        let start = self.tokens[self.pos - 1].span;
        let name = self.ident(&format!("after `{keyword}`"))?;

        let mut generics = Vec::new();
        if self.eat(TokenKind::Lt).is_some() {
            loop {
                generics.push(self.ident("as a size parameter")?);
                if self.eat(TokenKind::Comma).is_none() || self.at(TokenKind::Gt) {
                    break;
                }
            }
            self.expect(TokenKind::Gt, "to close the size parameters")?;
        }

        self.expect(TokenKind::LParen, &format!("after the {keyword} name"))?;
        self.nest += 1;
        let mut params = Vec::new();
        while !self.at(TokenKind::RParen) {
            let name = self.ident("as a parameter name")?;
            self.expect(TokenKind::Colon, "and a type after the parameter name")?;
            let ty = self.ty()?;
            let default = match self.eat(TokenKind::Assign) {
                Some(_) => Some(self.expr()?),
                None => None,
            };
            params.push(Param { name, ty, default });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RParen, "to close the parameter list")?;
        self.nest -= 1;

        self.no_arrow(&format!("{keyword} {}(x: Sample) Sample", name.name))?;
        if self.at(TokenKind::LBrace) {
            return Err(self.unexpected("a return type").with_help(format!(
                "{keyword}s declare what they return after the parameters, as in `{keyword} {}(x: Sample) Sample`",
                name.name
            )));
        }
        let ret = self.ty()?;

        let rate = if let Some(at) = self.eat(TokenKind::At) {
            let word = self.ident("after `@`")?;
            if word.name != "rate" {
                return Err(Diagnostic::error(word.span, "expected `rate` after `@`")
                    .with_help("write the output rate as `@ rate / 2` or `@ rate * 2`"));
            }
            let (num, den) = if self.eat(TokenKind::Slash).is_some() {
                (1, self.int("as the rate divisor")?)
            } else if self.eat(TokenKind::Star).is_some() {
                (self.int("as the rate multiplier")?, 1)
            } else {
                (1, 1)
            };
            Some(RateSpec {
                num,
                den,
                span: self.span_from(at.span),
            })
        } else {
            None
        };

        let body = self.block()?;
        Ok(Def {
            name,
            generics,
            params,
            ret,
            rate,
            body,
            span: self.span_from(start),
        })
    }

    fn int(&mut self, context: &str) -> PResult<u32> {
        let t = self.peek();
        if let TokenKind::Number {
            value,
            unit: None,
            integral: true,
        } = t.kind
            && value >= 1.0
            && value <= f64::from(u32::MAX)
        {
            self.bump();
            return Ok(value as u32);
        }
        Err(self.unexpected(&format!("a positive whole number {context}")))
    }

    fn ty(&mut self) -> PResult<TypeExpr> {
        if let Some(start) = self.eat(TokenKind::Fn) {
            self.expect(TokenKind::LParen, "after `fn` in a function type")?;
            self.nest += 1;
            let mut params = Vec::new();
            while !self.at(TokenKind::RParen) {
                params.push(self.ty()?);
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            self.expect(TokenKind::RParen, "to close the parameter types")?;
            self.nest -= 1;
            self.no_arrow("fn(Pitch) Freq")?;
            let ret = self.ty()?;
            return Ok(TypeExpr::Fn {
                params,
                ret: Box::new(ret),
                span: self.span_from(start.span),
            });
        }
        if let Some(open) = self.eat(TokenKind::LBracket) {
            self.nest += 1;
            let elem = self.ty()?;
            self.expect(TokenKind::Semi, "and a channel count, as in `[Sample; 2]`")?;
            let size = if self.at(TokenKind::Ident) {
                SizeExpr::Var(self.ident("")?)
            } else {
                let at = self.peek().span;
                SizeExpr::Lit(self.int("as the channel count")?, at)
            };
            self.expect(TokenKind::RBracket, "to close the frame type")?;
            self.nest -= 1;
            return Ok(TypeExpr::Frame {
                elem: Box::new(elem),
                size,
                span: self.span_from(open.span),
            });
        }
        Ok(TypeExpr::Named(self.ident("as a type")?))
    }

    fn block(&mut self) -> PResult<Block> {
        let open = self.expect(TokenKind::LBrace, "to start a block")?;
        let saved = std::mem::replace(&mut self.nest, 0);
        let mut stmts = Vec::new();
        while !self.at(TokenKind::RBrace) {
            let unclosed = self.at(TokenKind::Eof)
                || (self.recover && self.peek().newline_before && self.at_def_start());
            if unclosed {
                let err = Diagnostic::error(open.span, "this `{` is never closed");
                if !self.recover {
                    return Err(err);
                }
                self.errors.push(err);
                self.nest = saved;
                return Ok(Block {
                    stmts,
                    span: self.span_from(open.span),
                });
            }
            let stmt_start = self.pos;
            match self.stmt() {
                Ok(stmt) => stmts.push(stmt),
                Err(err) if self.recover => {
                    self.errors.push(err);
                    self.skip_stmt(stmt_start);
                }
                Err(err) => return Err(err),
            }
        }
        self.bump();
        self.nest = saved;
        Ok(Block {
            stmts,
            span: self.span_from(open.span),
        })
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        let start = self.peek().span;
        if self.at_ident("on") && self.peek_at(1).kind == TokenKind::Ident {
            return self.event_handler();
        }
        let stmt = match self.peek().kind {
            TokenKind::Let | TokenKind::State => {
                let is_state = self.bump().kind == TokenKind::State;
                let name = self.ident("as the variable name")?;
                let ty = match self.eat(TokenKind::Colon) {
                    Some(_) => Some(self.ty()?),
                    None => None,
                };
                let value = match self.eat(TokenKind::Assign) {
                    Some(_) => Some(self.expr()?),
                    None if !is_state && ty.is_some() => None,
                    None => return Err(self.unexpected("and an initial value")),
                };
                let span = self.span_from(start);
                if is_state {
                    Stmt::State {
                        name,
                        ty,
                        init: value.expect("state always has an initial value"),
                        span,
                    }
                } else {
                    Stmt::Let {
                        name,
                        ty,
                        value,
                        span,
                    }
                }
            }
            TokenKind::Return => {
                self.bump();
                let value = self.expr()?;
                Stmt::Return {
                    value,
                    span: self.span_from(start),
                }
            }
            TokenKind::For => self.for_stmt()?,
            TokenKind::Ident if self.at_assignment(TokenKind::Assign) => {
                let target = self.assign_target()?;
                self.expect(TokenKind::Assign, "in assignment")?;
                let value = self.expr()?;
                Stmt::Assign {
                    target,
                    value,
                    span: self.span_from(start),
                }
            }
            TokenKind::Ident if self.at_assignment(TokenKind::PlusAssign) => {
                let target = self.assign_target()?;
                self.expect(TokenKind::PlusAssign, "in assignment")?;
                let rhs = self.expr()?;
                let lhs = self.assign_target_expr(&target);
                let span = lhs.span.to(rhs.span);
                let value = self.expr_node(
                    ExprKind::Binary(BinOp::Add, Box::new(lhs), Box::new(rhs)),
                    span,
                );
                Stmt::Assign {
                    target,
                    value,
                    span: self.span_from(start),
                }
            }
            _ => Stmt::Expr(self.expr()?),
        };

        if self.eat(TokenKind::Semi).is_none()
            && !self.at(TokenKind::RBrace)
            && !self.at(TokenKind::Eof)
            && !self.peek().newline_before
        {
            let err = self.unexpected("a line break or `;` after the statement");
            return Err(if self.at(TokenKind::Assign) {
                err.with_help("only plain names can be assigned to")
            } else {
                err
            });
        }
        Ok(stmt)
    }

    fn at_assignment(&self, op: TokenKind) -> bool {
        if same_kind(&self.peek_at(1).kind, &op) {
            return true;
        }
        self.peek_at(1).kind == TokenKind::LBracket
            && self
                .assign_op_after_index()
                .is_some_and(|k| same_kind(&k, &op))
    }

    fn assign_op_after_index(&self) -> Option<TokenKind> {
        let mut pos = self.pos + 1;
        if self.tokens.get(pos)?.kind != TokenKind::LBracket {
            return None;
        }
        let mut depth = 0u32;
        loop {
            let t = *self.tokens.get(pos)?;
            match t.kind {
                TokenKind::LBracket | TokenKind::LParen => depth += 1,
                TokenKind::RBracket | TokenKind::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let op = self.tokens.get(pos + 1)?.kind;
                        return matches!(op, TokenKind::Assign | TokenKind::PlusAssign)
                            .then_some(op);
                    }
                }
                TokenKind::Eof | TokenKind::RBrace => return None,
                _ => {}
            }
            pos += 1;
        }
    }

    fn assign_target(&mut self) -> PResult<AssignTarget> {
        let base = self.ident("")?;
        if self.eat(TokenKind::LBracket).is_none() {
            return Ok(AssignTarget::Name(base));
        }
        self.nest += 1;
        let index = self.expr()?;
        self.expect(TokenKind::RBracket, "to close the index")?;
        self.nest -= 1;
        let span = self.span_from(base.span);
        Ok(AssignTarget::Index { base, index, span })
    }

    fn assign_target_expr(&mut self, target: &AssignTarget) -> Expr {
        match target {
            AssignTarget::Name(id) => self.expr_node(ExprKind::Name(id.name.clone()), id.span),
            AssignTarget::Index { base, index, span } => {
                let base_expr = self.expr_node(ExprKind::Name(base.name.clone()), base.span);
                self.expr_node(
                    ExprKind::Index(Box::new(base_expr), Box::new(index.clone())),
                    *span,
                )
            }
        }
    }

    /// At `fn name` or `rill name`: a definition, not an anonymous fn.
    fn at_def_start(&self) -> bool {
        (self.at(TokenKind::Fn) || self.at(TokenKind::Rill))
            && self.peek_at(1).kind == TokenKind::Ident
    }

    /// After the statement starting at token `stmt_start` failed to parse:
    /// skip to where the next one starts (a line break or `;` outside
    /// brackets opened since), or to the `}` closing the block. A line
    /// starting with `let`, `state` or `return` always starts a statement,
    /// even inside brackets left open.
    fn skip_stmt(&mut self, stmt_start: usize) {
        self.nest = 0;
        let mut depth = 0u32;
        let mut first = true;
        loop {
            let t = self.peek();
            let starts_stmt = matches!(
                t.kind,
                TokenKind::Let | TokenKind::State | TokenKind::Return | TokenKind::For
            );
            if self.pos > stmt_start && t.newline_before && starts_stmt {
                return;
            }
            match t.kind {
                TokenKind::Eof => return,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket if depth == 0 => {
                    if t.kind == TokenKind::RBrace {
                        return;
                    }
                }
                _ if depth == 0 && !first && t.newline_before => return,
                _ if depth == 0 && !first && self.at_def_start() => return,
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => depth -= 1,
                TokenKind::Semi if depth == 0 => {
                    self.bump();
                    return;
                }
                _ => {}
            }
            self.bump();
            first = false;
        }
    }

    fn at_ident(&self, name: &str) -> bool {
        self.at(TokenKind::Ident) && self.text(self.peek()) == name
    }

    /// `(name: value, ...)`, as in event filters and sequence settings.
    fn settings(&mut self, what: &str) -> PResult<Vec<Filter>> {
        let mut out = Vec::new();
        if self.eat(TokenKind::LParen).is_some() {
            self.nest += 1;
            while !self.at(TokenKind::RParen) {
                let name = self.ident(&format!("as a {what} name"))?;
                self.expect(
                    TokenKind::Colon,
                    &format!("and a value after the {what} name"),
                )?;
                let value = self.expr()?;
                out.push(Filter { name, value });
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            self.expect(TokenKind::RParen, &format!("to close the {what}s"))?;
            self.nest -= 1;
        }
        Ok(out)
    }

    fn seq_decl(&mut self) -> PResult<SeqDecl> {
        let start = self.bump().span; // seq
        let name = self.ident("as the sequence's name")?;
        let settings = self.settings("setting")?;
        self.expect(TokenKind::LBrace, "and the steps of the sequence")?;
        // Steps may span lines.
        self.nest += 1;
        let mut steps = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            let at = self.peek().span;
            let notes = if self.at_ident("_") {
                self.bump();
                None
            } else {
                Some(self.expr()?)
            };
            let velocity = match self.eat(TokenKind::At) {
                Some(_) => Some(self.unary()?),
                None => None,
            };
            steps.push(Step {
                notes,
                velocity,
                span: self.span_from(at),
            });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RBrace, "or `,` between steps")?;
        self.nest -= 1;
        Ok(SeqDecl {
            name,
            settings,
            steps,
            span: self.span_from(start),
        })
    }

    fn event_decl(&mut self) -> PResult<EventDecl> {
        let start = self.bump().span; // event
        let name = self.ident("as the event's name")?;
        if self.at(TokenKind::LParen) || !self.at(TokenKind::Ident) {
            let kinds = "`note_on`, `note_off` or `control_change`";
            return Err(self
                .unexpected(&format!("the event's kind ({kinds}) after its name"))
                .with_help(format!(
                    "give it a name and a kind, as in `event keys {}(channel: 1)`",
                    if crate::event::EventKind::from_name(&name.name).is_some() {
                        name.name.as_str()
                    } else {
                        "note_on"
                    }
                )));
        }
        let kind = self.ident("")?;
        let filters = self.settings("filter")?;
        if self.eat(TokenKind::Semi).is_none()
            && !self.at(TokenKind::Eof)
            && !self.peek().newline_before
        {
            return Err(self.unexpected("a line break or `;` after the event declaration"));
        }
        Ok(EventDecl {
            name,
            kind,
            filters,
            span: self.span_from(start),
        })
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        let start = self.bump().span; // for
        let name = self.ident("after `for`")?;
        self.expect(TokenKind::In, "after the loop variable")?;
        let saved = std::mem::replace(&mut self.nest, 0);
        let iter = self.expr()?;
        self.nest = saved;
        let body = self.block()?;
        Ok(Stmt::For {
            name,
            iter,
            body,
            span: self.span_from(start),
        })
    }

    fn event_handler(&mut self) -> PResult<Stmt> {
        let start = self.bump().span; // on
        let name = self.ident("after `on`")?;
        let mut params = Vec::new();
        if self.eat(TokenKind::LParen).is_some() {
            self.nest += 1;
            while !self.at(TokenKind::RParen) {
                params.push(self.ident("as an event parameter")?);
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            self.expect(TokenKind::RParen, "to close the event parameter list")?;
            self.nest -= 1;
        }
        let mode = if self.at_ident("claim") {
            self.bump();
            let tail = if self.at(TokenKind::LParen) {
                let mut settings = self.settings("claim setting")?;
                match settings.len() {
                    0 => None,
                    1 if settings[0].name.name == "tail" => Some(settings.remove(0).value),
                    _ => {
                        // Something other than `tail`, or `tail` twice.
                        let bad = settings
                            .iter()
                            .find(|s| s.name.name != "tail")
                            .unwrap_or(&settings[settings.len() - 1]);
                        return Err(Diagnostic::error(
                            bad.name.span,
                            format!("`claim` takes only `tail`, not `{}`", bad.name.name),
                        )
                        .with_help("as in `claim(tail: 2s)`"));
                    }
                }
            } else {
                None
            };
            HandlerMode::Claim { tail }
        } else if self.at_ident("release") && self.peek_at(1).kind == TokenKind::LBrace {
            self.bump();
            HandlerMode::Release
        } else {
            HandlerMode::Plain
        };
        let body = self.block()?;
        Ok(Stmt::EventHandler {
            name,
            params,
            mode,
            body,
            span: self.span_from(start),
        })
    }

    /// True if the next token continues the current expression rather than
    /// starting a new statement on the next line.
    fn continues(&self) -> bool {
        let t = self.peek();
        !t.newline_before || self.nest > 0 || t.kind == TokenKind::Pipe
    }

    pub fn expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.range()?;
        while self.continues() && self.eat(TokenKind::Pipe).is_some() {
            let callee = self.ident("after `|>`").map_err(|e| {
                e.with_help("the right side of `|>` must name a fn or rill, as in `x |> peak`")
            })?;
            let sizes = if self.at_size_call_args() {
                self.size_args()?
            } else {
                Vec::new()
            };
            let mut args = vec![Arg {
                name: None,
                value: lhs,
            }];
            if self.at(TokenKind::LParen) && !self.peek().newline_before {
                args.extend(self.args()?);
            }
            let span = args[0].value.span.to(Span {
                start: callee.span.start,
                end: self.prev_end(),
            });
            lhs = self.expr_node(
                ExprKind::Call {
                    callee,
                    sizes,
                    args,
                    piped: true,
                },
                span,
            );
        }
        Ok(lhs)
    }

    fn range(&mut self) -> PResult<Expr> {
        let lhs = self.binary(0)?;
        if !self.continues() {
            return Ok(lhs);
        }
        let inclusive = if self.eat(TokenKind::DotDotEq).is_some() {
            true
        } else if self.eat(TokenKind::DotDot).is_some() {
            false
        } else {
            return Ok(lhs);
        };
        let rhs = self.binary(0)?;
        let span = lhs.span.to(rhs.span);
        Ok(self.expr_node(
            ExprKind::Range {
                start: Box::new(lhs),
                end: Box::new(rhs),
                inclusive,
            },
            span,
        ))
    }

    /// Precedence climbing over the binary operators below `|>`.
    fn binary(&mut self, min_level: u8) -> PResult<Expr> {
        let mut lhs = self.cast()?;
        loop {
            if !self.continues() {
                break;
            }
            let Some((op, level)) = binop(self.peek().kind) else {
                break;
            };
            if level < min_level {
                break;
            }
            self.bump();
            let rhs = self.binary(level + 1)?;
            if level == CMP_LEVEL
                && let Some((_, CMP_LEVEL)) = binop(self.peek().kind)
                && self.continues()
            {
                return Err(
                    Diagnostic::error(self.peek().span, "comparisons cannot be chained").with_help(
                        format!(
                            "split it up, as in `a {} b && b {} c`",
                            op.symbol(),
                            op.symbol()
                        ),
                    ),
                );
            }
            let span = lhs.span.to(rhs.span);
            lhs = self.expr_node(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span);
        }
        Ok(lhs)
    }

    /// `x as Float`. Binds tighter than the binary operators and looser
    /// than a leading `-`, so `-x as Int` is `(-x) as Int`.
    fn cast(&mut self) -> PResult<Expr> {
        let mut e = self.unary()?;
        while self.continues() && self.eat(TokenKind::As).is_some() {
            let ty = self.ty()?;
            let span = e.span.to(ty.span());
            e = self.expr_node(ExprKind::Cast(Box::new(e), ty), span);
        }
        Ok(e)
    }

    fn unary(&mut self) -> PResult<Expr> {
        let op = match self.peek().kind {
            TokenKind::Minus => UnOp::Neg,
            TokenKind::Plus => UnOp::Plus,
            TokenKind::Bang => UnOp::Not,
            _ => return self.postfix(),
        };
        let start = self.bump().span;
        let operand = self.unary()?;
        let span = start.to(operand.span);
        Ok(self.expr_node(ExprKind::Unary(op, Box::new(operand)), span))
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        while !self.peek().newline_before {
            if self.at(TokenKind::LBracket) {
                self.bump();
                self.nest += 1;
                let index = self.expr()?;
                self.expect(TokenKind::RBracket, "to close the index")?;
                self.nest -= 1;
                let span = self.span_from(e.span);
                e = self.expr_node(ExprKind::Index(Box::new(e), Box::new(index)), span);
            } else if self.eat(TokenKind::Dot).is_some() {
                let field = self.ident("after `.`")?;
                let span = e.span.to(field.span);
                e = self.expr_node(ExprKind::Field(Box::new(e), field), span);
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn args(&mut self) -> PResult<Vec<Arg>> {
        self.expect(TokenKind::LParen, "")?;
        self.nest += 1;
        let mut args = Vec::new();
        while !self.at(TokenKind::RParen) {
            let name = if self.at(TokenKind::Ident) && self.peek_at(1).kind == TokenKind::Colon {
                let name = self.ident("")?;
                self.bump();
                Some(name)
            } else {
                None
            };
            let value = self.expr()?;
            args.push(Arg { name, value });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RParen, "to close the argument list")?;
        self.nest -= 1;
        Ok(args)
    }

    fn size_args(&mut self) -> PResult<Vec<SizeExpr>> {
        if self.peek().newline_before || self.eat(TokenKind::Lt).is_none() {
            return Ok(Vec::new());
        }
        let mut sizes = Vec::new();
        loop {
            if self.at(TokenKind::Ident) {
                sizes.push(SizeExpr::Var(self.ident("as a size argument")?));
            } else {
                let at = self.peek().span;
                sizes.push(SizeExpr::Lit(self.int("as a size argument")?, at));
            }
            if self.eat(TokenKind::Comma).is_none() || self.at(TokenKind::Gt) {
                break;
            }
        }
        self.expect(TokenKind::Gt, "to close the size arguments")?;
        Ok(sizes)
    }

    fn at_size_call_args(&self) -> bool {
        if self.peek().newline_before || self.peek().kind != TokenKind::Lt {
            return false;
        }
        let mut pos = self.pos + 1;
        loop {
            match self.tokens.get(pos).map(|t| t.kind) {
                Some(TokenKind::Ident | TokenKind::Number { .. }) => pos += 1,
                _ => return false,
            }
            match self.tokens.get(pos).map(|t| t.kind) {
                Some(TokenKind::Comma) => pos += 1,
                Some(TokenKind::Gt) => {
                    let Some(next) = self.tokens.get(pos + 1) else {
                        return false;
                    };
                    return !next.newline_before && next.kind == TokenKind::LParen;
                }
                _ => return false,
            }
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.peek();
        match t.kind {
            TokenKind::Number {
                value,
                unit,
                integral,
            } => {
                self.bump();
                Ok(self.expr_node(
                    ExprKind::Number {
                        value: unit.map_or(value, |u| u.to_base(value)),
                        unit,
                        integral,
                    },
                    t.span,
                ))
            }
            TokenKind::True | TokenKind::False => {
                self.bump();
                Ok(self.expr_node(ExprKind::Bool(t.kind == TokenKind::True), t.span))
            }
            TokenKind::Ident if self.at_invoke() => self.invoke(),
            TokenKind::Ident => {
                let name = self.ident("")?;
                let sizes = if self.at_size_call_args() {
                    self.size_args()?
                } else {
                    Vec::new()
                };
                if self.at(TokenKind::LParen) && !self.peek().newline_before {
                    let args = self.args()?;
                    let span = self.span_from(name.span);
                    Ok(self.expr_node(
                        ExprKind::Call {
                            callee: name,
                            sizes,
                            args,
                            piped: false,
                        },
                        span,
                    ))
                } else {
                    if !sizes.is_empty() {
                        return Err(self
                            .unexpected("`(` after explicit size arguments")
                            .with_help("write explicit sizes on a call, as in `unison<8>(...)`"));
                    }
                    Ok(self.expr_node(ExprKind::Name(name.name), name.span))
                }
            }
            TokenKind::LParen => {
                self.bump();
                self.nest += 1;
                let mut e = self.expr()?;
                self.expect(TokenKind::RParen, "to close the parenthesis")?;
                self.nest -= 1;
                e.span = self.span_from(t.span);
                Ok(e)
            }
            TokenKind::LBracket => {
                self.bump();
                self.nest += 1;
                let mut elems = Vec::new();
                while !self.at(TokenKind::RBracket) {
                    elems.push(self.expr()?);
                    if elems.len() == 1 && self.eat(TokenKind::Semi).is_some() {
                        let count = self.int("as the number of copies")?;
                        self.expect(TokenKind::RBracket, "to close the frame")?;
                        self.nest -= 1;
                        let span = self.span_from(t.span);
                        let e = elems.pop().expect("one element");
                        return Ok(self.expr_node(ExprKind::Repeat(Box::new(e), count), span));
                    }
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RBracket, "or `,` in the frame")?;
                self.nest -= 1;
                let span = self.span_from(t.span);
                if elems.is_empty() {
                    return Err(Diagnostic::error(
                        span,
                        "a frame needs at least one channel",
                    ));
                }
                Ok(self.expr_node(ExprKind::Frame(elems), span))
            }
            TokenKind::If => self.if_expr(),
            TokenKind::Fn => self.lambda(),
            TokenKind::LBrace => {
                let block = self.block()?;
                let span = block.span;
                Ok(self.expr_node(ExprKind::Block(block), span))
            }
            _ => Err(self.unexpected("an expression")),
        }
    }

    /// At `invoke`, `trigger` or `halt` used as a keyword: followed by what
    /// it acts on rather than by an operator or `(`.
    fn at_invoke(&self) -> bool {
        let next = self.peek_at(1);
        (self.at_ident("invoke") || self.at_ident("trigger") || self.at_ident("halt"))
            && !next.newline_before
            && matches!(next.kind, TokenKind::Ident | TokenKind::Number { .. })
    }

    /// An instance id or a step: a number, or a name with optional fields.
    fn id_expr(&mut self) -> PResult<Expr> {
        if !self.at(TokenKind::Ident) {
            return self.primary();
        }
        let name = self.ident("")?;
        let mut e = self.expr_node(ExprKind::Name(name.name), name.span);
        while self.eat(TokenKind::Dot).is_some() {
            let field = self.ident("after `.`")?;
            let span = Span {
                start: e.span.start,
                end: field.span.end,
            };
            e = self.expr_node(ExprKind::Field(Box::new(e), field), span);
        }
        Ok(e)
    }

    /// An id comes before the target when another name, a number or a
    /// field access follows it.
    fn at_id(&self) -> bool {
        match self.peek().kind {
            TokenKind::Number { .. } => true,
            TokenKind::Ident => {
                let next = self.peek_at(1);
                !next.newline_before && matches!(next.kind, TokenKind::Ident | TokenKind::Dot)
            }
            _ => false,
        }
    }

    fn invoke(&mut self) -> PResult<Expr> {
        let start = self.peek().span;
        let keyword = self.ident("")?;
        let step = match keyword.name.as_str() {
            "trigger" => Some(Box::new(self.id_expr()?)),
            _ => None,
        };
        let id = match self.at_id() {
            true => Some(Box::new(self.id_expr()?)),
            false => None,
        };
        let target = self.ident(&format!(
            "as the sequence or event after `{}`",
            keyword.name
        ))?;
        if keyword.name == "halt" {
            let span = self.span_from(start);
            return Ok(self.expr_node(ExprKind::Halt { id, target }, span));
        }
        let args = if self.at(TokenKind::LParen) && !self.peek().newline_before {
            self.args()?
        } else {
            Vec::new()
        };
        let span = self.span_from(start);
        Ok(self.expr_node(
            ExprKind::Invoke {
                step,
                id,
                target,
                args,
            },
            span,
        ))
    }

    /// `fn(p) { ... }`: a fn without a name.
    fn lambda(&mut self) -> PResult<Expr> {
        let start = self.bump().span;
        if self.at(TokenKind::Ident) {
            let name = self.ident("")?;
            return Err(Diagnostic::error(
                name.span,
                "a fn with a name can only be defined at the top level",
            )
            .with_help(format!(
                "leave the name out for an anonymous fn, as in `let {} = fn(x) {{ ... }}`",
                name.name
            )));
        }
        self.expect(TokenKind::LParen, "after `fn`")?;
        self.nest += 1;
        let mut params = Vec::new();
        while !self.at(TokenKind::RParen) {
            let name = self.ident("as a parameter name")?;
            let ty = match self.eat(TokenKind::Colon) {
                Some(_) => Some(self.ty()?),
                None => None,
            };
            params.push(FnParam { name, ty });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RParen, "to close the parameter list")?;
        self.nest -= 1;
        self.no_arrow("fn(p: Pitch) Freq { ... }")?;
        let ret = match self.at(TokenKind::LBrace) {
            true => None,
            false => Some(self.ty()?),
        };
        let body = self.block()?;
        let span = self.span_from(start);
        Ok(self.expr_node(ExprKind::Fn { params, ret, body }, span))
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let start = self.expect(TokenKind::If, "")?.span;
        // The condition is not inside brackets even if the `if` is.
        let saved = std::mem::replace(&mut self.nest, 0);
        let cond = self.expr()?;
        self.nest = saved;
        let then = self.block()?;
        let els = if self.eat(TokenKind::Else).is_some() {
            if self.at(TokenKind::If) {
                Some(Box::new(self.if_expr()?))
            } else {
                let block = self.block()?;
                let span = block.span;
                Some(Box::new(self.expr_node(ExprKind::Block(block), span)))
            }
        } else {
            None
        };
        let span = self.span_from(start);
        Ok(self.expr_node(
            ExprKind::If {
                cond: Box::new(cond),
                then,
                els,
            },
            span,
        ))
    }
}

const CMP_LEVEL: u8 = 2;

fn binop(kind: TokenKind) -> Option<(BinOp, u8)> {
    use TokenKind as T;
    Some(match kind {
        T::OrOr => (BinOp::Or, 0),
        T::AndAnd => (BinOp::And, 1),
        T::Lt => (BinOp::Lt, CMP_LEVEL),
        T::Le => (BinOp::Le, CMP_LEVEL),
        T::Gt => (BinOp::Gt, CMP_LEVEL),
        T::Ge => (BinOp::Ge, CMP_LEVEL),
        T::EqEq => (BinOp::Eq, CMP_LEVEL),
        T::Ne => (BinOp::Ne, CMP_LEVEL),
        T::Plus => (BinOp::Add, 3),
        T::Minus => (BinOp::Sub, 3),
        T::Star => (BinOp::Mul, 4),
        T::Slash => (BinOp::Div, 4),
        T::Percent => (BinOp::Rem, 4),
        _ => return None,
    })
}

fn same_kind(a: &TokenKind, b: &TokenKind) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}
