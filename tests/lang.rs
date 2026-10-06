//! Parser and type checker, through the public `rill::lang` API.

use rill::lang::ast::{BinOp, ExprKind, Item, Stmt};
use rill::lang::types::{Size, Type};
use rill::lang::{self, Diagnostic};

const SINE: &str = "
rill sine(freq: Hz) -> sample {
    state phase: f32 = 0
    phase = wrap(phase + freq / RATE)
    return sin(phase * TAU)
}
rill peak(x: sample, release: Time = 300ms) -> sample {
    state level: sample = 0
    let a = abs(x)
    level = if a > level { a } else { level * decay(release) }
    return level
}
rill mix_down<N>(x: [sample; N]) -> [sample; 1] {
    return [sum(x) / N]
}
";

fn diagnostics(src: &str) -> Vec<Diagnostic> {
    match lang::compile(src) {
        Ok((_, checked)) => checked.warnings,
        Err(diags) => diags,
    }
}

/// Messages of all errors in `src`.
fn errors(src: &str) -> Vec<String> {
    diagnostics(src)
        .into_iter()
        .filter(|d| d.is_error())
        .map(|d| d.message)
        .collect()
}

/// The single error in `src`, with its help text.
fn error(src: &str) -> (String, Option<String>) {
    let errs: Vec<Diagnostic> = diagnostics(src)
        .into_iter()
        .filter(|d| d.is_error())
        .collect();
    assert_eq!(errs.len(), 1, "expected one error, got {errs:#?}");
    (errs[0].message.clone(), errs[0].help.clone())
}

fn assert_ok(src: &str) {
    if let Err(diags) = lang::compile(src) {
        let rendered: String = diags.iter().map(|d| d.render("test.rill", src)).collect();
        panic!("expected no errors:\n{rendered}");
    }
}

/// Type of the top-level `let name = ...`.
fn type_of(src: &str, name: &str) -> Type {
    let (program, checked) = lang::compile(src).unwrap_or_else(|d| panic!("{d:#?}"));
    for item in &program.items {
        if let Item::Stmt(Stmt::Let { name: n, value, .. }) = item
            && n.name == name
        {
            return checked.types[value.id as usize].clone();
        }
    }
    panic!("no top-level let `{name}`");
}

fn frame(t: Type, n: u32) -> Type {
    Type::Frame(Box::new(t), Size::Const(n))
}

// ---- examples -----------------------------------------------------------

#[test]
fn examples_check_cleanly() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rill") {
            let src = std::fs::read_to_string(&path).unwrap();
            match lang::compile(&src) {
                Ok((_, checked)) => assert!(
                    checked.warnings.is_empty(),
                    "{path:?}: {:?}",
                    checked.warnings
                ),
                Err(diags) => {
                    let name = path.display().to_string();
                    let rendered: String = diags.iter().map(|d| d.render(&name, &src)).collect();
                    panic!("{rendered}");
                }
            }
            count += 1;
        }
    }
    assert!(count >= 2);
}

#[test]
fn design_doc_graph_types() {
    let src = format!(
        "{SINE}\nlet lfo = sine(0.5Hz) * 20Hz + 440Hz\nlet voice = sine(lfo) * 0.3\nout(voice)"
    );
    assert_eq!(type_of(&src, "lfo"), Type::Hz);
    assert_eq!(type_of(&src, "voice"), Type::Sample);
}

// ---- parsing ------------------------------------------------------------

#[test]
fn pipes_desugar_to_calls() {
    let src = format!("{SINE}\nlet x = sine(1Hz) |> peak(release: 10ms) |> abs");
    let (program, _) = lang::compile(&src).unwrap();
    let Some(Item::Stmt(Stmt::Let { value, .. })) = program.items.last() else {
        panic!()
    };
    let ExprKind::Call {
        callee,
        args,
        piped,
    } = &value.kind
    else {
        panic!("{value:?}")
    };
    assert_eq!(callee.name, "abs");
    assert!(piped);
    let ExprKind::Call {
        callee,
        args: inner,
        ..
    } = &args[0].value.kind
    else {
        panic!()
    };
    assert_eq!(callee.name, "peak");
    assert_eq!(inner.len(), 2);
    assert_eq!(inner[1].name.as_ref().unwrap().name, "release");
}

