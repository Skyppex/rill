//! Programs made of several files.
//!
//! A file is a module. It names the files it uses with `import ./path` and
//! sees what they `export`; `export import ./path` passes on everything that
//! file exports. [`load_tree`] reads the file a program starts from and
//! every file it reaches, then merges them into one [`Program`] that the
//! checker and compiler treat like one file, with each declaration tagged
//! with its module (see [`Modules`]).
//!
//! Every file gets its own range of the program's spans, one after the
//! other, starting with the root file at 0. A [`SourceMap`] turns a span
//! back into a file, for showing diagnostics.

use std::collections::BTreeSet;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::ast::{ModuleInfo, Modules, Program};
use super::diag::{Diagnostic, Span};
use super::{lexer, parser};

/// Where the files a program is built from come from: the disk, an
/// editor's open buffers, memory, or anything else. Every file is read
/// through here in the build stage (when a program is checked, built or
/// reloaded), never while it plays.
pub trait Sources {
    /// A module's text.
    fn read(&self, path: &Path) -> io::Result<String>;

    /// The one path a file is known by, so a file reached by two routes is
    /// loaded once.
    fn canonical(&self, path: &Path) -> PathBuf {
        normalize(path)
    }

    /// A file's bytes, as for an audio file. From the disk unless said
    /// otherwise.
    fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// A version of a file: while it stays the same, what was read from it
    /// is kept. `None` reads it every time. From the disk unless said
    /// otherwise.
    fn stamp(&self, path: &Path) -> Option<u64> {
        crate::audio::disk_stamp(path)
    }

    /// An audio file, decoded: by default its bytes through the decoder
    /// (see [`crate::audio`]). Give samples from anywhere by overriding it;
    /// then no decoder is needed.
    fn audio(&self, path: &Path) -> Result<std::sync::Arc<crate::audio::Audio>, String> {
        let bytes = self
            .read_bytes(path)
            .map_err(|e| format!("cannot read it: {e}"))?;
        crate::audio::decode_bytes(bytes, path).map(std::sync::Arc::new)
    }

    /// What to watch for changes to the file at `path`, for hot reload:
    /// the file itself unless said otherwise, or nothing.
    fn watch(&self, path: &Path) -> Option<PathBuf> {
        Some(path.to_owned())
    }
}

/// No files at all: for a program from a string, which can only import
/// the core library.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFiles;

impl Sources for NoFiles {
    fn read(&self, _: &Path) -> io::Result<String> {
        Err(io::Error::from(io::ErrorKind::NotFound))
    }
}

/// Files on disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct Disk;

impl Sources for Disk {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn canonical(&self, path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
    }
}

/// `path` with `.` and `dir/..` taken out, without asking the disk.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                let up = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if up {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// One file of a program.
#[derive(Clone, Debug)]
pub struct SourceFile {
    /// How to show the file in a diagnostic: its path as the user would
    /// type it.
    pub display: String,
    pub path: Option<PathBuf>,
    pub text: String,
    /// Where its text starts in the program's spans.
    pub base: u32,
}

impl SourceFile {
    /// For a file of the core library, its name as imported, without `@`:
    /// `"gen/osc"`.
    pub fn core_module(&self) -> Option<&str> {
        self.display.strip_prefix('@')?.strip_suffix(".rill")
    }
}

/// The files of a program and where each one's spans are.
#[derive(Clone, Debug, Default)]
pub struct SourceMap {
    pub files: Vec<SourceFile>,
}

impl SourceMap {
    /// A program of one file, or of a string shown as `display`.
    pub fn single(display: impl Into<String>, text: impl Into<String>) -> SourceMap {
        SourceMap {
            files: vec![SourceFile {
                display: display.into(),
                path: None,
                text: text.into(),
                base: 0,
            }],
        }
    }

    /// The index of the file `offset` is in.
    pub fn file_at(&self, offset: u32) -> Option<usize> {
        let after = self.files.partition_point(|f| f.base <= offset);
        after.checked_sub(1)
    }

    /// The text `span` covers, from whichever file it is in.
    pub fn slice(&self, span: Span) -> &str {
        let Some(f) = self.file_at(span.start).map(|i| &self.files[i]) else {
            return "";
        };
        let start = (span.start - f.base) as usize;
        let end = (span.end.saturating_sub(f.base) as usize).min(f.text.len());
        f.text.get(start.min(end)..end).unwrap_or("")
    }

