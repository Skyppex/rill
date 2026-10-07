//! The Rill language front end: lexing, parsing and type checking.
//!
//! ```
//! let src = "
//!     rill sine(freq: Freq) -> Sample {
//!         state phase: Float = 0
//!         phase = wrap(phase + freq / RATE)
//!         return sin(phase * TAU)
//!     }
//!     rill main(freq: Freq = 440Hz) -> Sample {
//!         return sine(freq) * 0.3
//!     }
//! ";
//! let (program, checked) = rill::lang::compile(src).unwrap();
//! assert_eq!(checked.signatures[0].to_string(), "rill sine(freq: Freq) -> Sample");
//!
//! let config = rill::Config::default();
//! let (graph, _warnings) = rill::lang::load(src, &config, "main").unwrap();
//! let mut engine = rill::Engine::new(graph, config).unwrap();
//! # let _ = (program, &mut engine);
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

/// Parse, check and build `src` into a graph that runs the rill named
/// `entry` (normally [`build::DEFAULT_ENTRY`]) on an engine with `config`.
/// On success, also returns any warnings.
pub fn load(
    src: &str,
    config: &crate::Config,
    entry: &str,
) -> Result<(crate::Graph, Vec<Diagnostic>), Vec<Diagnostic>> {
    let (program, checked) = compile(src)?;
    let graph = build::build(&program, &checked, config, entry)?;
    Ok((graph, checked.warnings))
}

/// Parse and check `src`, including that `entry` can run as the program.
pub fn compile_entry(src: &str, entry: &str) -> Result<(ast::Program, Checked), Vec<Diagnostic>> {
    let (program, checked) = compile(src)?;
    check::check_entry(&program, &checked, entry)?;
    Ok((program, checked))
}

/// Parse and type-check `src`.
pub fn compile(src: &str) -> Result<(ast::Program, Checked), Vec<Diagnostic>> {
    let program = parse(src)?;
    let checked = check::check(&program)?;
    Ok((program, checked))
}