#[test]
fn precedence() {
    let program = lang::parse("let x = 1 + 2 * 3 < 4 && true").unwrap();
    let Item::Stmt(Stmt::Let { value, .. }) = &program.items[0] else {
        panic!()
    };
    let ExprKind::Binary(BinOp::And, cmp, _) = &value.kind else {
        panic!()
    };
    let ExprKind::Binary(BinOp::Lt, sum, _) = &cmp.kind else {
        panic!()
    };
    let ExprKind::Binary(BinOp::Add, _, product) = &sum.kind else {
        panic!()
    };
    assert!(matches!(product.kind, ExprKind::Binary(BinOp::Mul, ..)));
}

#[test]
fn line_breaks_end_statements() {
    // A `-` at the start of a line begins a new statement...
    let program = lang::parse("let a = 1\n-2").unwrap();
    assert_eq!(program.items.len(), 2);
    // ...but not inside brackets, and `|>` always continues.
    let program = lang::parse("let a = (1\n- 2)\nlet b = a\n  |> f\n  |> g").unwrap();
    assert_eq!(program.items.len(), 2);
    // `else` may start a line.
    lang::parse("let a = if true { 1 }\nelse { 2 }").unwrap();
    // Two statements on one line need a `;`.
    lang::parse("let a = 1; let b = 2").unwrap();
    let errs = lang::parse("let a = 1 let b = 2").unwrap_err();
    assert_eq!(
        errs[0].message,
        "expected a line break or `;` after the statement, found `let`"
    );
}

#[test]
fn parse_errors_point_at_the_problem() {
    let src = "rill f(x: sample) sample { return x }";
    let errs = lang::parse(src).unwrap_err();
    assert_eq!(
        errs[0].message,
        "expected `->` and a return type, found `sample`"
    );
    assert_eq!(
        &src[errs[0].span.start as usize..errs[0].span.end as usize],
        "sample"
    );

    assert_eq!(
        lang::parse("let x = a < b < c").unwrap_err()[0].message,
        "comparisons cannot be chained"
    );
    assert_eq!(
        lang::parse("rill f() -> sample {\n return 1\n").unwrap_err()[0].message,
        "this `{` is never closed"
    );
    assert_eq!(
        lang::parse("let x = 1 |> 2").unwrap_err()[0].message,
        "expected a name after `|>`, found `2`"
    );
}

#[test]
fn parser_recovers_at_the_next_definition() {
    let errs = lang::parse(
        "fn a( -> sample { 1 }\nfn b() -> sample { 2 }\nrill c(x: ) -> sample { return x }",
    )
    .unwrap_err();
    assert_eq!(errs.len(), 2, "{errs:#?}");
}

// ---- units --------------------------------------------------------------

#[test]
fn unit_arithmetic() {
    let src = "
        let a = 440Hz * 2
        let b = 440Hz / 2Hz
        let c = 1 / 1kHz
        let d = 2s * 3Hz
        let e = 300ms + 2s
        let f = [1Hz, 2Hz] * 3
        let g = 7st + 50cents
    ";
    assert_eq!(type_of(src, "a"), Type::Hz);
    assert_eq!(type_of(src, "b"), Type::F32);
    assert_eq!(type_of(src, "c"), Type::Time);
    assert_eq!(type_of(src, "d"), Type::F32);
    assert_eq!(type_of(src, "e"), Type::Time);
    assert_eq!(type_of(src, "f"), frame(Type::Hz, 2));
    assert_eq!(type_of(src, "g"), Type::Interval);
}

