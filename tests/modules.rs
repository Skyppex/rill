//! Programs made of several files, read from memory.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use rill::lang::{self, Diagnostic, SourceMap, Sources};
use rill::{Config, Engine, offline};

/// Files by path, as if on disk.
struct Files(HashMap<PathBuf, String>);

impl Files {
    fn new(files: &[(&str, &str)]) -> Files {
        Files(
            files
                .iter()
                .map(|(p, s)| (PathBuf::from(p), (*s).to_owned()))
                .collect(),
        )
    }
}

impl Sources for Files {
    fn read(&self, path: &Path) -> io::Result<String> {
        self.0
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

/// Check the program starting at the first file.
fn compile(
    files: &[(&str, &str)],
) -> (
    SourceMap,
    Result<(lang::ast::Program, lang::Checked), Vec<Diagnostic>>,
) {
    lang::compile_file(Path::new(files[0].0), &Files::new(files))
}

fn errors(files: &[(&str, &str)]) -> Vec<String> {
    match compile(files).1 {
        Ok(_) => Vec::new(),
        Err(diags) => diags
            .into_iter()
            .filter(Diagnostic::is_error)
            .map(|d| d.message)
            .collect(),
    }
}

fn warnings(files: &[(&str, &str)]) -> Vec<String> {
    match compile(files).1 {
        Ok((_, checked)) => checked.warnings.into_iter().map(|d| d.message).collect(),
        Err(diags) => panic!("{diags:#?}"),
    }
}

fn assert_ok(files: &[(&str, &str)]) {
    let (map, result) = compile(files);
    if let Err(diags) = result {
        let shown: String = diags.iter().map(|d| map.render(d)).collect();
        panic!("expected no errors:\n{shown}");
    }
}

/// The first `frames` samples of the program starting at the first file.
fn render(files: &[(&str, &str)], frames: usize) -> Vec<f32> {
    let config = Config {
        sample_rate: 48_000,
        max_frames: 256,
        out_channels: 1,
    };
    let options = lang::build::Options {
        seed: Some(0),
        ..Default::default()
    };
    let (map, result) = lang::load_file(
        Path::new(files[0].0),
        &Files::new(files),
        &config,
        "main",
        &options,
    );
    let graph = match result {
        Ok((graph, _)) => graph,
        Err(diags) => panic!(
            "{}",
            diags.iter().map(|d| map.render(d)).collect::<String>()
        ),
    };
    let mut engine = Engine::new(graph, config).unwrap();
    offline::render(&mut engine, frames, &offline::Blocks::Fixed(64))
}

const OSC: &str = "
fn blep(t: Float) Float { t * 0 }
export rill saw(freq: Freq) Sample {
    state phase: Float = 0
    phase = wrap(phase + freq / RATE)
    return phase * 2 - 1 - blep(phase)
}
";

#[test]
fn an_import_gives_what_is_exported() {
    assert_ok(&[
        (
            "song.rill",
            "import \"osc\"\nrill main() Sample { return saw(110Hz) }",
        ),
        ("osc.rill", OSC),
    ]);
}

#[test]
fn what_is_not_exported_stays_in_its_file() {
    let files = [
        (
            "song.rill",
            "import \"osc\"\nrill main() Sample { return blep(0.5) }",
        ),
        ("osc.rill", OSC),
    ];
    let (_, result) = compile(&files);
    let diags = result.unwrap_err();
    assert_eq!(diags[0].message, "`blep` is private to \"osc\"");
    assert_eq!(
        diags[0].help.as_deref(),
        Some("put `export` before it in \"osc\" to use it in other files")
    );
}

#[test]
fn a_file_that_is_not_imported_says_so() {
    let files = [
        (
            "song.rill",
            "import \"synth\"\nrill main() Sample { return lowpass(0) }",
        ),
        ("synth.rill", "import \"fx\"\nexport fn x() Float { 1 }"),
        (
            "fx.rill",
            "export rill lowpass(x: Sample) Sample { return x }",
        ),
    ];
    let (_, result) = compile(&files);
    let d = &result.unwrap_err()[0];
    assert_eq!(
        d.message,
        "`lowpass` is in \"fx\", which this file does not import"
    );
    assert_eq!(
        d.help.as_deref(),
        Some("add `import \"fx\"` at the top of this file")
    );
}

#[test]
fn export_import_passes_everything_on() {
    // `synth` re-exports `osc`; `song` gets `saw` through it, and `synth`
    // can still use `saw` itself.
    let files = [
        (
            "song.rill",
            "import \"synth\"\nrill main() Sample { return saw(110Hz) + pad(220Hz) }",
        ),
        (
            "synth.rill",
            "export import \"osc\"\nexport rill pad(f: Freq) Sample { return saw(f) * 0.5 }",
        ),
        ("osc.rill", OSC),
    ];
    assert_ok(&files);
    // Three deep.
    assert_ok(&[
        (
            "a.rill",
            "import \"b\"\nrill main() Sample { return saw(110Hz) }",
        ),
        ("b.rill", "export import \"c\""),
        ("c.rill", "export import \"osc\""),
        ("osc.rill", OSC),
    ]);
    // Without `export`, it is not passed on.
    let errs = errors(&[
        (
            "song.rill",
            "import \"synth\"\nrill main() Sample { return saw(110Hz) }",
        ),
        ("synth.rill", "import \"osc\"\nexport fn x() Float { 1 }"),
        ("osc.rill", OSC),
    ]);
    assert_eq!(
        errs,
        ["`saw` is in \"osc\", which this file does not import"]
    );
}

#[test]
fn paths_are_relative_to_the_importing_file() {
    assert_ok(&[
        (
            "songs/a.rill",
            "import \"../lib/synth\"\nrill main() Sample { return saw(110Hz) }",
        ),
        ("lib/synth.rill", "export import \"osc/saw\""),
        ("lib/osc/saw.rill", OSC),
    ]);
}

#[test]
fn import_paths_are_names() {
    let errs = |path: &str| {
        errors(&[
            (
                "song.rill",
                &format!("import \"{path}\"\nrill main() Sample {{ return 0 }}"),
            ),
            ("osc.rill", OSC),
        ])
    };
    assert_eq!(
        errs("osc.rill"),
        ["leave out `.rill`: a module is imported by its name"]
    );
    assert_eq!(
        errs("kick.wav"),
        ["only Rill files can be imported, and `kick.wav` is not one"]
    );
    assert_eq!(
        errs("my-osc"),
        ["`my-osc` is not a valid module name: `-` would read as minus"]
    );
    assert_eq!(errs("nothere"), ["cannot find module \"nothere\""]);
    assert_eq!(errs("song"), ["a file cannot import itself"]);
}

#[test]
fn a_file_name_that_is_not_a_name_is_a_warning() {
    let w = warnings(&[("my-song.rill", "rill main() Sample { return 0 }")]);
    assert_eq!(
        w,
        [
            "`my-song` is not a valid module name, so this file cannot be imported: `-` would read as minus"
        ]
    );
}

#[test]
fn errors_in_imported_files_are_shown_there() {
    let files = [
        (
            "song.rill",
            "import \"osc\"\nrill main() Sample { return 0 }",
        ),
        ("osc.rill", "export fn oops( Float { 1 }"),
    ];
    let (map, result) = compile(&files);
    let d = &result.unwrap_err()[0];
    assert!(
        map.render(d).contains("--> osc.rill:1:"),
        "{}",
        map.render(d)
    );
}

#[test]
fn spans_in_the_root_file_stay_where_they_are() {
    let src = "import \"osc\"\nrill main() Sample { return saw(110Hz) }";
    let (map, result) = compile(&[("song.rill", src), ("osc.rill", OSC)]);
    let (program, _) = result.unwrap();
    let main = program
        .items
        .iter()
        .find(|i| i.def().name.name == "main")
        .unwrap();
    assert_eq!(
        main.def().name.span.start as usize,
        src.find("main").unwrap()
    );
    assert_eq!(map.files[0].base, 0);
    assert!(map.files[1].base as usize > src.len());
}

#[test]
fn a_files_own_definition_wins() {
    assert_ok(&[
        (
            "song.rill",
            "import \"osc\"\nrill saw(f: Freq) Sample { return 0 }\nrill main() Sample { return saw(1Hz) }",
        ),
        ("osc.rill", OSC),
    ]);
}

#[test]
fn two_imports_with_one_name_clash_only_where_it_is_used() {
    let files = |use_it: &str| {
        [
            (
                "song.rill",
                format!("import \"a\"\nimport \"b\"\nrill main() Sample {{ return {use_it} }}"),
            ),
            (
                "a.rill",
                "export fn shared(x: Float) Float { x }\nexport fn only_a() Float { 1 }".to_owned(),
            ),
            (
                "b.rill",
                "export fn shared(x: Float) Float { x * 2 }".to_owned(),
            ),
        ]
    };
    let as_refs = |f: &[(&'static str, String); 3]| -> Vec<(&'static str, String)> { f.to_vec() };
    let check = |use_it: &str| {
        let f = as_refs(&files(use_it));
        let refs: Vec<(&str, &str)> = f.iter().map(|(p, s)| (*p, s.as_str())).collect();
        errors(&refs)
    };
    assert_eq!(check("only_a()"), Vec::<String>::new());
    assert_eq!(
        check("shared(1)"),
        ["`shared` is exported by both \"a\" and \"b\""]
    );
    // One thing reached by two routes is not a clash.
    assert_ok(&[
        (
            "song.rill",
            "import \"a\"\nimport \"b\"\nrill main() Sample { return saw(1Hz) }",
        ),
        ("a.rill", "export import \"osc\""),
        ("b.rill", "export import \"osc\""),
        ("osc.rill", OSC),
    ]);
}

#[test]
fn private_names_in_different_files_are_different_things() {
    // Each file has its own `helper` and its own `main`; neither is
    // recursion or a clash.
    let files = [
        (
            "song.rill",
            "import \"a\"\nfn helper(x: Float) Float { x + 1 }\nrill main() Sample { return helper(a(1)) }",
        ),
        (
            "a.rill",
            "fn helper(x: Float) Float { x * 10 }\nexport fn a(x: Float) Float { helper(x) }\nrill main() Sample { return 5 }",
        ),
    ];
    assert_ok(&files);
    assert_eq!(render(&files, 1), [11.0]);
}

#[test]
fn fns_passed_between_files_keep_their_own_meaning() {
    let files = [
        (
            "song.rill",
            "import \"apply\"\nfn twice(x: Float) Float { x * 2 }\nrill main() Sample { return apply(twice, 3) }",
        ),
        (
            "apply.rill",
            "fn twice(x: Float) Float { x * 100 }\nexport fn apply(f: fn(Float) Float, x: Float) Float { f(x) + twice(0) }",
        ),
    ];
    assert_eq!(render(&files, 1), [6.0]);
}

#[test]
fn imports_can_go_round_in_a_circle() {
    let files = [
        (
            "a.rill",
            "import \"b\"\nexport fn one() Float { 1 }\nrill main() Sample { return two() }",
        ),
        (
            "b.rill",
            "import \"a\"\nexport fn two() Float { one() + 1 }",
        ),
    ];
    assert_eq!(render(&files, 1), [2.0]);
    // Re-exports in a circle too.
    assert_ok(&[
        (
            "song.rill",
            "import \"a\"\nrill main() Sample { return x() + y() }",
        ),
        ("a.rill", "export import \"b\"\nexport fn x() Float { 1 }"),
        ("b.rill", "export import \"a\"\nexport fn y() Float { 2 }"),
    ]);
    // Recursion across files is still recursion.
    assert_eq!(
        errors(&[
            (
                "a.rill",
                "import \"b\"\nexport fn one() Float { two() }\nrill main() Sample { return one() }"
            ),
            ("b.rill", "import \"a\"\nexport fn two() Float { one() }"),
        ]),
        ["recursion is not allowed: `one` -> `two` -> `one`"]
    );
}

#[test]
fn a_file_reached_twice_is_loaded_once() {
    // `riff` is one sequence, whichever route reaches it.
    let files = [
        (
            "song.rill",
            "import \"a\"\nimport \"b\"\nrill main() Sample { return 0 }",
        ),
        ("a.rill", "export import \"tune\""),
        ("b.rill", "export import \"tune\""),
        ("tune.rill", "export seq riff { C4, E4 }"),
    ];
    let (map, result) = compile(&files);
    let (program, _) = result.unwrap();
    assert_eq!(map.files.len(), 4);
    assert_eq!(program.seqs.len(), 1);
}

#[test]
fn a_sequence_brings_its_events_and_fields() {
    let files = [
        (
            "song.rill",
            "
import \"tune\"
rill voice() Sample {
    state level: Float = 0
    on riff_note_on(note) claim { level = note.velocity }
    on riff_note_off release { level = 0 }
    return level
}
rill main() Sample {
    on start { invoke riff }
    let accents: [Float; riff.step_count] = [1, 0.5]
    return sum([voice(); 2]) * accents[0]
}",
        ),
        ("tune.rill", "export seq riff(velocity: 0.75) { C4, E4 }"),
    ];
    assert_eq!(render(&files, 1), [0.75]);
}

#[test]
fn exported_events_and_consts() {
    let files = [
        (
            "song.rill",
            "
import \"setup\"
rill main() Sample {
    state x: Float = 0
    on keys(note) { x = note.velocity }
    let levels: [Float; VOICES] = [LEVEL; VOICES]
    return levels[3] + x
}",
        ),
        (
            "setup.rill",
            "export event keys note_on(channel: 1)\nexport const VOICES = HALF * 2\nconst HALF = 2\nexport const LEVEL = 0.25",
        ),
    ];
    assert_eq!(render(&files, 1), [0.25]);
    // A private `const` stays private, even when an exported one uses it.
    assert_eq!(
        errors(&[
            (
                "song.rill",
                "import \"setup\"\nrill main() Sample { return HALF }"
            ),
            (
                "setup.rill",
                "export const VOICES = HALF * 2\nconst HALF = 2"
            ),
        ]),
        ["`HALF` is private to \"setup\""]
    );
}

#[test]
fn exports_are_never_unused() {
    let w = warnings(&[
        (
            "song.rill",
            "import \"tune\"\nrill main() Sample {\n    on start { invoke riff }\n    return 0\n}",
        ),
        (
            "tune.rill",
            "export seq riff { C4 }\nexport event keys note_on\nseq hidden { C4 }",
        ),
    ]);
    assert_eq!(w, ["sequence `hidden` is never invoked"]);
}

#[test]
fn an_import_nothing_uses_is_a_warning() {
    let w = warnings(&[
        (
            "song.rill",
            "import \"osc\"\nimport \"lib\"\nrill main() Sample { return 0 }",
        ),
        ("osc.rill", OSC),
        ("lib.rill", "export import \"osc\""),
    ]);
    assert_eq!(
        w,
        [
            "nothing from \"osc\" is used here",
            "nothing from \"lib\" is used here"
        ]
    );
    // Using what an import passes on counts.
    assert_eq!(
        warnings(&[
            (
                "song.rill",
                "import \"lib\"\nrill main() Sample { return saw(1Hz) }"
            ),
            ("osc.rill", OSC),
            ("lib.rill", "export import \"osc\""),
        ]),
        Vec::<String>::new()
    );
}

#[test]
fn an_import_of_a_file_that_exports_nothing_is_a_warning() {
    let w = warnings(&[
        (
            "song.rill",
            "import \"empty\"\nrill main() Sample { return 0 }",
        ),
        ("empty.rill", "fn private() Float { 1 }"),
    ]);
    assert_eq!(w, ["\"empty\" exports nothing"]);
}

#[test]
fn the_entry_can_be_imported() {
    let files = Files::new(&[
        (
            "song.rill",
            "import \"osc\"\nrill main() Sample { return 0 }",
        ),
        (
            "osc.rill",
            "export rill demo() Sample { return 0.5 }\nrill hidden() Sample { return 0 }",
        ),
    ]);
    let entry = |name: &str| {
        lang::compile_file_entry(Path::new("song.rill"), name, &files)
            .1
            .err()
            .map(|d| d[0].message.clone())
    };
    assert_eq!(entry("demo"), None);
    assert_eq!(
        entry("hidden"),
        Some("there is no rill named `hidden` to run".into())
    );
}

#[test]
fn a_program_split_over_files_sounds_the_same() {
    let one = "
fn blep(t: Float) Float { t * 0 }
rill saw(freq: Freq) Sample {
    state phase: Float = 0
    phase = wrap(phase + freq / RATE)
    return phase * 2 - 1 - blep(phase)
}
const ROOT = 110Hz
rill main() Sample { return saw(ROOT) + saw(ROOT * 1.5) * 0.5 }
";
    let split = [
        (
            "song.rill",
            "import \"osc\"\nimport \"tuning\"\nrill main() Sample { return saw(ROOT) + saw(ROOT * 1.5) * 0.5 }",
        ),
        ("osc.rill", OSC),
        ("tuning.rill", "export const ROOT = 110Hz"),
    ];
    assert_eq!(render(&split, 512), render(&[("one.rill", one)], 512));
}

#[test]
fn a_string_with_imports_needs_a_file() {
    let errs = lang::compile("import \"osc\"\nrill main() Sample { return 0 }").unwrap_err();
    assert_eq!(errs[0].message, "imports need a file to look next to");
}

#[test]
fn import_and_export_only_at_the_top_level() {
    let errs =
        lang::parse("rill main() Sample {\n    import \"osc\"\n    return 0\n}").unwrap_err();
    assert_eq!(
        errs[0].help.as_deref(),
        Some("`import` only works at the top level of a file, outside any definition")
    );
    let errs = lang::parse("export let x = 1").unwrap_err();
    assert_eq!(
        errs[0].message,
        "expected `fn`, `rill`, `const`, `event`, `seq` or `import` after `export`, found `let`"
    );
}
