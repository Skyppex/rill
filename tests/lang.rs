//! Parser and type checker, through the public `rill::lang` API.

use rill::lang::ast::{BinOp, ExprKind, Program, Stmt};
use rill::lang::types::{Size, Type};
use rill::lang::{self, Diagnostic};

const SINE: &str = "
rill sine(freq: Freq) Sample {
    state phase: Float = 0
    phase = wrap(phase + freq / RATE)
    return sin(phase * TAU)
}
rill peak(x: Sample, release: Time = 300ms) Sample {
    state level: Sample = 0
    let a = abs(x)
    level = if a > level { a } else { level * decay(release) }
    return level
}
rill mix_down<N>(x: [Sample; N]) [Sample; 1] {
    return [sum(x) / N]
}
";

/// `stmts` as the body of a `rill main`.
fn body(stmts: &str) -> String {
    format!("rill main() Sample {{\n{stmts}\nreturn 0\n}}")
}

/// [`SINE`] plus `stmts` in a `rill main`.
fn with_sine(stmts: &str) -> String {
    format!("{SINE}\n{}", body(stmts))
}

/// The statements of `stmts` parsed as a rill body.
fn parse_body(stmts: &str) -> Result<Vec<Stmt>, Vec<Diagnostic>> {
    let program = lang::parse(&format!("rill main() Sample {{\n{stmts}\n}}"))?;
    Ok(program.items[0].def().body.stmts.clone())
}

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

/// The `let name = ...` at the top of some definition's body, or of one of
/// its `on` handlers.
fn find_let<'p>(program: &'p Program, name: &str) -> &'p rill::lang::ast::Expr {
    for item in &program.items {
        let handlers = item.def().body.stmts.iter().filter_map(|s| match s {
            Stmt::EventHandler { body, .. } => Some(&body.stmts),
            _ => None,
        });
        for stmts in std::iter::once(&item.def().body.stmts).chain(handlers) {
            for s in stmts {
                if let Stmt::Let { name: n, value, .. } = s
                    && n.name == name
                    && value.is_some()
                {
                    return value.as_ref().unwrap();
                }
            }
        }
    }
    panic!("no `let {name}`");
}

/// Type of `let name = ...` in `src`.
fn type_of(src: &str, name: &str) -> Type {
    let (program, checked) = lang::compile(src).unwrap_or_else(|d| panic!("{d:#?}"));
    checked.types[find_let(&program, name).id as usize].clone()
}

fn frame(t: Type, n: u32) -> Type {
    Type::Frame(Box::new(t), Size::Const(n))
}