#[test]
fn units_do_not_mix_with_plain_numbers() {
    let (msg, help) = error("let x = 440Hz + 3");
    assert_eq!(msg, "cannot add `Hz` and `number`");
    assert_eq!(
        help.as_deref(),
        Some("give the number a unit, as in `440Hz`")
    );

    let (msg, help) = error(&format!("{SINE}\nlet x = sine(440)"));
    assert_eq!(
        msg,
        "argument `freq` of `sine` expects `Hz`, found `number`"
    );
    assert_eq!(
        help.as_deref(),
        Some("give the number a unit, as in `440Hz`")
    );

    assert_eq!(error("let x = 1Hz + 1s").0, "cannot add `Hz` and `Time`");
    let (msg, help) = error(&format!("{SINE}\nlet x = sine(1Hz) * 20 + 440Hz"));
    assert_eq!(msg, "cannot add `sample` and `Hz`");
    assert_eq!(
        help.as_deref(),
        Some("`sample` has no unit; multiplying by a `Hz` value gives it one, as in `x * 440Hz`")
    );
    assert_eq!(
        error("let x = 1Hz < 3").0,
        "cannot compare `Hz` and `number` with `<`"
    );
}

// ---- frames and lifting -------------------------------------------------

#[test]
fn rills_lift_over_channels() {
    let src = format!(
        "{SINE}
        let st = [sine(1Hz), sine(2Hz)]
        let p = peak(st)
        let piped = st |> peak
        let both = peak(st, release: 10ms)
        let m = mix_down([sine(1Hz), sine(2Hz), sine(3Hz)])
        let one = st[1]
        let builtin = abs(st)"
    );
    assert_eq!(type_of(&src, "st"), frame(Type::Sample, 2));
    assert_eq!(type_of(&src, "p"), frame(Type::Sample, 2));
    assert_eq!(type_of(&src, "piped"), frame(Type::Sample, 2));
    assert_eq!(type_of(&src, "both"), frame(Type::Sample, 2));
    assert_eq!(type_of(&src, "m"), frame(Type::Sample, 1));
    assert_eq!(type_of(&src, "one"), Type::Sample);
    assert_eq!(type_of(&src, "builtin"), frame(Type::Sample, 2));
}

#[test]
fn lifting_needs_matching_channel_counts() {
    let src = "
        fn mix(a: sample, b: sample) -> sample { a + b }
        let x = mix([1, 2], [3, 4, 5])
    ";
    assert_eq!(
        error(src).0,
        "channel counts differ: this has 3 channels, an earlier argument has 2"
    );
    assert_eq!(
        error("let x = [1, 2] + [1, 2, 3]").0,
        "channel counts differ: `[number; 2]` and `[number; 3]`"
    );
    assert_eq!(
        error("let x = [1, 2][2]").0,
        "channel 2 is out of range for a frame of 2"
    );
    assert_eq!(
        error("let x = [1, 2Hz]").0,
        "frame channels have different types: `number` and `Hz`"
    );
}

#[test]
fn generic_sizes_are_inferred() {
    let src = "
        rill swap<N>(a: [sample; N], b: [sample; N]) -> [sample; N] { return b }
        let x = swap([1, 2], [3, 4])
    ";
    assert_eq!(type_of(src, "x"), frame(Type::Sample, 2));
    let src = "
        rill swap<N>(a: [sample; N], b: [sample; N]) -> [sample; N] { return b }
        let x = swap([1, 2], [3, 4, 5])
    ";
    assert_eq!(
        error(src).0,
        "argument `b` of `swap` expects `[sample; 2]`, found `[number; 3]`"
    );
    assert_eq!(
        error("rill f<N>(x: sample) -> sample { return x }").0,
        "size `N` is not used by any parameter"
    );
    assert_eq!(
        error("rill f(x: [sample; M]) -> sample { return x[0] }").0,
        "unknown size `M`"
    );
}

// ---- rill rules ---------------------------------------------------------

#[test]
fn rills_must_return_on_every_path() {
    let (msg, help) = error("rill f(x: sample) -> sample { x }");
    assert_eq!(msg, "not every path through rill `f` returns");
    assert!(help.unwrap().contains("add `return`"));

    assert_eq!(
        error("rill f(x: sample) -> sample { if x > 0 { return x } }").0,
        "not every path through rill `f` returns"
    );
    assert_ok("rill f(x: sample) -> sample { if x > 0 { return x } else { return -x } }");
    assert_eq!(
        error("rill f(x: sample) -> sample { return x\n return x }").0,
        "unreachable code"
    );
}