    /// `d` rendered against the file it is in. A diagnostic about the
    /// program as a whole goes with the root file.
    pub fn render(&self, d: &Diagnostic) -> String {
        let Some(f) = self.files.get(self.file_at(d.span.start).unwrap_or(0)) else {
            return d.render("", "");
        };
        let mut local = d.clone();
        if d.span != Span::default() {
            local.span = Span {
                start: d.span.start - f.base,
                end: d.span.end.saturating_sub(f.base),
            };
        }
        local.render(&f.display, &f.text)
    }
}

/// A program read from files, with what went wrong reading it.
#[derive(Debug)]
pub struct Loaded {
    pub program: Program,
    pub sources: SourceMap,
    /// Files that could not be found or parsed, and imports that are not
    /// allowed, along with any warnings.
    pub diagnostics: Vec<Diagnostic>,
}

/// Read the program starting at `root`, and every file it imports, through
/// `sources`. `root_text` is the root file's text if it is already known
/// (an editor's buffer). With `recover`, every file is parsed the way
/// [`parser::parse_partial`] does it.
///
/// The program always comes back, with whatever could be read; anything
/// that went wrong is in [`Loaded::diagnostics`].
pub fn load_tree(
    root: &Path,
    root_text: Option<&str>,
    sources: &dyn Sources,
    recover: bool,
) -> Loaded {
    let mut diagnostics = Vec::new();
    let root_text = match root_text {
        Some(t) => t.to_owned(),
        None => match sources.read(root) {
            Ok(t) => t,
            Err(e) => {
                diagnostics.push(Diagnostic::error(
                    Span::default(),
                    format!("cannot read {}: {e}", root.display()),
                ));
                String::new()
            }
        },
    };
    let stem = root
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    // A program from a string, not a file: it can only import core.
    let no_file = root.as_os_str().is_empty();
    let mut l = Loader {
        files: Vec::new(),
        map: SourceMap::default(),
        infos: Vec::new(),
        next_id: 0,
        next_base: 0,
        recover,
        diagnostics,
    };
    l.add(
        Some(root.to_owned()),
        sources.canonical(root),
        stem,
        root.display().to_string(),
        root_text,
    );

    // Breadth first: each file's imports, loading the files not seen yet.
    let mut i = 0;
    while i < l.files.len() {
        let here = l.infos[i].path.clone().unwrap_or_default();
        let dir = here.parent().map(Path::to_owned).unwrap_or_default();
        let name_dir = Path::new(&l.infos[i].name)
            .parent()
            .map(Path::to_owned)
            .unwrap_or_default();
        let display_dir = Path::new(&l.map.files[i].display)
            .parent()
            .map(Path::to_owned)
            .unwrap_or_default();
        let mut seen: Vec<PathBuf> = Vec::new();
        for k in 0..l.files[i].program.imports.len() {
            let import = l.files[i].program.imports[k].clone();
            if let Err(d) = check_path(&import.path, import.path_span) {
                l.diagnostics.push(d);
                continue;
            }
            if let Some(name) = import.path.strip_prefix('@') {
                match load_core(&mut l, name, import.path_span) {
                    Some(t) => l.files[i].program.imports[k].module = Some(t as u32),
                    None => continue,
                }
                continue;
            }
            if no_file && i == 0 {
                l.diagnostics.push(
                    Diagnostic::error(import.path_span, "imports need a file to look next to")
                        .with_help("only core modules, such as `@gen/osc`, can be imported by a program that is not in a file"),
                );
                continue;
            }
            // Core modules only import each other: a relative path in one
            // would depend on where the compiler was built.
            if l.map.files[i].core_module().is_some() {
                l.diagnostics.push(Diagnostic::error(
                    import.path_span,
                    "a core module imports other core modules, with `@`",
                ));
                continue;
            }
            let rel = format!("{}.rill", import.path);
            let path = normalize(&dir.join(&rel));
            let canonical = sources.canonical(&path);
            if canonical == l.files[i].canonical {
                l.diagnostics.push(
                    Diagnostic::error(import.path_span, "a file cannot import itself")
                        .with_help("everything a file declares is already usable in it"),
                );
                continue;
            }
            if seen.contains(&canonical) {
                l.diagnostics.push(
                    Diagnostic::warning(
                        import.path_span,
                        format!("`{}` is already imported here", import.path),
                    )
                    .with_help("remove this line"),
                );
            }
            seen.push(canonical.clone());
            let target = match l.files.iter().position(|f| f.canonical == canonical) {
                Some(t) => t,
                None => match sources.read(&path) {
                    Ok(text) => {
                        let name = normalize(&name_dir.join(&import.path))
                            .to_string_lossy()
                            .replace('\\', "/");
                        let display = normalize(&display_dir.join(&rel)).display().to_string();
                        l.add(Some(path.clone()), canonical, name, display, text);
                        l.files.len() - 1
                    }
                    Err(_) => {
                        l.diagnostics.push(
                            Diagnostic::error(
                                import.path_span,
                                format!("cannot find module `{}`", import.path),
                            )
                            .with_help(format!("looked for {}", path.display())),
                        );
                        continue;
                    }
                },
            };
            l.files[i].program.imports[k].module = Some(target as u32);
        }
        i += 1;
    }
    let Loader {
        files,
        map,
        mut infos,
        next_id,
        diagnostics,
        ..
    } = l;

    // What each module passes on: the modules it imports with `export`,
    // and what those pass on, until nothing changes (imports can go round
    // in a circle).
    let n = files.len();
    let direct = |m: usize, exported_only: bool| -> Vec<usize> {
        files[m]
            .program
            .imports
            .iter()
            .filter(|imp| !exported_only || imp.export.is_some())
            .filter_map(|imp| imp.module.map(|t| t as usize))
            .collect()
    };
    let mut passes_on: Vec<BTreeSet<usize>> = (0..n)
        .map(|m| direct(m, true).into_iter().collect())
        .collect();
    loop {
        let mut changed = false;
        for m in 0..n {
            let more: Vec<usize> = passes_on[m]
                .iter()
                .flat_map(|&t| passes_on[t].iter().copied())
                .collect();
            for t in more {
                changed |= passes_on[m].insert(t);
            }
        }
        if !changed {
            break;
        }
    }
    for (m, info) in infos.iter_mut().enumerate() {
        let mut sees = BTreeSet::new();
        for t in direct(m, false) {
            sees.insert(t);
            sees.extend(passes_on[t].iter().copied());
        }
        sees.remove(&m);
        info.sees = sees.into_iter().map(|t| t as u32).collect();
    }

    // One program, with each declaration tagged with its module.
    let mut program = Program {
        items: Vec::new(),
        events: Vec::new(),
        seqs: Vec::new(),
        consts: Vec::new(),
        imports: Vec::new(),
        modules: Modules::default(),
        expr_count: next_id,
    };
    for (m, file) in files.into_iter().enumerate() {
        let p = file.program;
        let m32 = m as u32;
        let tag = |list: &mut Vec<u32>, count: usize| list.extend(std::iter::repeat_n(m32, count));
        tag(&mut program.modules.items, p.items.len());
        tag(&mut program.modules.events, p.events.len());
        tag(&mut program.modules.seqs, p.seqs.len());
        tag(&mut program.modules.consts, p.consts.len());
        tag(&mut program.modules.imports, p.imports.len());
        program.items.extend(p.items);
        program.events.extend(p.events);
        program.seqs.extend(p.seqs);
        program.consts.extend(p.consts);
        program.imports.extend(p.imports);
    }
    program.modules.files = infos;
    Loaded {
        program,
        sources: map,
        diagnostics,
    }
}

