//! Tokens to syntax tree.
//!
//! Grammar, lowest precedence first:
//!
//! ```text
//! program := item*
//! item    := "fn" def | "rill" def
//! def     := NAME ("<" NAME ("," NAME)* ">")? "(" params ")" "->" type
//!            ("@" "rate" (("*" | "/") INT)?)? block
//! param   := NAME ":" type ("=" expr)?
//! type    := NAME | "[" type ";" (INT | NAME) "]" | "fn" "(" types ")" "->" type
//! block   := "{" stmt* "}"
//! stmt    := "let" NAME (":" type)? "=" expr
//!          | "state" NAME (":" type)? "=" expr
//!          | "return" expr
//!          | NAME "=" expr
//!          | expr
//! expr    := or ("|>" NAME ("(" args ")")?)*
//! or      := and ("||" and)*
//! and     := cmp ("&&" cmp)*
//! cmp     := sum (("<" | "<=" | ">" | ">=" | "==" | "!=") sum)?
//! sum     := term (("+" | "-") term)*
//! term    := cast (("*" | "/" | "%") cast)*
//! cast    := unary ("as" type)*
//! unary   := ("-" | "+" | "!") unary | postfix
//! postfix := primary ("[" expr "]")*
//! primary := NUMBER UNIT? | "true" | "false" | NAME ("(" args ")")?
//!          | "(" expr ")" | "[" expr ("," expr)* "]" | if | block | lambda
//! lambda  := "fn" "(" (NAME (":" type)?),* ")" ("->" type)? block
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
        while !self.at(TokenKind::Eof) {
            match self.item() {
                Ok(Some(item)) => items.push(item),
                Ok(None) => {}
                Err(err) => {
                    self.errors.push(err);
                    self.recover();
                }
            }
        }
        let program = Program {
            items,
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
            && !(self.peek().newline_before && (self.at(TokenKind::Fn) || self.at(TokenKind::Rill)))
        {
            self.bump();
        }
        self.nest = 0;
    }

    fn item(&mut self) -> PResult<Option<Item>> {
        if self.eat(TokenKind::Fn).is_some() {
            return Ok(Some(Item::Fn(self.def("fn")?)));
        }
        if self.eat(TokenKind::Rill).is_some() {
            return Ok(Some(Item::Rill(self.def("rill")?)));
        }
        if self.at_ident("event") {
            self.skip_event_decl()?;
            return Ok(None);
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

        self.expect(TokenKind::Arrow, "and a return type")
            .map_err(|e| {
                e.with_help(format!(
                    "{keyword}s declare what they return, e.g. `-> Sample`"
                ))
            })?;
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
            self.expect(
                TokenKind::Arrow,
                "and a return type, as in `fn(Pitch) -> Freq`",
            )?;
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
            return self.skip_event_handler();
        }
        let stmt = match self.peek().kind {
            TokenKind::Let | TokenKind::State => {
                let is_state = self.bump().kind == TokenKind::State;
                let name = self.ident("as the variable name")?;
                let ty = match self.eat(TokenKind::Colon) {
                    Some(_) => Some(self.ty()?),
                    None => None,
                };
                self.expect(TokenKind::Assign, "and an initial value")?;
                let value = self.expr()?;
                let span = self.span_from(start);
                if is_state {
                    Stmt::State {
                        name,
                        ty,
                        init: value,
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
            TokenKind::Ident if self.peek_at(1).kind == TokenKind::Assign => {
                let target = self.ident("")?;
                self.bump();
                let value = self.expr()?;
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
                TokenKind::Let | TokenKind::State | TokenKind::Return
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

    fn skip_event_decl(&mut self) -> PResult<()> {
        self.bump(); // event
        self.ident("after `event`")?;
        if self.eat(TokenKind::LParen).is_some() {
            self.skip_balanced(TokenKind::LParen, TokenKind::RParen)?;
        }
        if self.eat(TokenKind::Semi).is_none()
            && !self.at(TokenKind::Eof)
            && !self.peek().newline_before
        {
            return Err(self.unexpected("a line break or `;` after the event declaration"));
        }
        Ok(())
    }

    fn skip_event_handler(&mut self) -> PResult<Stmt> {
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
        let body = self.block()?;
        Ok(Stmt::EventHandler {
            name,
            params,
            body,
            span: self.span_from(start),
        })
    }

    fn skip_balanced(&mut self, open: TokenKind, close: TokenKind) -> PResult<()> {
        let mut depth = 1u32;
        while depth > 0 {
            let t = self.bump();
            if same_kind(&t.kind, &TokenKind::Eof) {
                return Err(Diagnostic::error(
                    t.span,
                    format!("this {} is never closed", open.describe()),
                ));
            }
            if same_kind(&t.kind, &open) {
                depth += 1;
            } else if same_kind(&t.kind, &close) {
                depth -= 1;
            }
        }
        Ok(())
    }

    /// True if the next token continues the current expression rather than
    /// starting a new statement on the next line.
    fn continues(&self) -> bool {
        let t = self.peek();
        !t.newline_before || self.nest > 0 || t.kind == TokenKind::Pipe
    }

    pub fn expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.binary(0)?;
        while self.continues() && self.eat(TokenKind::Pipe).is_some() {
            let callee = self.ident("after `|>`").map_err(|e| {
                e.with_help("the right side of `|>` must name a fn or rill, as in `x |> peak`")
            })?;
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
                    args,
                    piped: true,
                },
                span,
            );
        }
        Ok(lhs)
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
            TokenKind::Ident => {
                let name = self.ident("")?;
                if self.at(TokenKind::LParen) && !self.peek().newline_before {
                    let args = self.args()?;
                    let span = self.span_from(name.span);
                    Ok(self.expr_node(
                        ExprKind::Call {
                            callee: name,
                            args,
                            piped: false,
                        },
                        span,
                    ))
                } else {
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
        let ret = match self.eat(TokenKind::Arrow) {
            Some(_) => Some(self.ty()?),
            None => None,
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