#[test]
fn state_rules() {
    let (msg, help) = error("fn f(x: sample) -> sample { state s: sample = 0\n x }");
    assert_eq!(msg, "`state` is only allowed in rills");
    assert!(help.unwrap().contains("make this a rill"));

    assert_eq!(
        error("state s: sample = 0").0,
        "`state` is only allowed in rills"
    );
    assert_eq!(
        error("rill f(x: sample) -> sample { if x > 0 { state s: sample = 0 }\n return x }").0,
        "`state` must be declared at the top level of the rill body"
    );
    assert_eq!(
        error("rill f(x: sample) -> sample { state s: sample = x\n return s }").0,
        "the initial value of `state` must be a constant"
    );
    assert_eq!(
        error("rill f(x: sample) -> sample { let y = x\n y = 1\n return y }").0,
        "cannot assign to `y`"
    );
    assert_eq!(
        error("rill f(x: sample) -> sample { x = 1\n return x }").0,
        "cannot assign to `x`"
    );
    // Untyped state settles on `sample`.
    assert_ok("rill f(x: sample) -> sample { state s = 0\n s = s + x\n return s }");
    assert_ok("rill f<N>(x: [sample; N]) -> sample { state s: f32 = N * 2\n return x[0] + s }");
}

#[test]
fn fns_are_pure() {
    let src = format!("{SINE}\nfn f(x: sample) -> sample {{ peak(x) }}");
    let (msg, help) = error(&src);
    assert_eq!(msg, "fn `f` cannot call rill `peak`");
    assert!(help.unwrap().contains("make `f` a rill"));
}

#[test]
fn fn_bodies_produce_their_return_type() {
    assert_ok("fn f(x: sample) -> sample { x * 2 }");
    assert_ok("fn f(x: sample) -> sample { return x * 2 }");
    assert_eq!(
        error("fn f(x: sample) -> Hz { x * 2 }").0,
        "fn `f` should return `Hz`, but its body produces `sample`"
    );
    assert_eq!(
        error("fn f(x: sample) -> sample { let y = x }").0,
        "fn `f` should return `sample`, but its body produces `()`"
    );
}

#[test]
fn recursion_is_rejected() {
    let errs = errors(
        "fn a(x: sample) -> sample { b(x) }
         fn b(x: sample) -> sample { a(x) }
         rill c(x: sample) -> sample { return c(x) }",
    );
    assert_eq!(
        errs,
        [
            "recursion is not allowed: `a` -> `b` -> `a`",
            "recursion is not allowed: `c` -> `c`",
        ]
    );
}

#[test]
fn bodies_cannot_see_top_level_bindings() {
    let (msg, _) = error("let g = 1\nfn f(x: sample) -> sample { x + g }");
    assert_eq!(msg, "unknown name `g`");
}

// ---- calls and names ----------------------------------------------------

#[test]
fn argument_checking() {
    let p = |rest: &str| format!("{SINE}\n{rest}");
    assert_eq!(
        error(&p("let x = sine()")).0,
        "missing argument `freq` for `sine`"
    );
    assert_eq!(
        error(&p("let x = sine(1Hz, 2Hz)")).0,
        "`sine` takes 1 argument(s), but 2 were given"
    );
    let (msg, help) = error(&p("let x = peak(1, relase: 1s)"));
    assert_eq!(msg, "`peak` has no parameter `relase`");
    assert_eq!(help.as_deref(), Some("did you mean `release`?"));
    assert_eq!(
        error(&p("let x = peak(1, x: 1)")).0,
        "argument `x` is given more than once"
    );
    assert_eq!(
        error(&p("let x = peak(release: 1s, 1)")).0,
        "positional arguments must come before named ones"
    );
    assert_ok(&p("let x = peak(release: 1s, x: 1)"));
    assert_eq!(
        error("let x = min(1, 2, 3)").0,
        "`min` takes 1 or 2 argument(s), but 3 were given"
    );
    assert_eq!(
        error("let x = sin(1Hz)").0,
        "argument `x` of `sin` must be a plain number (`sample`, `f32` or `i32`), found `Hz`"
    );
}