struct File {
    canonical: PathBuf,
    program: Program,
}

/// The files read so far.
struct Loader {
    files: Vec<File>,
    map: SourceMap,
    infos: Vec<ModuleInfo>,
    next_id: u32,
    next_base: u32,
    recover: bool,
    diagnostics: Vec<Diagnostic>,
}

impl Loader {
    /// Parse one more file, after the others in the program's spans.
    fn add(
        &mut self,
        path: Option<PathBuf>,
        canonical: PathBuf,
        name: String,
        display: String,
        text: String,
    ) {
        let base = self.next_base;
        self.next_base = base + text.len() as u32 + 1;
        let (tokens, lex_errors) = lexer::lex_partial_at(&text, base);
        let (program, parse_errors) =
            parser::parse_file(&text, tokens, base, self.next_id, self.recover);
        self.next_id = program.expr_count;
        self.diagnostics.extend(lex_errors);
        self.diagnostics.extend(parse_errors);
        self.map.files.push(SourceFile {
            display,
            path: path.clone(),
            text,
            base,
        });
        self.infos.push(ModuleInfo {
            name,
            path,
            sees: Vec::new(),
        });
        self.files.push(File { canonical, program });
    }
}

/// Load the core module `name` (as in `"gen/osc"`), or find it already
/// loaded. An error at `span` if there is none by that name.
fn load_core(l: &mut Loader, name: &str, span: Span) -> Option<usize> {
    let canonical = PathBuf::from(format!("@{name}.rill"));
    if let Some(t) = l.files.iter().position(|f| f.canonical == canonical) {
        return Some(t);
    }
    let Some(text) = super::core::module(name) else {
        let names: Vec<String> = super::core::names().collect();
        let wanted = format!("@{name}");
        let help = match super::diag::suggest(&wanted, names.iter().map(String::as_str)) {
            Some(s) => format!("did you mean `{s}`? `rill core` lists them all"),
            None => format!(
                "the core modules are {}",
                names
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        l.diagnostics.push(
            Diagnostic::error(span, format!("there is no core module `{wanted}`")).with_help(help),
        );
        return None;
    };
    l.add(
        None,
        canonical.clone(),
        format!("@{name}"),
        format!("@{name}.rill"),
        text.to_owned(),
    );
    Some(l.files.len() - 1)
}

/// What is wrong with `path` in `import path`, as an error at `span`.
pub fn check_path(path: &str, span: Span) -> Result<(), Diagnostic> {
    let error =
        |message: String, help: String| Err(Diagnostic::error(span, message).with_help(help));
    // A core module: `@` and its path in the core library.
    if let Some(rest) = path.strip_prefix('@') {
        let rest = rest.strip_suffix(".rill").map_or(Ok(rest), |stem| {
            Err(
                Diagnostic::error(span, "leave out `.rill`: a module is imported by its name")
                    .with_help(format!("write `import @{stem}`")),
            )
        })?;
        if rest.is_empty()
            || rest
                .split('/')
                .any(|p| p.is_empty() || p == ".." || p == ".")
        {
            return error(
                "a core module is named by its folders in the core library".into(),
                "as in `import @gen/osc`".into(),
            );
        }
        return Ok(());
    }
    if let Some(stem) = path.strip_suffix(".rill") {
        return error(
            "leave out `.rill`: a module is imported by its name".into(),
            format!("write `import {stem}`"),
        );
    }
    let last = path.rsplit('/').next().unwrap_or(path);
    if let Some((_, ext)) = last.rsplit_once('.')
        && !ext.is_empty()
        && last != ".."
    {
        return error(
            format!("only Rill files can be imported, and `{last}` is not one"),
            "a module is a `.rill` file, imported by its name without the extension".into(),
        );
    }
    let rest = path
        .strip_prefix('/')
        .unwrap_or(path)
        .trim_start_matches("./");
    if rest
        .split('/')
        .any(|p| p.is_empty() || (p == "." && !rest.is_empty()))
        || matches!(last, "." | "..")
    {
        return error(
            "a module's path names a file, with one `/` between folders".into(),
            "as in `import ./lib/osc` for `lib/osc.rill` next to this file".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_normalize() {
        assert_eq!(
            normalize(Path::new("a/./b/../c.rill")),
            PathBuf::from("a/c.rill")
        );
        assert_eq!(normalize(Path::new("../x")), PathBuf::from("../x"));
        assert_eq!(normalize(Path::new("a/../../x")), PathBuf::from("../x"));
    }

    #[test]
    fn import_paths() {
        let s = Span::default();
        let msg = |p: &str| check_path(p, s).map_err(|d| d.message);
        assert_eq!(msg("./osc"), Ok(()));
        assert_eq!(msg("./lib/osc"), Ok(()));
        assert_eq!(msg("../shared/osc"), Ok(()));
        assert_eq!(msg("./my-osc"), Ok(()));
        assert_eq!(msg("/abs/osc"), Ok(()));
        assert_eq!(msg("@gen/osc"), Ok(()));
        assert_eq!(
            msg("./osc.rill"),
            Err("leave out `.rill`: a module is imported by its name".into())
        );
        assert_eq!(
            msg("./kick.wav"),
            Err("only Rill files can be imported, and `kick.wav` is not one".into())
        );
        assert!(msg("./lib//osc").is_err());
        assert!(msg("./lib/").is_err());
        assert!(msg("..").is_err());
        assert!(msg("@").is_err());
    }
}
