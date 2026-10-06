//! The Rill language front end: lexing, parsing and type checking.
//!
//! ```
//! let src = "
//!     rill sine(freq: Hz) -> sample {
//!         state phase: f32 = 0
//!         phase = wrap(phase + freq / RATE)
//!         return sin(phase * TAU)
//!     }
//!     out(sine(440Hz) * 0.3)
//! ";
//! let (program, checked) = rill::lang::compile(src).unwrap();
//! assert_eq!(checked.signatures[0].to_string(), "rill sine(freq: Hz) -> sample");
//! # let _ = program;
//! ```

pub mod ast;
pub mod builtins;
pub mod check;
pub mod diag;
pub mod lexer;
pub mod parser;
pub mod types;

pub use check::Checked;
pub use diag::{Diagnostic, Severity, Span};

/// Parse `src` into a syntax tree.
pub fn parse(src: &str) -> Result<ast::Program, Vec<Diagnostic>> {
    let tokens = lexer::lex(src).map_err(|e| vec![e])?;
    parser::parse(src, tokens)
}

/// Parse and type-check `src`.
pub fn compile(src: &str) -> Result<(ast::Program, Checked), Vec<Diagnostic>> {
    let program = parse(src)?;
    let checked = check::check(&program)?;
    Ok((program, checked))
}