/// The errors from checking `src` as a program that starts at `entry`.
fn entry_errors(src: &str, entry: &str) -> Vec<(String, Option<String>)> {
    match lang::compile_entry(src, entry) {
        Ok(_) => Vec::new(),
        Err(diags) => diags.into_iter().map(|d| (d.message, d.help)).collect(),
    }
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
            match lang::compile_entry(&src, "main") {
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
fn design_doc_types() {
    let src = with_sine("let lfo = sine(0.5Hz) * 20Hz + 440Hz\nlet voice = sine(lfo) * 0.3");
    assert_eq!(type_of(&src, "lfo"), Type::Freq);
    assert_eq!(type_of(&src, "voice"), Type::Sample);
}

// ---- parsing ------------------------------------------------------------

#[test]
fn programs_are_only_definitions() {
    let errs = lang::parse("let x = 1").unwrap_err();
    assert_eq!(errs[0].message, "expected `fn` or `rill`, found `let`");
    assert_eq!(
        errs[0].help.as_deref(),
        Some("statements must be inside a rill; the program starts at `rill main`")
    );
}

#[test]
fn pipes_desugar_to_calls() {
    let src = with_sine("let x = sine(1Hz) |> peak(release: 10ms) |> abs");
    let (program, _) = lang::compile(&src).unwrap();
    let value = find_let(&program, "x");
    let ExprKind::Call {
        callee,
        args,
        piped,
        ..
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
    let stmts = parse_body("let x = 1 + 2 * 3 < 4 && true").unwrap();
    let Stmt::Let {
        value: Some(value), ..
    } = &stmts[0]
    else {
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
    assert_eq!(parse_body("let a = 1\n-2").unwrap().len(), 2);
    // ...but not inside brackets, and `|>` always continues.
    let stmts = parse_body("let a = (1\n- 2)\nlet b = a\n  |> f\n  |> g").unwrap();
    assert_eq!(stmts.len(), 2);
    // `else` may start a line.
    parse_body("let a = if true { 1 }\nelse { 2 }").unwrap();
    // Two statements on one line need a `;`.
    parse_body("let a = 1; let b = 2").unwrap();
    let errs = parse_body("let a = 1 let b = 2").unwrap_err();
    assert_eq!(
        errs[0].message,
        "expected a line break or `;` after the statement, found `let`"
    );
}

#[test]
fn parse_errors_point_at_the_problem() {
    let src = "rill f(x: Sample) { return x }";
    let errs = lang::parse(src).unwrap_err();
    assert_eq!(errs[0].message, "expected a return type, found `{`");
    assert_eq!(
        &src[errs[0].span.start as usize..errs[0].span.end as usize],
        "{"
    );

    let src = "rill f(x: Sample) -> Sample { return x }";
    let errs = lang::parse(src).unwrap_err();
    assert_eq!(
        errs[0].message,
        "return types come right after the parameters, without `->`"
    );
    assert_eq!(
        &src[errs[0].span.start as usize..errs[0].span.end as usize],
        "->"
    );
    assert_eq!(
        lang::parse("rill main(f: fn(Pitch) -> Freq) Sample { return 0 }").unwrap_err()[0].message,
        "return types come right after the parameters, without `->`"
    );

    assert_eq!(
        parse_body("let x = a < b < c").unwrap_err()[0].message,
        "comparisons cannot be chained"
    );
    assert_eq!(
        lang::parse("rill f() Sample {\n return 1\n").unwrap_err()[0].message,
        "this `{` is never closed"
    );
    assert_eq!(
        parse_body("let x = 1 |> 2").unwrap_err()[0].message,
        "expected a name after `|>`, found `2`"
    );
}

#[test]
fn parser_recovers_at_the_next_definition() {
    let errs =
        lang::parse("fn a( -> Sample { 1 }\nfn b() Sample { 2 }\nrill c(x: ) Sample { return x }")
            .unwrap_err();
    assert_eq!(errs.len(), 2, "{errs:#?}");
}

// ---- units --------------------------------------------------------------

#[test]
fn unit_arithmetic() {
    let src = body(
        "
        let a = 440Hz * 2
        let b = 440Hz / 2Hz
        let c = 1 / 1kHz
        let d = 2s * 3Hz
        let e = 300ms + 2s
        let f = [1Hz, 2Hz] * 3
        let g = 7st + 50cents
    ",
    );
    assert_eq!(type_of(&src, "a"), Type::Freq);
    assert_eq!(type_of(&src, "b"), Type::Float);
    assert_eq!(type_of(&src, "c"), Type::Time);
    assert_eq!(type_of(&src, "d"), Type::Float);
    assert_eq!(type_of(&src, "e"), Type::Time);
    assert_eq!(type_of(&src, "f"), frame(Type::Freq, 2));
    assert_eq!(type_of(&src, "g"), Type::Interval);
}

#[test]
fn units_do_not_mix_with_plain_numbers() {
    let (msg, help) = error(&body("let x = 440Hz + 3"));
    assert_eq!(msg, "cannot add `Freq` and `number`");
    assert_eq!(
        help.as_deref(),
        Some("give the number a unit, as in `440Hz`")
    );

    let (msg, help) = error(&with_sine("let x = sine(440)"));
    assert_eq!(
        msg,
        "argument `freq` of `sine` expects `Freq`, found `number`"
    );
    assert_eq!(
        help.as_deref(),
        Some("give the number a unit, as in `440Hz`")
    );

    assert_eq!(
        error(&body("let x = 1Hz + 1s")).0,
        "cannot add `Freq` and `Time`"
    );
    let (msg, help) = error(&with_sine("let x = sine(1Hz) * 20 + 440Hz"));
    assert_eq!(msg, "cannot add `Sample` and `Freq`");
    assert_eq!(
        help.as_deref(),
        Some("`Sample` has no unit; multiplying by a `Freq` value gives it one, as in `x * 440Hz`")
    );
    assert_eq!(
        error(&body("let x = 1Hz < 3")).0,
        "cannot compare `Freq` and `number` with `<`"
    );
}

// ---- frames and lifting -------------------------------------------------

#[test]
fn rills_lift_over_channels() {
    let src = with_sine(
        "
        let st = [sine(1Hz), sine(2Hz)]
        let p = peak(st)
        let piped = st |> peak
        let both = peak(st, release: 10ms)
        let m = mix_down([sine(1Hz), sine(2Hz), sine(3Hz)])
        let one = st[1]
        let scaled = st * 2",
    );
    assert_eq!(type_of(&src, "st"), frame(Type::Sample, 2));
    assert_eq!(type_of(&src, "p"), frame(Type::Sample, 2));
    assert_eq!(type_of(&src, "piped"), frame(Type::Sample, 2));
    assert_eq!(type_of(&src, "both"), frame(Type::Sample, 2));
    assert_eq!(type_of(&src, "m"), frame(Type::Sample, 1));
    assert_eq!(type_of(&src, "one"), Type::Sample);
    assert_eq!(type_of(&src, "scaled"), frame(Type::Sample, 2));
}

#[test]
fn fns_and_builtins_do_not_lift() {
    let src = "
        fn double(x: Sample) Sample { x * 2 }
        rill main() Sample { return double([1, 2])[0] }
    ";
    let (msg, help) = error(src);
    assert_eq!(msg, "`double` takes one value, not a frame (`[number; 2]`)");
    assert!(help.unwrap().contains("fn double<N>(x: [Sample; N])"));

    let (msg, help) = error(&body("let x = abs([1, -2])"));
    assert_eq!(msg, "`abs` takes one value, not a frame (`[number; 2]`)");
    assert!(
        help.unwrap()
            .starts_with("built-in functions take one value")
    );

    // Saying so with a size parameter works.
    let src = "
        fn double<N>(x: [Sample; N]) [Sample; N] { x * 2 }
        rill main() Sample {
            let d = double([1, 2])
            return d[0]
        }
    ";
    assert_eq!(type_of(src, "d"), frame(Type::Sample, 2));
}

#[test]
fn lifting_needs_matching_channel_counts() {
    let src = "
        rill mix(a: Sample, b: Sample) Sample { return a + b }
        rill main() Sample { return mix([1, 2], [3, 4, 5])[0] }
    ";
    assert_eq!(
        error(src).0,
        "channel counts differ: this has 3 channels, another argument has 2"
    );
    assert_eq!(
        error(&body("let x = [1, 2] + [1, 2, 3]")).0,
        "channel counts differ: `[number; 2]` and `[number; 3]`"
    );
    assert_eq!(
        error(&body("let x = [1, 2][2]")).0,
        "channel 2 is out of range for a frame of 2"
    );
    assert_eq!(
        error(&body("let x = [1, 2Hz]")).0,
        "frame channels have different types: `number` and `Freq`"
    );
}

#[test]
fn generic_sizes_are_inferred() {
    let swap = "rill swap<N>(a: [Sample; N], b: [Sample; N]) [Sample; N] { return b }";
    let src = format!("{swap}\n{}", body("let x = swap([1, 2], [3, 4])"));
    assert_eq!(type_of(&src, "x"), frame(Type::Sample, 2));
    let src = format!("{swap}\n{}", body("let x = swap([1, 2], [3, 4, 5])"));
    assert_eq!(
        error(&src).0,
        "argument `b` of `swap` expects `[Sample; 2]`, found `[number; 3]`"
    );
    assert_eq!(
        error("rill f<N>(x: Sample) Sample { return x }").0,
        "size `N` is not used"
    );
    assert_eq!(
        error("rill f(x: [Sample; M]) Sample { return x[0] }").0,
        "unknown size `M`"
    );
}

#[test]
fn for_loops_iterate_ranges_and_frames() {
    let src = body(
        "
        let acc = 0
        for x in [1, 2, 3] {
            acc += x
        }
        let total = acc
    ",
    );
    assert_eq!(type_of(&src, "total"), Type::Num);

    let src = "
        rill freqs<N>() [Freq; N] {
            let xs: [Freq; N]
            for i in 0..=N - 1 {
                xs[i] = 440Hz
            }
            return xs
        }
        rill main() Sample {
            let xs = freqs<2>()
            return 0
        }
    ";
    assert_eq!(type_of(src, "xs"), frame(Type::Freq, 2));

    assert_eq!(
        error(&body("for x in 1 { let y = x }")).0,
        "cannot loop over `number`"
    );
}

// ---- rill rules ---------------------------------------------------------

#[test]
fn rills_must_return_on_every_path() {
    let (msg, help) = error("rill f(x: Sample) Sample { x }");
    assert_eq!(msg, "not every path through rill `f` returns");
    assert!(help.unwrap().contains("add `return`"));

    assert_eq!(
        error("rill f(x: Sample) Sample { if x > 0 { return x } }").0,
        "not every path through rill `f` returns"
    );
    assert_ok("rill f(x: Sample) Sample { if x > 0 { return x } else { return -x } }");
    assert_eq!(
        error("rill f(x: Sample) Sample { return x\n return x }").0,
        "unreachable code"
    );
}

#[test]
fn state_rules() {
    let (msg, help) = error("fn f(x: Sample) Sample { state s: Sample = 0\n x }");
    assert_eq!(msg, "`state` is only allowed in rills");
    assert!(help.unwrap().contains("make this a rill"));

    assert_eq!(
        error("rill f(x: Sample) Sample { if x > 0 { state s: Sample = 0 }\n return x }").0,
        "`state` must be declared at the top level of the rill body"
    );
    assert_eq!(
        error("rill f(x: Sample) Sample { state s: Sample = x\n return s }").0,
        "the initial value of `state` must be a constant"
    );
    assert_ok("rill f(x: Sample) Sample { let y = x\n y = 1\n return y }");
    assert_eq!(
        error("rill f(x: Sample) Sample { x = 1\n return x }").0,
        "cannot assign to `x`"
    );
    // Untyped state settles on `sample`.
    assert_ok("rill f(x: Sample) Sample { state s = 0\n s = s + x\n return s }");
    assert_ok("rill f<N>(x: [Sample; N]) Sample { state s: Float = N * 2\n return x[0] + s }");
}

#[test]
fn fns_are_pure() {
    let src = format!("{SINE}\nfn f(x: Sample) Sample {{ peak(x) }}");
    let (msg, help) = error(&src);
    assert_eq!(msg, "fn `f` cannot call rill `peak`");
    assert!(help.unwrap().contains("make `f` a rill"));
}

#[test]
fn fn_bodies_produce_their_return_type() {
    assert_ok("fn f(x: Sample) Sample { x * 2 }");
    assert_ok("fn f(x: Sample) Sample { return x * 2 }");
    assert_eq!(
        error("fn f(x: Sample) Freq { x * 2 }").0,
        "fn `f` should return `Freq`, but its body produces `Sample`"
    );
    assert_eq!(
        error("fn f(x: Sample) Sample { let y = x }").0,
        "fn `f` should return `Sample`, but its body produces `()`"
    );
}

#[test]
fn recursion_is_rejected() {
    let errs = errors(
        "fn a(x: Sample) Sample { b(x) }
         fn b(x: Sample) Sample { a(x) }
         rill c(x: Sample) Sample { return c(x) }",
    );
    assert_eq!(
        errs,
        [
            "recursion is not allowed: `a` -> `b` -> `a`",
            "recursion is not allowed: `c` -> `c`",
        ]
    );
}

// ---- the entry rill -----------------------------------------------------

#[test]
fn entry_rill_rules() {
    assert!(
        entry_errors(
            "rill main(gain: Sample = 0.5) Sample { return gain }",
            "main"
        )
        .is_empty()
    );
    assert!(entry_errors("rill main() [Sample; 2] { return [0, 0] }", "main").is_empty());

    assert_eq!(
        entry_errors("fn f(x: Sample) Sample { x }", "main"),
        [(
            "there is no rill named `main` to run".to_owned(),
            Some("add one, as in `rill main() Sample { return 0 }`".to_owned())
        )]
    );
    assert_eq!(
        entry_errors("rill mian() Sample { return 0 }", "main")[0]
            .1
            .as_deref(),
        Some("did you mean `mian`?")
    );
    assert_eq!(
        entry_errors(
            "rill a() Sample { return 0 }\nrill b() Sample { return 0 }",
            "main"
        )[0]
        .1
        .as_deref(),
        Some("pick one with `--entry`: a, b")
    );
    assert!(entry_errors("rill other() Sample { return 0 }", "other").is_empty());

    let messages = |src: &str| -> Vec<String> {
        entry_errors(src, "main")
            .into_iter()
            .map(|(m, _)| m)
            .collect()
    };
    assert_eq!(
        messages("fn main() Sample { 0 }"),
        ["`main` is a fn, but the program must start at a rill"]
    );
    assert_eq!(
        messages("rill main<N>(x: [Sample; N]) Sample { return 0 }"),
        [
            "the entry rill cannot have size parameters",
            "`x` needs a default value"
        ]
    );
    assert_eq!(
        messages("rill main(freq: Freq) Sample { return 0 }"),
        ["`freq` needs a default value"]
    );
    assert_eq!(
        messages("rill main() Sample @ rate / 2 { return 0 }"),
        ["the entry rill cannot change the sample rate"]
    );
    assert_eq!(
        messages("rill main() Freq { return 1Hz }"),
        ["the entry rill must return audio (`Sample` or `[Sample; N]`), found `Freq`"]
    );
}

// ---- calls and names ----------------------------------------------------

#[test]
fn argument_checking() {
    assert_eq!(
        error(&with_sine("let x = sine()")).0,
        "missing argument `freq` for `sine`"
    );
    assert_eq!(
        error(&with_sine("let x = sine(1Hz, 2Hz)")).0,
        "`sine` takes 1 argument(s), but 2 were given"
    );
    let (msg, help) = error(&with_sine("let x = peak(1, relase: 1s)"));
    assert_eq!(msg, "`peak` has no parameter `relase`");
    assert_eq!(help.as_deref(), Some("did you mean `release`?"));
    assert_eq!(
        error(&with_sine("let x = peak(1, x: 1)")).0,
        "argument `x` is given more than once"
    );
    assert_eq!(
        error(&with_sine("let x = peak(release: 1s, 1)")).0,
        "positional arguments must come before named ones"
    );
    assert_ok(&with_sine("let x = peak(release: 1s, x: 1)"));
    assert_eq!(
        error(&body("let x = min(1, 2, 3)")).0,
        "`min` takes 1 or 2 argument(s), but 3 were given"
    );
    assert_eq!(
        error(&body("let x = sin(1Hz)")).0,
        "argument `x` of `sin` must be a plain number (`Sample`, `Float` or `Int`), found `Freq`"
    );
}

#[test]
fn names_and_suggestions() {
    let (msg, help) = error(&with_sine("let x = sien(1Hz)"));
    assert_eq!(msg, "unknown fn or rill `sien`");
    assert_eq!(help.as_deref(), Some("did you mean `sine`?"));

    let (msg, help) = error(&body("let level = 1\nlet x = levl * 2"));
    assert_eq!(msg, "unknown name `levl`");
    assert_eq!(help.as_deref(), Some("did you mean `level`?"));

    assert_eq!(
        error(&with_sine("let x = sine")).0,
        "rill `sine` cannot be used as a value"
    );
    assert_eq!(
        error(&body("let a = 1\nlet x = a(2)")).0,
        "`a` is a value of type `number`, not a fn or rill"
    );
    assert_eq!(
        error("fn f(x: Sample) Sample { x }\nfn f(x: Sample) Sample { x }").0,
        "`f` is defined more than once"
    );
    let (msg, help) = error(&body("let x: bool = true"));
    assert_eq!(msg, "unknown type `bool`");
    assert_eq!(help.as_deref(), Some("`bool` is now called `Bool`"));
    let (msg, help) = error(&body("let x: sample = 1"));
    assert_eq!(msg, "unknown type `sample`");
    assert_eq!(help.as_deref(), Some("`sample` is now called `Sample`"));
    let (msg, help) = error(&body("let x: Hz = 1Hz"));
    assert_eq!(msg, "unknown type `Hz`");
    assert_eq!(
        help.as_deref(),
        Some("`Hz` is the unit for literals like `440Hz`; the type is `Freq`")
    );
    let (msg, help) = error(&body("let x = f32(1)"));
    assert_eq!(msg, "unknown fn or rill `f32`");
    assert_eq!(
        help.as_deref(),
        Some("convert with `as`, as in `x as Float`")
    );
}

#[test]
fn user_definitions_shadow_builtins() {
    // The design doc defines its own `abs`.
    assert_ok(&format!(
        "fn abs(x: Sample) Sample {{ if x < 0 {{ -x }} else {{ x }} }}\n{}",
        body("let y = abs(-1)")
    ));
}

#[test]
fn unused_values_warn() {
    let warnings = diagnostics(&with_sine("sine(440Hz)"));
    assert_eq!(warnings.len(), 1);
    assert!(!warnings[0].is_error());
    assert_eq!(warnings[0].message, "this value is never used");
}

#[test]
fn conditions_and_branches() {
    let (msg, help) = error(&body("let x = if 1 { 2 } else { 3 }"));
    assert_eq!(msg, "condition must be `Bool`, found `number`");
    assert_eq!(help.as_deref(), Some("compare it, as in `x > 0`"));
    assert_eq!(
        error(&body("let x = if true { 1Hz } else { 1s }")).0,
        "`if` and `else` have different types: `Freq` and `Time`"
    );
    assert_eq!(
        type_of(
            &body("let x = if true { 1 } else if false { 2Hz / 1Hz } else { 3 }"),
            "x"
        ),
        Type::Float
    );
}

#[test]
fn every_error_is_reported() {
    let src = format!(
        "{}\nfn f(x: Sample) Sample {{ state s = 0\n x }}",
        body("let a = 1Hz + 1\nlet b = nope")
    );
    assert_eq!(errors(&src).len(), 3, "{:?}", errors(&src));
}

#[test]
fn garbage_never_panics() {
    const PIECES: &[&str] = &[
        "rill", "fn", "state", "let", "return", "if", "else", "f", "x", "N", "sample", "Hz", "[",
        "]", "(", ")", "{", "}", "<", ">", ";", ":", ",", "->", "|>", "@", "rate", "=", "+", "-",
        "*", "/", "==", "&&", "!", "1", "2.5", "440Hz", "3ms", "\n", " ", "sum", "main", "true",
        "x[0]", "// c\n", "/* c */", "seq", "invoke", "trigger", "halt", "claim", "release", "on",
        "event", "start", "_", "@", "120bpm", "note_on", "note_off", "1/8", "s", "tail:",
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..60_000 {
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
        if let Err(diags) = lang::compile_entry(&src, "main") {
            for d in diags {
                d.render("fuzz.rill", &src);
            }
        }
    }
}

// ---- pitch --------------------------------------------------------------

#[test]
fn pitches_are_positions_not_amounts() {
    assert_eq!(type_of(&body("let i = E4 - C4"), "i"), Type::Interval);
    assert_eq!(type_of(&body("let p = C4 + 4st"), "p"), Type::Pitch);
    let (m, help) = error(&body("let p = 7st + C4"));
    assert_eq!(m, "cannot add `Interval` and `Pitch`");
    assert_eq!(
        help.as_deref(),
        Some("the interval comes after the pitch, as in `C4 + 7st`")
    );
    assert_eq!(type_of(&body("let p = C4 - 50cents"), "p"), Type::Pitch);
    assert_eq!(
        type_of(&body("let chord = [C4, E4, G4]"), "chord"),
        frame(Type::Pitch, 3)
    );
    assert_eq!(type_of(&body("let low = min(C4, G3)"), "low"), Type::Pitch);
    assert_eq!(type_of(&body("let up = C4 < E4"), "up"), Type::Bool);

    for (src, msg) in [
        ("let x = A4 * 2", "cannot multiply `Pitch` and `number`"),
        ("let x = A4 + A4", "cannot add `Pitch` and `Pitch`"),
        ("let x = A4 / C4", "cannot divide `Pitch` and `Pitch`"),
        ("let x = 4st - C4", "cannot subtract `Interval` and `Pitch`"),
    ] {
        let (m, help) = error(&body(src));
        assert_eq!(m, msg, "{src}");
        assert!(help.unwrap().starts_with("a pitch is a position"), "{src}");
    }
    assert_eq!(error(&body("let x = -A4")).0, "cannot negate `Pitch`");

    let (m, help) = error(&body("let x = A4 == 69"));
    assert_eq!(m, "cannot compare `Pitch` and `number` with `==`");
    assert_eq!(
        help.as_deref(),
        Some("write a pitch as a note name, like `A4` or `F#3`")
    );
}

#[test]
fn tunings_are_functions() {
    assert_eq!(type_of(&body("let f = E4 |> equal(24)"), "f"), Type::Freq);
    assert_eq!(
        type_of(&body("let chord = [C4, E4, G4] |> just(C)"), "chord"),
        frame(Type::Freq, 3)
    );
    // Settings may vary while playing.
    assert_ok("rill main(a4: Freq = 440Hz) Sample {\n return (A4 |> equal(a4: a4)) / 1kHz\n}");

    let (msg, help) = error(&body("let t = equal(12)"));
    assert_eq!(
        msg,
        "argument `pitch` of `equal` expects `Pitch`, found `number`"
    );
    assert_eq!(
        help.as_deref(),
        Some("write a pitch as a note name, like `A4` or `F#3`")
    );
}

#[test]
fn functions_are_values() {
    let defs = "
        fn a432(p: Pitch) Freq { 432Hz * pow(2, (p - A4) / 12st) }
        rill voice(pitch: Pitch, tune: fn(Pitch) Freq) Sample {
            return (pitch |> tune) / 1kHz
        }
        fn tuned(steps: Int) fn(Pitch) Freq { fn(p) { equal(p, steps) } }
    ";
    let with = |stmts: &str| format!("{SINE}\n{defs}\n{}", body(stmts));

    assert_eq!(
        type_of(&with("let t = fn(p: Pitch) Freq { equal(p) }"), "t"),
        Type::Fn(vec![Type::Pitch], Box::new(Type::Freq))
    );
    assert_eq!(
        type_of(&with("let t = tuned(24)"), "t"),
        Type::Fn(vec![Type::Pitch], Box::new(Type::Freq))
    );
    assert_ok(&with(
        "let a = voice(E4, a432)
         let b = voice(E4, fn(p) { equal(p, 24) })
         let c = voice(E4, equal)
         let t: fn(Pitch) Freq = equal
         let d = E4 |> t
         let f: fn(Sample) Sample = sin
         let e = f(0.5)",
    ));

    for (stmts, msg) in [
        ("let f = fn(p) { p }", "cannot tell the type of `p`"),
        (
            "let a = voice(E4, sine)",
            "rill `sine` cannot be used as a value",
        ),
        ("let f = sin", "cannot tell which `sin` is meant here"),
        (
            "let a = voice(E4, fn(p) { p })",
            "this fn should return `Freq`, but its body produces `Pitch`",
        ),
        (
            "let f = fn(x: Sample) Sample { sine(1Hz) }",
            "an anonymous fn cannot call rill `sine`",
        ),
        ("state f = a432", "a function cannot be `state`"),
        (
            "let t = fn(p: Pitch) Freq { equal(p) }\nlet x = t(A4, B4)",
            "`t` takes 1 argument(s), but 2 were given",
        ),
        (
            "let a = voice(E4, decay)",
            "`decay` does not fit `fn(Pitch) Freq`",
        ),
    ] {
        assert_eq!(error(&with(stmts)).0, msg, "{stmts}");
    }

    let src = format!(
        "{SINE}\n{}",
        "rill main() Sample {
            state s: Sample = 0
            let f = fn(x: Sample) Sample {
                s = x
                x
            }
            return f(1)
        }"
    );
    assert_eq!(error(&src).0, "an anonymous fn cannot change `s`");

    assert_eq!(
        entry_errors(
            "rill main(t: fn(Pitch) Freq = equal) Sample { return 0 }",
            "main"
        )[0]
        .0,
        "`t` cannot be a function"
    );
}

#[test]
fn recursion_through_function_values_is_rejected() {
    let errs = errors(
        "fn apply(f: fn(Sample) Sample, x: Sample) Sample { f(x) }
         fn spin(x: Sample) Sample { apply(spin, x) }",
    );
    assert_eq!(errs, ["recursion is not allowed: `spin` -> `spin`"]);
}

// ---- levels -------------------------------------------------------------

#[test]
fn levels_move_signals_up_and_down() {
    let t = |stmts: &str, name: &str| type_of(&with_sine(stmts), name);
    assert_eq!(t("let v = sine(1Hz) - 6dB", "v"), Type::Sample);
    assert_eq!(t("let v = sine(1Hz) + 3dB", "v"), Type::Sample);
    assert_eq!(
        t("let v = [sine(1Hz), sine(2Hz)] - [3dB, 6dB]", "v"),
        frame(Type::Sample, 2)
    );
    assert_eq!(t("let g = -6dB - 3dB", "g"), Type::Gain);
    assert_eq!(t("let g = -6dB * 0.5", "g"), Type::Gain);
    assert_eq!(t("let g = 0.5 * -6dB", "g"), Type::Gain);
    assert_eq!(t("let r = -12dB / -6dB", "r"), Type::Float);
    assert_eq!(t("let l = level(sine(1Hz))", "l"), Type::Gain);
    assert_eq!(t("let factor = amp(-6dB)", "factor"), Type::Float);
    assert_eq!(
        t("let quiet = level(sine(1Hz)) < -20dB", "quiet"),
        Type::Bool
    );
    // A plain number is an amplitude factor, so it can be used as a gain.
    assert_ok(&with_sine("let g: Gain = 0.5\nlet v = sine(1Hz) - g"));

    for (stmts, msg, help) in [
        (
            "let v = -6dB + sine(1Hz)",
            "cannot add `Gain` and `Sample`",
            "the level comes after the signal, as in `voice - 6dB`",
        ),
        (
            "let v = sine(1Hz) * -6dB",
            "cannot multiply `Sample` and `Gain`",
            "to change a signal's level, add or subtract it, as in `voice - 6dB`",
        ),
        (
            "let v = 3 as Int - 6dB",
            "cannot subtract `Int` and `Gain`",
            "integers have no level; convert with `as Float` first",
        ),
        (
            "let q = level(sine(1Hz)) < 0.5",
            "cannot compare `Gain` and `number` with `<`",
            "write the level in dB, as in `-6dB`",
        ),
    ] {
        let (m, h) = error(&with_sine(stmts));
        assert_eq!(m, msg, "{stmts}");
        assert_eq!(h.as_deref(), Some(help), "{stmts}");
    }
    // Without a unit it is ordinary subtraction, not a level change.
    assert_eq!(
        type_of(&with_sine("let v = sine(1Hz) - 0.5"), "v"),
        Type::Sample
    );
}

#[test]
fn named_fns_only_at_the_top_level() {
    let errs = parse_body("let f = fn double(x: Sample) Sample { x * 2 }").unwrap_err();
    assert_eq!(
        errs[0].message,
        "a fn with a name can only be defined at the top level"
    );
    assert_eq!(
        errs[0].help.as_deref(),
        Some("leave the name out for an anonymous fn, as in `let double = fn(x) { ... }`")
    );
}

#[test]
fn casts_with_as() {
    let t = |stmts: &str, name: &str| type_of(&with_sine(stmts), name);
    // `as` binds tighter than `*`: this casts only the 10.
    assert_eq!(
        error(&with_sine("let i = sine(1Hz) * 10 as Int")).0,
        "cannot multiply `Sample` and `Int`"
    );
    assert_eq!(t("let i = (sine(1Hz) * 10) as Int", "i"), Type::Int);
    assert_eq!(t("let f = 3 as Int as Float", "f"), Type::Float);
    assert_eq!(t("let s = 2.5 as Sample", "s"), Type::Sample);
    // A leading `-` binds tighter: this is `(-x) as Int`.
    let stmts = parse_body("let i = -x as Int").unwrap();
    let Stmt::Let {
        value: Some(value), ..
    } = &stmts[0]
    else {
        panic!()
    };
    let ExprKind::Cast(inner, _) = &value.kind else {
        panic!("{value:?}")
    };
    assert!(matches!(inner.kind, ExprKind::Unary(..)));

    for (stmts, msg, help) in [
        (
            "let f = 440Hz as Float",
            "cannot cast `Freq` to `Float`",
            Some("units never disappear on their own; divide by one, as in `x / 1Hz`"),
        ),
        (
            "let f = -6dB as Float",
            "cannot cast `Gain` to `Float`",
            Some("use `amp(x)` for the amplitude factor of a level"),
        ),
        (
            "let f = 1 as Freq",
            "cannot cast to `Freq`",
            Some("`as` converts between `Sample`, `Float` and `Int`"),
        ),
        (
            "let f = Float(1)",
            "unknown fn or rill `Float`",
            Some("convert with `as`, as in `x as Float`"),
        ),
    ] {
        let (m, h) = error(&with_sine(stmts));
        assert_eq!(m, msg, "{stmts}");
        assert_eq!(h.as_deref(), help, "{stmts}");
    }
}

// ---- nested frames ------------------------------------------------------

const VOICES: &str = "
rill drive(x: Sample) Sample { return tanh(x * 2) }
rill widen(x: [Sample; 2]) [Sample; 2] { return [x[0], x[1] * 0.5] }
rill pan(x: Sample, pos: Float = 0.5) [Sample; 2] { return [x * (1 - pos), x * pos] }
rill gain(x: [Sample; 2], g: Float) [Sample; 2] { return x * g }
";

/// [`VOICES`] plus `stmts` in a `rill main`, with `buses` (four stereo
/// buses) and `mono` (three voices) in scope.
fn with_voices(stmts: &str) -> String {
    format!(
        "{VOICES}\n{}",
        body(&format!(
            "let s = sin(0.1) as Sample
             let mono = [s, s, s]
             let buses: [[Sample; 2]; 4] = [[s, s], [s, s], [s, s], [s, s]]
             {stmts}"
        ))
    )
}

#[test]
fn frames_nest() {
    let stereo = || frame(Type::Sample, 2);
    let src = with_voices("let a = buses[1]\nlet b = buses[1][0]\nlet c = [[1, 2], [3, 4]]");
    assert_eq!(type_of(&src, "buses"), frame(stereo(), 4));
    assert_eq!(type_of(&src, "a"), stereo());
    assert_eq!(type_of(&src, "b"), Type::Sample);
    assert_eq!(type_of(&src, "c"), frame(frame(Type::Num, 2), 2));

    assert_eq!(
        error(&body("let x = [[1, 2], [1, 2, 3]]")).0,
        "frame elements have different shapes: `[number; 2]` and `[number; 3]`"
    );
    assert_eq!(
        errors(&body("let f: [fn(Sample) Sample; 2] = [sin, cos]"))[0],
        "frames cannot hold functions"
    );
}

#[test]
fn lifting_peels_layers_until_the_argument_fits() {
    let stereo = || frame(Type::Sample, 2);
    let src = with_voices(
        "let a = drive(buses)
         let b = widen(buses)
         let c = pan(mono)
         let d = pan(mono, [0.1, 0.5, 0.9])
         let e = gain(buses, [1, 0.5, 0.5, 1])
         let f = mono |> pan |> widen |> drive",
    );
    assert_eq!(type_of(&src, "a"), frame(stereo(), 4));
    assert_eq!(type_of(&src, "b"), frame(stereo(), 4));
    assert_eq!(type_of(&src, "c"), frame(stereo(), 3));
    assert_eq!(type_of(&src, "d"), frame(stereo(), 3));
    assert_eq!(type_of(&src, "e"), frame(stereo(), 4));
    assert_eq!(type_of(&src, "f"), frame(stereo(), 3));

    // A rill returning a frame lifts over two layers too.
    let src = with_voices("let g = pan(buses)");
    assert_eq!(type_of(&src, "g"), frame(frame(stereo(), 2), 4));

    // Tunings lift at any depth.
    let src = body("let f = [[C4, E4], [D4, F4]] |> equal");
    assert_eq!(type_of(&src, "f"), frame(frame(Type::Freq, 2), 2));
}

#[test]
fn lifted_arguments_line_up_with_either_end() {
    let stereo = || frame(Type::Sample, 2);
    let src = with_voices(
        "let a = pan(buses, [0.1, 0.2, 0.3, 0.4])
         let b = pan(buses, [0.1, 0.9])
         let c = pan([0.1, 0.9], pos: [mono, mono])",
    );
    // One position per bus (outer), or one per channel of every bus
    // (inner).
    assert_eq!(type_of(&src, "a"), frame(frame(stereo(), 2), 4));
    assert_eq!(type_of(&src, "b"), frame(frame(stereo(), 2), 4));
    assert_eq!(type_of(&src, "c"), frame(frame(stereo(), 3), 2));

    assert_eq!(
        error(&with_voices("let x = pan(buses, [0.1, 0.2, 0.3])")),
        (
            "this runs `pan` over shape `3`, another argument over shape `4 × 2`".into(),
            Some(
                "a value with fewer layers must match the outer or the inner layers of the other"
                    .into()
            )
        )
    );
    assert_eq!(
        error(&with_voices(
            "let sq: [[Sample; 2]; 2] = [[s, s], [s, s]]\nlet x = pan(sq, [0.1, 0.9])"
        )),
        (
            "shape `2` could line up with the outer or the inner layers of shape `2 × 2`".into(),
            Some(
                "say which: to reuse it for every outer element, repeat it, as in `[x; 2]`; for \
                 one value per outer element, build the full shape"
                    .into()
            )
        )
    );
    assert_eq!(
        error(&with_voices("let x = gain(buses, [1, 2])")).0,
        "channel counts differ: this has 2 channels, another argument has 4"
    );
    // Fns still take exactly what they declare.
    let src = format!(
        "fn first<N>(x: [Sample; N]) Sample {{ x[0] }}\n{}",
        with_voices("let x = first(buses)")
    );
    assert_eq!(
        error(&src).0,
        "`first` takes `[Sample; 2]`, not `[[Sample; 2]; 4]`"
    );
}

#[test]
fn operators_line_up_with_either_end() {
    let stereo = || frame(Type::Sample, 2);
    let src = with_voices(
        "let a = buses * 0.5
         let b = buses + buses
         let c = buses * [0.5, 1, 1, 0.2]
         let d = -buses
         let e = buses - 6dB
         let f = pan(mono) + [0dB, -3dB, -6dB]",
    );
    for name in ["a", "b", "c", "d", "e"] {
        assert_eq!(type_of(&src, name), frame(stereo(), 4), "{name}");
    }
    assert_eq!(type_of(&src, "f"), frame(stereo(), 3));

    // A frame matching the inner layers applies to every bus.
    let src = with_voices("let g = buses * [1, 0.5]\nlet h = buses - [0dB, -6dB]");
    assert_eq!(type_of(&src, "g"), frame(stereo(), 4));
    assert_eq!(type_of(&src, "h"), frame(stereo(), 4));

    assert_eq!(
        error(&with_voices("let x = buses * [1, 2, 3]")).0,
        "shapes do not line up: `[[Sample; 2]; 4]` and `[number; 3]`"
    );
    assert_eq!(
        error(&with_voices(
            "let sq: [[Sample; 2]; 2] = [[s, s], [s, s]]\nlet x = sq * [1, 2]"
        ))
        .0,
        "`[number; 2]` could line up with the outer or the inner layers of `[[Sample; 2]; 2]`"
    );
    assert_eq!(
        error(&with_voices("let x = buses * [[1, 2], [3, 4]]")).0,
        "channel counts differ: `[[Sample; 2]; 4]` and `[[number; 2]; 2]`"
    );
}

#[test]
fn reductions_take_the_outer_layer_off() {
    let stereo = || frame(Type::Sample, 2);
    let src = with_voices(
        "let a = sum(buses)\nlet b = max(buses)\nlet c = min(buses)\nlet d = sum(mono)",
    );
    for name in ["a", "b", "c"] {
        assert_eq!(type_of(&src, name), stereo(), "{name}");
    }
    assert_eq!(type_of(&src, "d"), Type::Sample);
}

#[test]
fn the_entry_rill_stays_flat() {
    let errs = entry_errors(
        "rill main(x: [[Sample; 2]; 2] = [[0, 0], [0, 0]]) [[Sample; 2]; 2] { return x }",
        "main",
    );
    assert_eq!(
        errs,
        [
            (
                "`x` cannot be a frame of frames".to_owned(),
                Some("the entry rill's parameters are live controls; use a flat frame".to_owned())
            ),
            (
                "the entry rill must return flat audio (`Sample` or `[Sample; N]`), found `[[Sample; 2]; 2]`"
                    .to_owned(),
                Some("mix the outer layer down with `sum`, as in `return sum(voices)`".to_owned())
            ),
        ]
    );
}

// ---- events -------------------------------------------------------------

/// `decls` at the top, then a rill with `handlers` in its body.
fn with_events(decls: &str, handlers: &str) -> String {
    format!("{decls}\nrill main() Sample {{\n{handlers}\nreturn 0\n}}")
}

#[test]
fn event_declarations() {
    assert_ok(&with_events(
        "event keys note_on(sender: 5, channel: 1)\nevent all note_off\nevent cc control_change(channel: 11);",
        "on keys(n) { }\non all { }\non cc(v) { }",
    ));

    for (decls, msg, help) in [
        (
            "event keys note_onn",
            "unknown event kind `note_onn`",
            Some("did you mean `note_on`?"),
        ),
        (
            "event keys note_on(device: 1)",
            "unknown filter `device`",
            Some("events can be filtered by `sender` and `channel`"),
        ),
        (
            "event keys note_on(channel: 1, channel: 2)",
            "filter `channel` is given more than once",
            None,
        ),
        (
            "event keys note_on(channel: 1.5)",
            "a filter is a whole number ≥ 0",
            Some("filters are fixed when the program is built, as in `channel: 1`"),
        ),
        (
            "event keys note_on(channel: -1)",
            "a filter is a whole number ≥ 0",
            Some("filters are fixed when the program is built, as in `channel: 1`"),
        ),
        (
            "event keys note_on\nevent keys note_off",
            "event `keys` is declared more than once",
            None,
        ),
        (
            "event main note_on",
            "`main` is already the name of a fn or rill",
            None,
        ),
        (
            "event note_on(note)",
            "expected the event's kind (`note_on`, `note_off` or `control_change`) after its name, found `(`",
            Some("give it a name and a kind, as in `event keys note_on(channel: 1)`"),
        ),
    ] {
        let src = with_events(decls, "on keys { }");
        let errs: Vec<Diagnostic> = diagnostics(&src)
            .into_iter()
            .filter(|d| d.is_error())
            .collect();
        assert_eq!(errs[0].message, msg, "{decls}");
        assert_eq!(errs[0].help.as_deref(), help, "{decls}");
    }
}

#[test]
fn event_handlers() {
    let decls = "event keys note_on\nevent lifts note_off\nevent cc control_change";
    let all = "on keys { }\non lifts { }\non cc { }";
    for (handler, msg, help) in [
        (
            "on key(n) { }",
            "unknown event `key`".to_owned(),
            Some("did you mean `keys`?".to_owned()),
        ),
        (
            "on note_on(n) { }",
            "`note_on` is a kind of event, not a declared event".to_owned(),
            Some("declare one at the top level and handle it by name: `event keys note_on(channel: 1)`, then `on keys(...)`".to_owned()),
        ),
        (
            "on keys(a, b) { }",
            "an event handler takes one parameter: the event".to_owned(),
            Some("read its fields, as in `note.pitch`".to_owned()),
        ),
        (
            "on keys(n) { let r = n.release }",
            "a `NoteOn` has no field `release`".to_owned(),
            Some("`release` belongs to `note_off` events; a `NoteOn` has `pitch`, `velocity` and `instance`".to_owned()),
        ),
        (
            "on lifts(n) { let v = n.velocity }",
            "a `NoteOff` has no field `velocity`".to_owned(),
            Some("`velocity` belongs to `note_on` events; a `NoteOff` has `pitch`, `release` and `instance`".to_owned()),
        ),
        (
            "on cc(v) { let x = v.value }",
            "cannot read fields from `Float`".to_owned(),
            None,
        ),
    ] {
        let (m, h) = error(&with_events(decls, &format!("{all}\n{handler}")));
        assert_eq!(m, msg, "{handler}");
        assert_eq!(h, help, "{handler}");
    }

    // Payload types come from the kind, whatever the parameter is called.
    let src = with_events(
        decls,
        "state p: Pitch = A4\nstate x: Float = 0
         on keys(anything) { p = anything.pitch; x = anything.velocity }
         on lifts(n) { x = n.release }
         on cc(v) { x = v }",
    );
    assert_ok(&src);

    // A declaration nothing handles is a warning.
    let warnings: Vec<String> = diagnostics(&with_events(decls, "on keys { }\non cc { }"))
        .into_iter()
        .map(|d| d.message)
        .collect();
    assert_eq!(warnings, ["event `lifts` is declared but never handled"]);
}

// ---- sequences ----------------------------------------------------------

/// `decls` at the top, then a rill whose handlers include `handler`.
fn with_seq(decls: &str, handler: &str) -> String {
    format!(
        "seq riff(step: 1/8) {{ C4, _, E4@0.5, [G4, B4] }}
        event pad note_on(sender: 1)
        event lead note_on(sender: riff)
        {decls}
        rill main() Sample {{
            on pad(hit) {{ {handler} }}
            on lead {{ }}
            return 0
        }}"
    )
}

#[test]
fn sequences_check() {
    assert_ok(&with_seq("", "invoke riff"));
    assert_ok(&with_seq(
        "seq beat(meter: 7/8, step: 3/16, tempo: 140bpm, gate: 1, velocity: 0.5, repeat: 2, loop: false, instances: 4) { C2 + 7st, _ }\nevent b note_on(sender: beat)",
        "let id = invoke 3 riff(tempo: 90bpm * hit.velocity, repeat: 4)\ntrigger 2 id beat\nhalt riff\nhalt id beat\ninvoke lead(pitch: C4, velocity: 1)",
    ));
    // `invoke` gives the instance id, an `Int`.
    let src = with_seq("", "let id = invoke riff\nlet x: Int = id + 1");
    assert_ok(&src);
    assert_eq!(
        error(&with_seq("", "let x: Float = invoke riff")).0,
        "`x` expects `Float`, found `Int`"
    );

    for (decls, msg, help) in [
        ("seq s(swing: 1) { C4 }", "unknown setting `swing`", None),
        (
            "seq s(meter: 4) { C4 }",
            "`meter` is a time signature, written as two whole numbers",
            Some("as in `meter: 4/4`"),
        ),
        (
            "seq s(step: 0.125) { C4 }",
            "`step` is a note value, written as two whole numbers",
            Some("as in `step: 1/8`"),
        ),
        ("seq s(tempo: 0bpm) { C4 }", "a tempo is above 0bpm", None),
        (
            "seq s(gate: 1.5) { C4 }",
            "`gate` is a fraction of the step, above 0 and at most 1",
            None,
        ),
        (
            "seq s(velocity: 2) { C4 }",
            "a velocity is from 0 to 1",
            None,
        ),
        (
            "seq s(repeat: 0) { C4 }",
            "this is a whole number, at least 1",
            None,
        ),
        (
            "seq s(tempo: 120Hz, tempo: 1Hz) { C4 }",
            "setting `tempo` is given more than once",
            None,
        ),
        (
            "seq s { 440Hz }",
            "a step is a pitch, a chord or `_`, not `Freq`",
            Some("as in `C4`, `[C4, E4, G4]` or `_` for a rest"),
        ),
        ("seq s { C4@1.5 }", "a velocity is from 0 to 1", None),
        ("seq s { _@0.5 }", "a rest has no velocity", None),
        (
            "seq s { }",
            "sequence `s` has no steps",
            Some("add steps between the braces, as in `{ C4, _, E4 }`"),
        ),
        (
            "seq riff { C4 }",
            "sequence `riff` is declared more than once",
            None,
        ),
        (
            "event riff note_on",
            "`riff` is already the name of a sequence",
            None,
        ),
        (
            "event x note_on(sender: rif)",
            "unknown sequence `rif`",
            Some("a sender is a whole number from the host, or a sequence's name"),
        ),
    ] {
        let errs: Vec<Diagnostic> = diagnostics(&with_seq(decls, "invoke riff"))
            .into_iter()
            .filter(|d| d.is_error())
            .collect();
        assert!(!errs.is_empty(), "{decls}: no error");
        assert_eq!(errs[0].message, msg, "{decls}");
        if let Some(help) = help {
            assert_eq!(errs[0].help.as_deref(), Some(help), "{decls}");
        }
    }

    // A sequence nothing invokes is a warning.
    let warnings: Vec<String> = diagnostics(&with_seq("seq idle { C4 }", "invoke riff"))
        .into_iter()
        .map(|d| d.message)
        .collect();
    assert_eq!(warnings, ["sequence `idle` is never invoked"]);
}

#[test]
fn invoking() {
    for (handler, msg, help) in [
        (
            "invoke rif",
            "unknown sequence `rif`",
            Some("did you mean `riff`?"),
        ),
        (
            "invoke riff(meter: 3/4)",
            "`meter` is fixed when `riff` is declared",
            Some(
                "only `tempo`, `gate`, `velocity`, `repeat` and `loop` can be given when invoking",
            ),
        ),
        ("invoke riff(swing: 1)", "unknown setting `swing`", None),
        (
            "invoke riff(90bpm)",
            "settings are given by name",
            Some("as in `invoke riff(tempo: 90bpm)`"),
        ),
        ("invoke riff(tempo: 0bpm)", "a tempo is above 0bpm", None),
        (
            "invoke riff(tempo: 2s)",
            "setting `tempo` expects `Freq`, found `Time`",
            None,
        ),
        (
            "invoke 0 riff",
            "an id you pick is a whole number ≥ 1",
            Some("0 marks notes that did not come from a sequence, and fresh ids are negative"),
        ),
        (
            "trigger 5 riff",
            "step 5 is outside `riff`, which has 4 steps",
            Some("steps count from 1"),
        ),
        (
            "trigger 0 riff",
            "step 0 is outside `riff`, which has 4 steps",
            Some("steps count from 1"),
        ),
        (
            "trigger 1 lead",
            "`lead` is an event; only sequences have steps and instances",
            Some("send it with `invoke lead(...)`"),
        ),
        (
            "invoke lead(release: 1)",
            "a note_on event has no field `release`",
            Some("it has `pitch`, `velocity`, `instance`"),
        ),
        (
            "invoke lead(pitch: 440Hz)",
            "field `pitch` expects `Pitch`, found `Freq`",
            None,
        ),
        (
            "invoke start",
            "`start` cannot be invoked",
            Some("it runs once, before the first sample"),
        ),
        ("halt pad", "unknown sequence `pad`", None),
    ] {
        let (m, h) = error(&with_seq("", handler));
        assert_eq!(m, msg, "{handler}");
        if let Some(help) = help {
            assert_eq!(h.as_deref(), Some(help), "{handler}");
        }
    }

    // Only in handlers.
    let src = "seq s { C4 }\nrill main() Sample { let id = invoke s\nreturn 0 }";
    let (m, h) = error(src);
    assert_eq!(m, "`invoke` only works inside an `on` handler");
    assert!(h.unwrap().starts_with("a rill body runs every sample"));
}

#[test]
fn invoke_loops_are_rejected() {
    // A riff note starting the riff again would never end.
    let (m, h) = error(
        "seq riff { C4 }
         event lead note_on(sender: riff)
         rill main() Sample {
             on start { invoke riff }
             on lead { invoke riff }
             return 0
         }",
    );
    assert_eq!(
        m,
        "invoking here can lead back to the same handler: `lead` -> `riff` -> `lead`"
    );
    assert!(h.unwrap().contains("same sample"));
    // And through events.
    let (m, _) = error(
        "event a control_change(channel: 1)
         event b control_change(channel: 2)
         rill main() Sample {
             on a { invoke b(value: 1) }
             on b { invoke a(value: 1) }
             return 0
         }",
    );
    assert!(
        m.starts_with("invoking here can lead back to the same handler"),
        "{m}"
    );
}

#[test]
fn start_and_voice_handlers() {
    let src = "event k note_on\nevent o note_off\nrill main() Sample {\n{h}\non k { }\non o { }\nreturn 0\n}";
    for (h, msg) in [
        ("on start(x) { }", "`start` has no payload"),
        (
            "on start claim { }",
            "`on start` runs in every copy; it cannot `claim` or `release`",
        ),
        ("on o claim { }", "only a `note_on` event can be claimed"),
        (
            "on k release { }",
            "only a `note_off` event can release a voice",
        ),
        (
            "on k claim(tail: 1Hz) { }",
            "`tail` expects `Time`, found `Freq`",
        ),
        (
            "on k claim(size: 1s) { }",
            "`claim` takes only `tail`, not `size`",
        ),
    ] {
        let errs: Vec<String> = errors(&src.replace("{h}", h));
        assert_eq!(errs.first().map(String::as_str), Some(msg), "{h}: {errs:?}");
    }
    assert_ok(&src.replace(
        "{h}",
        "on start { }\non k claim(tail: 2s) { }\non o release { }",
    ));
    assert_eq!(
        errors("event start note_on\nrill main() Sample { return 0 }")[0],
        "`start` is a built-in event"
    );
}

#[test]
fn repeated_frames() {
    let src = "rill v() Sample { return 0 }\nrill main() Sample { let a = [v(); 3]\nlet b = [[1, 2]; 2]\nreturn 0 }";
    assert_eq!(type_of(src, "a"), frame(Type::Sample, 3));
    assert_eq!(type_of(src, "b"), frame(frame(Type::Num, 2), 2));
}

#[test]
fn each_makes_an_argument_per_copy() {
    let stereo = || frame(Type::Sample, 2);
    let src = with_voices(
        "let a = pan(mono, pos: each random())
         let b = buses |> pan(each random(lo: 0.2, hi: 0.8))
         let each = 0.5
         let c = pan(s, each)
         let d = pan(s, pos: each)",
    );
    assert_eq!(type_of(&src, "a"), frame(stereo(), 3));
    assert_eq!(type_of(&src, "b"), frame(frame(stereo(), 2), 4));
    assert_eq!(type_of(&src, "c"), stereo());
    assert_eq!(type_of(&src, "d"), stereo());

    assert_eq!(
        error(&with_voices("let x = pan(s, pos: each random())")).0,
        "`each` needs copies, but `pan` runs once here"
    );
    assert_eq!(
        error(&with_voices("let x = sin(each random())")).0,
        "`each` only works on rills, and `sin` is a built-in function"
    );
    assert_eq!(
        error(&with_voices(
            "let f = fn(x: Float) Float { x }\nlet x = f(each 1)"
        ))
        .0,
        "`each` needs copies, but `f` is a function value, which never runs per element"
    );
    // The value is made per copy, so it is never taken apart itself.
    assert_eq!(
        error(&with_voices("let x = pan(mono, pos: each [0.1, 0.2, 0.3])")).0,
        "argument `pos` of `pan` expects `Float`, found `[number; 3]`"
    );
}

#[test]
fn random_picks_a_number_of_any_type() {
    let src = body(
        "let a = random()
         let b = random(100Hz, 200Hz)
         let c = random(lo: -6dB, hi: 0dB)
         let d = random(C4, C5)",
    );
    assert_eq!(type_of(&src, "a"), Type::Float);
    assert_eq!(type_of(&src, "b"), Type::Freq);
    assert_eq!(type_of(&src, "c"), Type::Gain);
    assert_eq!(type_of(&src, "d"), Type::Pitch);
    assert!(!errors(&body("let x = random(1Hz, 2s)")).is_empty());
    // It is known before audio starts, so it can be a default.
    let src = "
        rill tone(offset: Float = random()) Sample { return offset }
        rill main() Sample { return tone() }
    ";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
}

const RIFF: &str = "seq riff(meter: 3/4, step: 1/8, instances: 8) { C4, _, E4, _, G4, _ }";

/// [`RIFF`] and a `main` with `body`, invoking the riff at the start.
fn with_riff(body: &str) -> String {
    format!("{RIFF}\nrill main() Sample {{\non start {{ invoke riff }}\n{body}\nreturn 0\n}}")
}

#[test]
fn sequences_make_their_own_events() {
    let src = with_riff(
        "on riff_note_on(n) { let a = n.pitch }
         on riff_note_off(n) { let b = n.release }
         on riff_start(s) { let c = s.step }
         on riff_finished(f) { let d = f.step }
         on riff_halted(h) { let e = h.instance }
         on riff_replaced(r) { let f = r.step }
         on riff_end(e) { let g = e.step }
         on riff_repeated(r) { let h = r.pass }
         on riff_step(s) { let i = s.pass }
         on riff_rest(r) { let j = r.step }
         on riff_beat(b) { let k = b.bar }
         on riff_bar(b) { let l = b.bar }",
    );
    assert!(errors(&src).is_empty(), "{:?}", errors(&src));
    assert_eq!(type_of(&src, "a"), Type::Pitch);
    assert_eq!(type_of(&src, "b"), Type::Float);
    for name in ["c", "d", "e", "f", "g", "h", "i", "j", "k", "l"] {
        assert_eq!(type_of(&src, name), Type::Int, "{name}");
    }
    // Fields that belong to another kind are pointed out.
    let (m, h) = error(&with_riff("on riff_step(s) { let x = s.beat }"));
    assert_eq!(m, "a `SeqStep` has no field `beat`");
    assert!(h.unwrap().contains("`instance`, `step` and `pass`"));
}

#[test]
fn a_sequences_events_cannot_be_declared_or_invoked() {
    let (m, h) = error(&format!(
        "{RIFF}\nevent riff_step note_on\n{}",
        body("on riff_step { }")
    ));
    assert_eq!(m, "sequence `riff` already makes an event `riff_step`");
    assert!(h.unwrap().contains("rename this one"));
    assert_eq!(
        error(&with_riff("on riff_step { invoke riff_start(step: 1) }")).0,
        "`riff_start` is made by sequence `riff`, so it cannot be invoked"
    );
    assert_eq!(
        error("event x seq_step\nrill main() Sample { return 0 }").0,
        "unknown event kind `seq_step`"
    );
    // A fn or rill with one of the names.
    let src = format!(
        "{RIFF}\nrill riff_end() Sample {{ return 0 }}\n{}",
        with_riff("").replace(RIFF, "")
    );
    assert_eq!(
        error(&src).0,
        "sequence `riff` makes event `riff_end`, which is already the name of a fn or rill"
    );
}

#[test]
fn restarting_from_a_sequences_events() {
    // Later events are fine: finishing, repeating, ending (an instance is
    // never replaced in the sample it started).
    for h in [
        "on riff_finished { invoke riff }",
        "on riff_repeated { trigger 1 3 riff }",
        "on riff_end { invoke riff }",
        "on riff_step { halt riff }",
    ] {
        assert!(
            errors(&with_riff(h)).is_empty(),
            "{h}: {:?}",
            errors(&with_riff(h))
        );
    }
    // Starting again from what starting does would never end.
    for (h, chain) in [
        (
            "on riff_start { invoke riff }",
            "`riff_start` -> `riff` -> `riff_start`",
        ),
        (
            "on riff_beat { invoke riff }",
            "`riff_beat` -> `riff` -> `riff_beat`",
        ),
        // A restart halts, and the halt ends the instance at once.
        (
            "on riff_end { trigger 1 3 riff }",
            "`riff_end` -> `riff` -> `riff_end`",
        ),
        (
            "on riff_halted { halt riff }",
            "`riff_halted` -> `riff` -> `riff_halted`",
        ),
    ] {
        let (m, _) = error(&with_riff(h));
        assert_eq!(
            m,
            format!("invoking here can lead back to the same handler: {chain}"),
            "{h}"
        );
    }
}

#[test]
fn sequence_fields() {
    let src = format!(
        "{RIFF}\n{}",
        body(
            "let a = riff.step_count
             let b = riff.beats_per_bar
             let c = riff.beat_unit
             let d = riff.step_size
             let e = riff.steps_per_beat
             let f = riff.beat_count
             let g = riff.bar_count
             let h = riff.instances"
        )
    );
    for (name, ty) in [
        ("a", Type::Int),
        ("b", Type::Int),
        ("c", Type::Int),
        ("d", Type::Float),
        ("e", Type::Float),
        ("f", Type::Float),
        ("g", Type::Float),
        ("h", Type::Int),
    ] {
        assert_eq!(type_of(&src, name), ty, "{name}");
    }
    let (program, checked) = lang::compile(&src).unwrap();
    let value = |name| checked.const_values[&find_let(&program, name).id];
    assert_eq!(
        ["a", "b", "c", "d", "e", "f", "g", "h"].map(value),
        [6.0, 3.0, 4.0, 0.125, 2.0, 3.0, 1.0, 8.0]
    );

    // Constants: usable as defaults and state initialisers.
    let src = format!(
        "{RIFF}\nrill f(n: Int = riff.step_count) Sample {{ state s: Float = riff.bar_count\nreturn s }}\n{}",
        body("let x = f()")
    );
    assert!(errors(&src).is_empty(), "{:?}", errors(&src));

    let (m, h) = error(&format!("{RIFF}\n{}", body("let x = riff.step_cuont")));
    assert_eq!(m, "sequence `riff` has no field `step_cuont`");
    assert_eq!(h.unwrap(), "did you mean `step_count`?");
    let (m, h) = error(&format!("{RIFF}\n{}", body("let x = riff.tempo")));
    assert_eq!(m, "sequence `riff` has no field `tempo`");
    assert!(h.unwrap().starts_with("a sequence has `step_count`"));
    // A local of the same name hides the sequence.
    assert!(
        !errors(&format!(
            "{RIFF}\n{}",
            body("let riff = 1\nlet x = riff.step_count")
        ))
        .is_empty()
    );
}

#[test]
fn constant_sizes() {
    let src = format!(
        "{RIFF}
        fn first<N>(x: [Float; N]) Float {{ x[0] }}
        {}",
        body(
            "let a: [Float; riff.step_count] = [1, 2, 3, 4, 5, 6]
             let b = [0.5; riff.step_count * 2]
             let c: [Float; 6] = a
             let d = first<riff.step_count>(a)
             let e = [1; 7 % 4 + (2.9 as Int)]"
        )
    );
    assert!(errors(&src).is_empty(), "{:?}", errors(&src));
    assert_eq!(type_of(&src, "b"), frame(Type::Num, 12));
    assert_eq!(type_of(&src, "e"), frame(Type::Num, 5));

    // Inside a generic rill, `[x; N]` takes the size parameter.
    let src = "rill copies<N>(x: Sample) [Sample; N] { return [x; N] }\nrill main() [Sample; 3] { return copies<3>(1) }";
    assert!(errors(src).is_empty(), "{:?}", errors(src));

    for (size, msg) in [
        (
            "2.5",
            "a size must be a whole number of at least 1; this is 2.5",
        ),
        (
            "riff.step_count - 6",
            "a size must be a whole number of at least 1; this is 0",
        ),
        ("RATE", "`RATE` cannot be a size"),
        ("sin(1)", "a size must be a constant whole number"),
    ] {
        assert_eq!(
            error(&format!(
                "{RIFF}\n{}",
                body(&format!("let x = [1; {size}]"))
            ))
            .0,
            msg,
            "{size}"
        );
    }
    assert_eq!(
        error("rill f<N>(x: [Sample; N * 2]) Sample { return 0 }\nrill main() Sample { return 0 }")
            .0,
        "a size cannot be worked out from size parameter `N` yet"
    );
}
