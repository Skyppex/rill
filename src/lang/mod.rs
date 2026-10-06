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
pub mod build;
pub mod builtins;
pub mod check;
pub mod compile;
pub mod diag;
pub mod lexer;
pub mod parser;
pub mod pretty;
pub mod types;
pub mod vm;

pub use check::Checked;
pub use diag::{Diagnostic, Severity, Span};

/// Parse `src` into a syntax tree.
pub fn parse(src: &str) -> Result<ast::Program, Vec<Diagnostic>> {
    let tokens = lexer::lex(src).map_err(|e| vec![e])?;
    parser::parse(src, tokens)
}

/// Parse, check and build `src` into a graph for an engine with `config`.
/// On success, also returns any warnings.
pub fn load(
    src: &str,
    config: &crate::Config,
) -> Result<(crate::Graph, Vec<Diagnostic>), Vec<Diagnostic>> {
    let (program, checked) = compile(src)?;
    let built = build::build(&program, &checked, config)?;
    let mut warnings = checked.warnings;
    warnings.extend(built.warnings);
    Ok((built.graph, warnings))
}

/// Parse and type-check `src`.
pub fn compile(src: &str) -> Result<(ast::Program, Checked), Vec<Diagnostic>> {
    let program = parse(src)?;
    let checked = check::check(&program)?;
    Ok((program, checked))
}
