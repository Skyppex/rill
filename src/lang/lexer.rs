//! Source text to tokens.

use super::diag::{Diagnostic, Span};

/// Unit suffix on a number literal, as in `440Hz` or `300ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Hz,
    KHz,
    /// Beats per minute: a tempo is a frequency, so `120bpm` is `2Hz`.
    Bpm,
    Ms,
    S,
    /// Semitones.
    St,
    Cents,
    /// Decibels, for a `Gain`.
    Db,
}

/// What a unit measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dimension {
    Frequency,
    Time,
    Interval,
    Level,
}

impl Unit {
    pub const ALL: [(&'static str, Unit); 8] = [
        ("Hz", Unit::Hz),
        ("kHz", Unit::KHz),
        ("bpm", Unit::Bpm),
        ("ms", Unit::Ms),
        ("s", Unit::S),
        ("st", Unit::St),
        ("cents", Unit::Cents),
        ("dB", Unit::Db),
    ];

    pub fn dimension(self) -> Dimension {
        match self {
            Unit::Hz | Unit::KHz | Unit::Bpm => Dimension::Frequency,
            Unit::Ms | Unit::S => Dimension::Time,
            Unit::St | Unit::Cents => Dimension::Interval,
            Unit::Db => Dimension::Level,
        }
    }

    /// Convert to the base unit of the dimension: Hz, seconds, semitones, or
    /// for levels the amplitude factor (`-6dB` is about 0.5).
    pub fn to_base(self, value: f64) -> f64 {
        match self {
            Unit::Hz | Unit::S | Unit::St => value,
            Unit::KHz => value * 1000.0,
            Unit::Bpm => value / 60.0,
            Unit::Ms => value / 1000.0,
            Unit::Cents => value / 100.0,
            Unit::Db => 10f64.powf(value / 20.0),
        }
    }

    pub fn name(self) -> &'static str {
        Unit::ALL.iter().find(|(_, u)| *u == self).unwrap().0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TokenKind {
    Ident,
    Number {
        value: f64,
        unit: Option<Unit>,
        /// Written without a fraction or exponent.
        integral: bool,
    },
    // Keywords.
    Fn,
    Rill,
    State,
    Let,
    Const,
    For,
    In,
    Return,
    If,
    Else,
    As,
    True,
    False,
    // Punctuation.
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Semi,
    Dot,
    DotDot,
    DotDotEq,
    At,
    Arrow,
    Pipe,
    Plus,
    PlusAssign,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Assign,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Eof,
}

impl TokenKind {
    /// How the token is written, for "expected X" messages.
    pub fn describe(&self) -> &'static str {
        use TokenKind::*;
        match self {
            Ident => "a name",
            Number { .. } => "a number",
            Fn => "`fn`",
            Rill => "`rill`",
            State => "`state`",
            Let => "`let`",
            Const => "`const`",
            For => "`for`",
            In => "`in`",
            Return => "`return`",
            If => "`if`",
            Else => "`else`",
            As => "`as`",
            True => "`true`",
            False => "`false`",
            LParen => "`(`",
            RParen => "`)`",
            LBrace => "`{`",
            RBrace => "`}`",
            LBracket => "`[`",
            RBracket => "`]`",
            Comma => "`,`",
            Colon => "`:`",
            Semi => "`;`",
            Dot => "`.`",
            DotDot => "`..`",
            DotDotEq => "`..=`",
            At => "`@`",
            Arrow => "`->`",
            Pipe => "`|>`",
            Plus => "`+`",
            PlusAssign => "`+=`",
            Minus => "`-`",
            Star => "`*`",
            Slash => "`/`",
            Percent => "`%`",
            Bang => "`!`",
            Assign => "`=`",
            EqEq => "`==`",
            Ne => "`!=`",
            Lt => "`<`",
            Le => "`<=`",
            Gt => "`>`",
            Ge => "`>=`",
            AndAnd => "`&&`",
            OrOr => "`||`",
            Eof => "end of file",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// A line break separates this token from the previous one. The parser
    /// uses this to end statements without semicolons.
    pub newline_before: bool,
}

pub fn lex(src: &str) -> Result<Vec<Token>, Diagnostic> {
    Lexer::new(src, false).run()
}

/// Lex all of `src`, reporting every error instead of stopping at the
/// first. A bad character is skipped and a bad unit dropped, so the tokens
/// are always usable. For tools such as editors, which need something to
/// work with while the text is broken.
pub fn lex_partial(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lexer = Lexer::new(src, true);
    let tokens = lexer.run_inner().expect("a recovering lexer does not fail");
    (tokens, lexer.errors)
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    newline: bool,
    tokens: Vec<Token>,
    /// Keep going after an error, collecting it in `errors`.
    recover: bool,
    errors: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, recover: bool) -> Lexer<'a> {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            newline: false,
            tokens: Vec::new(),
            recover,
            errors: Vec::new(),
        }
    }

