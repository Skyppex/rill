//! Source spans and compiler diagnostics.

use std::fmt::Write as _;

/// Byte range into the source text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Span {
        Span {
            start: start as u32,
            end: end as u32,
        }
    }

    /// Smallest span covering both.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span,
            help: None,
        }
    }

    pub fn warning(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Warning,
            ..Diagnostic::error(span, message)
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Diagnostic {
        self.help = Some(help.into());
        self
    }

    fn kind(&self) -> &'static str {
        match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Render rustc-style, with the offending line and a caret underline.
    /// A diagnostic about the file as a whole ([`Span::default`]) gets no
    /// snippet.
    pub fn render(&self, file: &str, src: &str) -> String {
        if self.span == Span::default() {
            let mut out = format!("{}: {}\n --> {file}\n", self.kind(), self.message);
            if let Some(help) = &self.help {
                let _ = writeln!(out, "  = help: {help}");
            }
            return out;
        }
        let start = (self.span.start as usize).min(src.len());
        let end = (self.span.end as usize).clamp(start, src.len());
        let (line_no, col) = line_col(src, start);
        let line_start = src[..start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = src[start..].find('\n').map_or(src.len(), |i| start + i);
        let line = &src[line_start..line_end];

        let width = src[start..end.min(line_end)].chars().count().max(1);
        let gutter = line_no.to_string().len();
        let kind = self.kind();

        let mut out = String::new();
        let _ = writeln!(out, "{kind}: {}", self.message);
        let _ = writeln!(out, "{:gutter$}--> {file}:{line_no}:{col}", "");
        let _ = writeln!(out, "{:gutter$} |", "");
        let _ = writeln!(out, "{line_no} | {line}");
        let _ = writeln!(
            out,
            "{:gutter$} | {}{}",
            "",
            " ".repeat(col - 1),
            "^".repeat(width)
        );
        if let Some(help) = &self.help {
            let _ = writeln!(out, "{:gutter$} = help: {help}", "");
        }
        out
    }
}

/// 1-based line and column (in chars) of byte offset `at`.
pub fn line_col(src: &str, at: usize) -> (usize, usize) {
    let before = &src[..at];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = before[line_start..].chars().count() + 1;
    (line, col)
}

/// Closest of `candidates` to `name`, if it is close enough to be a likely
/// typo.
pub fn suggest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = name.len().div_ceil(3);
    candidates
        .into_iter()
        .map(|c| (edit_distance(name, c), c))
        .filter(|&(d, c)| d <= limit && c != name)
        .min_by_key(|&(d, _)| d)
        .map(|(_, c)| c)
}

/// Edit distance where swapping two neighbouring characters counts as one
/// edit (optimal string alignment), since that is the most common typo.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_with_caret() {
        let src = "let a = 1\nlet b = fo(a)\n";
        let at = src.find("fo").unwrap();
        let d = Diagnostic::error(Span::new(at, at + 2), "unknown name `fo`")
            .with_help("did you mean `foo`?");
        assert_eq!(
            d.render("x.rill", src),
            "error: unknown name `fo`\n \
             --> x.rill:2:9\n  \
             |\n\
             2 | let b = fo(a)\n  \
             |         ^^\n  \
             = help: did you mean `foo`?\n"
        );
    }

    #[test]
    fn file_level_diagnostics_have_no_snippet() {
        let d = Diagnostic::error(Span::default(), "there is no rill named `main` to run")
            .with_help("add one");
        assert_eq!(
            d.render("x.rill", "fn f() -> sample { 0 }"),
            "error: there is no rill named `main` to run\n --> x.rill\n  = help: add one\n"
        );
    }

    #[test]
    fn suggestions() {
        assert_eq!(suggest("sine", ["sin", "sine2", "saw"]), Some("sin"));
        assert_eq!(suggest("lvl", ["level", "x"]), None);
        assert_eq!(suggest("levle", ["level", "x"]), Some("level"));
        // A swap is one edit, so this beats `sin` (one deletion) by coming first.
        assert_eq!(suggest("sien", ["sine", "sin"]), Some("sine"));
    }
}
