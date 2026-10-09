//! The Rill language front end: lexing, parsing and type checking.
//!
//! ```
//! let src = "
//!     rill sine(freq: Freq) Sample {
//!         state phase: Float = 0
//!         phase = wrap(phase + freq / RATE)
//!         return sin(phase * TAU)
//!     }
//!     rill main(freq: Freq = 440Hz) Sample {
//!         return sine(freq) * 0.3
//!     }
//! ";
//! let (program, checked) = rill::lang::compile(src).unwrap();
//! assert_eq!(checked.signatures[0].to_string(), "rill sine(freq: Freq) Sample");
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
pub mod module;
pub mod opt;
pub mod parser;
pub mod pretty;
pub mod types;
pub mod vector;
pub mod vm;

pub use check::Checked;
pub use diag::{Diagnostic, Severity, Span};
pub use module::{Disk, SourceMap, Sources};

use std::path::Path;

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
    load_with(src, config, entry, &build::Options::default())
}

/// [`load`] with build [`Options`](build::Options).
pub fn load_with(
    src: &str,
    config: &crate::Config,
    entry: &str,
    options: &build::Options,
) -> Result<(crate::Graph, Vec<Diagnostic>), Vec<Diagnostic>> {
    let (program, checked) = compile(src)?;
    let graph = build::build_with(&program, &checked, config, entry, options)?;
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
    if !program.imports.is_empty() {
        return Err(program
            .imports
            .iter()
            .map(|i| {
                Diagnostic::error(i.path_span, "imports need a file to look next to")
                    .with_help("compile the program from its file, as `rill check song.rill` does")
            })
            .collect());
    }
    let checked = check::check(&program)?;
    Ok((program, checked))
}

/// Parse and type-check the program starting at the file `path`, with every
/// file it imports, read through `sources`. The [`SourceMap`] shows the
/// diagnostics, whichever file they are in.
pub fn compile_file(
    path: &Path,
    sources: &dyn Sources,
) -> (SourceMap, Result<(ast::Program, Checked), Vec<Diagnostic>>) {
    let loaded = module::load_tree(path, None, sources, false);
    let mut found = loaded.diagnostics;
    if found.iter().any(Diagnostic::is_error) {
        found.sort_by_key(|d| d.span.start);
        return (loaded.sources, Err(found));
    }
    let result = match check::check(&loaded.program) {
        Ok(mut checked) => {
            found.extend(checked.warnings);
            found.sort_by_key(|d| d.span.start);
            checked.warnings = found;
            Ok((loaded.program, checked))
        }
        Err(mut diags) => {
            diags.extend(found);
            diags.sort_by_key(|d| (!d.is_error(), d.span.start));
            Err(diags)
        }
    };
    (loaded.sources, result)
}

/// [`compile_file`], including that `entry` can run as the program.
pub fn compile_file_entry(
    path: &Path,
    entry: &str,
    sources: &dyn Sources,
) -> (SourceMap, Result<(ast::Program, Checked), Vec<Diagnostic>>) {
    let (map, result) = compile_file(path, sources);
    let result = result.and_then(|(program, checked)| {
        check::check_entry(&program, &checked, entry)?;
        Ok((program, checked))
    });
    (map, result)
}

/// [`load_with`] for the program starting at the file `path`.
pub fn load_file(
    path: &Path,
    sources: &dyn Sources,
    config: &crate::Config,
    entry: &str,
    options: &build::Options,
) -> (SourceMap, Result<(crate::Graph, Checked), Vec<Diagnostic>>) {
    let (map, result) = compile_file(path, sources);
    let result = result.and_then(|(program, checked)| {
        let graph = build::build_with(&program, &checked, config, entry, options)?;
        Ok((graph, checked))
    });
    (map, result)
}