#[test]
fn names_and_suggestions() {
    let (msg, help) = error(&format!("{SINE}\nlet x = sien(1Hz)"));
    assert_eq!(msg, "unknown fn or rill `sien`");
    assert_eq!(help.as_deref(), Some("did you mean `sine`?"));

    let (msg, help) = error("let level = 1\nlet x = levl * 2");
    assert_eq!(msg, "unknown name `levl`");
    assert_eq!(help.as_deref(), Some("did you mean `level`?"));

    assert_eq!(
        error(&format!("{SINE}\nlet x = sine")).0,
        "`sine` is a rill and must be called"
    );
    assert_eq!(
        error("let a = 1\nlet x = a(2)").0,
        "`a` is a value of type `number`, not a fn or rill"
    );
    assert_eq!(
        error("fn f(x: sample) -> sample { x }\nfn f(x: sample) -> sample { x }").0,
        "`f` is defined more than once"
    );
    assert_eq!(error("let x: Sample = 1").0, "unknown type `Sample`");
}

#[test]
fn user_definitions_shadow_builtins() {
    // The design doc defines its own `abs`.
    assert_ok("fn abs(x: sample) -> sample { if x < 0 { -x } else { x } }\nlet y = abs(-1)");
}

#[test]
fn top_level_rules() {
    assert_eq!(
        error("return 1").0,
        "`return` is only allowed in fn and rill bodies"
    );
    assert_eq!(
        error("rill f(x: sample) -> sample { out(x)\n return x }").0,
        "`out` can only be used at the top level"
    );
    assert_eq!(
        error("out(440Hz)").0,
        "argument `x` of `out` must be audio (`sample` or a frame of samples), found `Hz`"
    );
    assert_ok("out([0.1, 0.2])");

    let warnings = diagnostics(&format!("{SINE}\nsine(440Hz)"));
    assert_eq!(warnings.len(), 1);
    assert!(!warnings[0].is_error());
    assert_eq!(warnings[0].message, "this value is never used");
}

#[test]
fn conditions_and_branches() {
    let (msg, help) = error("let x = if 1 { 2 } else { 3 }");
    assert_eq!(msg, "condition must be `bool`, found `number`");
    assert_eq!(help.as_deref(), Some("compare it, as in `x > 0`"));
    assert_eq!(
        error("let x = if true { 1Hz } else { 1s }").0,
        "`if` and `else` have different types: `Hz` and `Time`"
    );
    assert_eq!(
        type_of(
            "let x = if true { 1 } else if false { 2Hz / 1Hz } else { 3 }",
            "x"
        ),
        Type::F32
    );
}

#[test]
fn every_error_is_reported() {
    let errs =
        errors("let a = 1Hz + 1\nlet b = nope\nfn f(x: sample) -> sample { state s = 0\n x }");
    assert_eq!(errs.len(), 3, "{errs:?}");
}

#[test]
fn garbage_never_panics() {
    const PIECES: &[&str] = &[
        "rill", "fn", "state", "let", "return", "if", "else", "f", "x", "N", "sample", "Hz", "[",
        "]", "(", ")", "{", "}", "<", ">", ";", ":", ",", "->", "|>", "@", "rate", "=", "+", "-",
        "*", "/", "==", "&&", "!", "1", "2.5", "440Hz", "3ms", "\n", " ", "sum", "out", "true",
        "x[0]", "// c\n", "/* c */",
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..20_000 {
        let mut src = String::new();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = (state % 24) as usize;
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            src.push_str(PIECES[(state % PIECES.len() as u64) as usize]);
            src.push(' ');
        }
        if let Err(diags) = lang::compile(&src) {
            for d in diags {
                d.render("fuzz.rill", &src);
            }
        }
    }
}