    /// Report `d`: an error when stopping at the first problem, otherwise
    /// recorded so lexing can go on.
    fn fail(&mut self, d: Diagnostic) -> Result<(), Diagnostic> {
        if self.recover {
            self.errors.push(d);
            Ok(())
        } else {
            Err(d)
        }
    }

    fn peek(&self, ahead: usize) -> u8 {
        self.bytes.get(self.pos + ahead).copied().unwrap_or(0)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
            newline_before: self.newline,
        });
        self.newline = false;
    }

    fn run(mut self) -> Result<Vec<Token>, Diagnostic> {
        self.run_inner()
    }

    fn run_inner(&mut self) -> Result<Vec<Token>, Diagnostic> {
        loop {
            self.skip_trivia()?;
            let start = self.pos;
            let c = self.peek(0);
            if c == 0 && self.pos >= self.bytes.len() {
                self.push(TokenKind::Eof, start);
                return Ok(std::mem::take(&mut self.tokens));
            }
            if c.is_ascii_digit() {
                let kind = self.number()?;
                self.push(kind, start);
                continue;
            }
            if c.is_ascii_alphabetic() || c == b'_' {
                // `#` is a sharp, so it is only part of a name right after a
                // note letter, as in `F#4`.
                while self.peek(0).is_ascii_alphanumeric()
                    || self.peek(0) == b'_'
                    || (self.peek(0) == b'#' && self.pos == start + 1 && matches!(c, b'A'..=b'G'))
                {
                    self.pos += 1;
                }
                let kind = match &self.src[start..self.pos] {
                    "fn" => TokenKind::Fn,
                    "rill" => TokenKind::Rill,
                    "state" => TokenKind::State,
                    "let" => TokenKind::Let,
                    "const" => TokenKind::Const,
                    "for" => TokenKind::For,
                    "in" => TokenKind::In,
                    "return" => TokenKind::Return,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "as" => TokenKind::As,
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    _ => TokenKind::Ident,
                };
                self.push(kind, start);
                continue;
            }

            use TokenKind::*;
            let two = [c, self.peek(1)];
            let (kind, len) = match &two {
                b"->" => (Arrow, 2),
                b"|>" => (Pipe, 2),
                b"+=" => (PlusAssign, 2),
                b".." if self.peek(2) == b'=' => (DotDotEq, 3),
                b".." => (DotDot, 2),
                b"==" => (EqEq, 2),
                b"!=" => (Ne, 2),
                b"<=" => (Le, 2),
                b">=" => (Ge, 2),
                b"&&" => (AndAnd, 2),
                b"||" => (OrOr, 2),
                _ => match c {
                    b'(' => (LParen, 1),
                    b')' => (RParen, 1),
                    b'{' => (LBrace, 1),
                    b'}' => (RBrace, 1),
                    b'[' => (LBracket, 1),
                    b']' => (RBracket, 1),
                    b',' => (Comma, 1),
                    b':' => (Colon, 1),
                    b';' => (Semi, 1),
                    b'.' => (Dot, 1),
                    b'@' => (At, 1),
                    b'+' => (Plus, 1),
                    b'-' => (Minus, 1),
                    b'*' => (Star, 1),
                    b'/' => (Slash, 1),
                    b'%' => (Percent, 1),
                    b'!' => (Bang, 1),
                    b'=' => (Assign, 1),
                    b'<' => (Lt, 1),
                    b'>' => (Gt, 1),
                    _ => {
                        let ch = self.src[start..].chars().next().unwrap();
                        let span = Span::new(start, start + ch.len_utf8());
                        let err = Diagnostic::error(span, format!("unexpected character `{ch}`"));
                        self.fail(if ch == '|' {
                            err.with_help("use `|>` to pipe or `||` for logical or")
                        } else if ch == '&' {
                            err.with_help("use `&&` for logical and")
                        } else if ch == '#' {
                            err.with_help("`#` only appears in note names, like `F#4`")
                        } else {
                            err
                        })?;
                        self.pos = start + ch.len_utf8();
                        continue;
                    }
                },
            };
            self.pos += len;
            self.push(kind, start);
        }
    }

    fn skip_trivia(&mut self) -> Result<(), Diagnostic> {
        loop {
            match (self.peek(0), self.peek(1)) {
                (b'\n', _) => {
                    self.newline = true;
                    self.pos += 1;
                }
                (b' ' | b'\t' | b'\r', _) => self.pos += 1,
                (b'/', b'/') => {
                    while self.pos < self.bytes.len() && self.peek(0) != b'\n' {
                        self.pos += 1;
                    }
                }
                (b'/', b'*') => {
                    let start = self.pos;
                    self.pos += 2;
                    let mut depth = 1;
                    while depth > 0 {
                        match (self.peek(0), self.peek(1)) {
                            (0, _) if self.pos >= self.bytes.len() => {
                                self.fail(Diagnostic::error(
                                    Span::new(start, start + 2),
                                    "unterminated block comment",
                                ))?;
                                return Ok(());
                            }
                            (b'/', b'*') => {
                                depth += 1;
                                self.pos += 2;
                            }
                            (b'*', b'/') => {
                                depth -= 1;
                                self.pos += 2;
                            }
                            (b'\n', _) => {
                                self.newline = true;
                                self.pos += 1;
                            }
                            _ => self.pos += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// `48_000`, `0.5`, `1e-3`, each optionally followed by a unit.
    fn number(&mut self) -> Result<TokenKind, Diagnostic> {
        let start = self.pos;
        let digits = |l: &mut Self| {
            while l.peek(0).is_ascii_digit() || l.peek(0) == b'_' {
                l.pos += 1;
            }
        };
        digits(self);
        let mut integral = true;
        if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
            integral = false;
            self.pos += 1;
            digits(self);
        }
        if matches!(self.peek(0), b'e' | b'E') {
            let sign = usize::from(matches!(self.peek(1), b'+' | b'-'));
            if self.peek(1 + sign).is_ascii_digit() {
                integral = false;
                self.pos += 1 + sign;
                digits(self);
            }
        }
        let text: String = self.src[start..self.pos]
            .chars()
            .filter(|&c| c != '_')
            .collect();
        let value: f64 = match text.parse() {
            Ok(v) => v,
            Err(_) => {
                self.fail(Diagnostic::error(
                    Span::new(start, self.pos),
                    "malformed number",
                ))?;
                0.0
            }
        };

        let unit_start = self.pos;
        while self.peek(0).is_ascii_alphanumeric() || self.peek(0) == b'_' {
            self.pos += 1;
        }
        let suffix = &self.src[unit_start..self.pos];
        let unit = if suffix.is_empty() {
            None
        } else {
            match Unit::ALL.iter().find(|(name, _)| *name == suffix) {
                Some(&(_, unit)) => Some(unit),
                None => {
                    let names = Unit::ALL.iter().map(|(n, _)| *n);
                    let known = names.clone().collect::<Vec<_>>().join(", ");
                    let err = Diagnostic::error(
                        Span::new(unit_start, self.pos),
                        format!("unknown unit `{suffix}`"),
                    );
                    self.fail(
                        match super::diag::suggest(suffix, names).or_else(|| {
                            Unit::ALL
                                .iter()
                                .map(|(n, _)| *n)
                                .find(|n| n.eq_ignore_ascii_case(suffix))
                        }) {
                            Some(s) => err.with_help(format!("did you mean `{s}`?")),
                            None => err.with_help(format!("known units: {known}")),
                        },
                    )?;
                    None
                }
            }
        };
        Ok(TokenKind::Number {
            value,
            unit,
            integral,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn num(value: f64, unit: Option<Unit>, integral: bool) -> TokenKind {
        TokenKind::Number {
            value,
            unit,
            integral,
        }
    }

    #[test]
    fn numbers_and_units() {
        use TokenKind::*;
        assert_eq!(
            kinds("440Hz 0.5Hz 2kHz 300ms 2s +7st 50cents 48_000 1e-3 3/2"),
            vec![
                num(440.0, Some(Unit::Hz), true),
                num(0.5, Some(Unit::Hz), false),
                num(2.0, Some(Unit::KHz), true),
                num(300.0, Some(Unit::Ms), true),
                num(2.0, Some(Unit::S), true),
                Plus,
                num(7.0, Some(Unit::St), true),
                num(50.0, Some(Unit::Cents), true),
                num(48000.0, None, true),
                num(0.001, None, false),
                num(3.0, None, true),
                Slash,
                num(2.0, None, true),
                Eof
            ]
        );
    }

    #[test]
    fn operators_keywords_and_comments() {
        use TokenKind::*;
        assert_eq!(
            kinds("rill f(x) Sample { x |> g /* c /* nested */ */ } // end\n a != b || !c"),
            vec![
                Rill, Ident, LParen, Ident, RParen, Ident, LBrace, Ident, Pipe, Ident, RBrace,
                Ident, Ne, Ident, OrOr, Bang, Ident, Eof
            ]
        );
    }

    #[test]
    fn tracks_line_breaks() {
        let toks = lex("a\n// comment\n  b c /*\n*/ d").unwrap();
        let flags: Vec<bool> = toks.iter().map(|t| t.newline_before).collect();
        assert_eq!(flags, [false, true, false, true, false]);
    }

    #[test]
    fn errors() {
        let e = lex("let x = 440hz").unwrap_err();
        assert_eq!(e.message, "unknown unit `hz`");
        assert_eq!(e.help.as_deref(), Some("did you mean `Hz`?"));
        assert_eq!(e.span, Span::new(11, 13));

        assert_eq!(
            lex("a | b").unwrap_err().message,
            "unexpected character `|`"
        );
        assert_eq!(
            lex("/* open").unwrap_err().message,
            "unterminated block comment"
        );
        assert_eq!(lex("é").unwrap_err().span, Span::new(0, 2));
    }

    #[test]
    fn decibel_typo_suggests_the_unit() {
        let e = lex("let g = 6db").unwrap_err();
        assert_eq!(e.message, "unknown unit `db`");
        assert_eq!(e.help.as_deref(), Some("did you mean `dB`?"));
    }

    #[test]
    fn sharps_only_in_note_names() {
        assert_eq!(
            kinds("F#4 C#"),
            vec![TokenKind::Ident, TokenKind::Ident, TokenKind::Eof]
        );
        let e = lex("let level#2 = 1").unwrap_err();
        assert_eq!(e.message, "unexpected character `#`");
        assert_eq!(
            e.help.as_deref(),
            Some("`#` only appears in note names, like `F#4`")
        );
        assert!(lex("a#").is_err());
        assert!(lex("H#4").is_err());
    }

    #[test]
    fn partial_lexing_reports_everything_and_keeps_going() {
        let (tokens, errors) = lex_partial("a | b 440hz é c /* open");
        let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "unexpected character `|`",
                "unknown unit `hz`",
                "unexpected character `é`",
                "unterminated block comment"
            ]
        );
        let kinds: Vec<TokenKind> = tokens.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Ident,
                TokenKind::Ident,
                num(440.0, None, true),
                TokenKind::Ident,
                TokenKind::Eof
            ]
        );
        // Valid text lexes the same either way.
        let src = "rill f(x: Sample) Sample { return x |> g(300ms) }";
        assert_eq!(lex_partial(src), (lex(src).unwrap(), vec![]));
    }
}
