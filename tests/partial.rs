//! The front end as tools such as editors use it: everything it can tell
//! about a broken program, and what every name refers to.

use rill::lang::ast::{Item, Program, Stmt};
use rill::lang::check::{BindingKind, Checked, Resolution, check_partial};
use rill::lang::diag::Span;
use rill::lang::types::Type;
use rill::lang::{lexer, parser};

fn analyze(src: &str) -> (Program, Checked, Vec<String>) {
    let (tokens, mut errors) = lexer::lex_partial(src);
    let (program, parse_errors) = parser::parse_partial(src, tokens);
    errors.extend(parse_errors);
    let (checked, check_errors) = check_partial(&program);
    errors.extend(check_errors);
    let messages = errors.into_iter().map(|e| e.message).collect();
    (program, checked, messages)
}

fn names(program: &Program) -> Vec<&str> {
    program
        .items
        .iter()
        .map(|i| i.def().name.name.as_str())
        .collect()
}

/// Span of the `n`th occurrence of `needle`.
fn nth(src: &str, needle: &str, n: usize) -> Span {
    let at = src
        .match_indices(needle)
        .nth(n)
        .unwrap_or_else(|| panic!("no occurrence {n} of {needle:?}"))
        .0;
    Span::new(at, at + needle.len())
}

fn resolution_at(checked: &Checked, span: Span) -> &Resolution {
    checked
        .resolutions
        .iter()
        .find(|(s, _)| *s == span)
        .map(|(_, r)| r)
        .unwrap_or_else(|| panic!("nothing resolved at {span:?}"))
}

#[test]
fn strict_and_partial_agree_on_valid_programs() {
    for entry in std::fs::read_dir("examples").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rill") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let strict = rill::lang::parse(&src).unwrap();
        let (program, checked, errors) = analyze(&src);
        assert_eq!(program, strict, "{}", path.display());
        assert!(errors.is_empty(), "{}: {errors:?}", path.display());
        let expected = rill::lang::check::check(&strict).unwrap();
        assert_eq!(checked.types, expected.types);
    }
}

#[test]
fn a_bad_statement_does_not_lose_its_def() {
    let src = "
rill f(x: Sample) Sample {
    let a = x * 2
    let b = (a +
    let c = a
    return c
}
rill g() Sample { return 0 }
";
    let (program, checked, errors) = analyze(src);
    assert_eq!(names(&program), ["f", "g"]);
    assert_eq!(errors.len(), 1, "{errors:?}");
    let Item::Rill(f) = &program.items[0] else {
        panic!()
    };
    // `let b` is dropped; the rest survives.
    assert_eq!(f.body.stmts.len(), 3);
    assert!(matches!(&f.body.stmts[1], Stmt::Let { name, .. } if name.name == "c"));
    let c = checked.bindings.iter().find(|b| b.name == "c").unwrap();
    assert_eq!(c.ty, Type::Sample);
}

#[test]
fn unclosed_blocks_end_at_the_next_definition() {
    let src = "
rill f(x: Sample) Sample {
    let a = x
    return a

rill g() Sample { return 0 }
";
    let (program, _, errors) = analyze(src);
    assert_eq!(names(&program), ["f", "g"]);
    assert_eq!(errors, ["this `{` is never closed"]);

    // And at the end of the file, mid-statement.
    let (program, checked, _) =
        analyze("rill f(x: Sample) Sample {\n    let a = x\n    return a |> ");
    assert_eq!(names(&program), ["f"]);
    assert!(checked.bindings.iter().any(|b| b.name == "a"));
}

#[test]
fn lexer_errors_do_not_stop_the_parse() {
    let (program, _, errors) =
        analyze("rill f() Sample { return 1hz }\nrill g() Sample { return 0 }");
    assert_eq!(names(&program), ["f", "g"]);
    assert_eq!(errors[0], "unknown unit `hz`");
}

#[test]
fn checking_goes_on_after_errors() {
    let src = "rill f(x: Sample) Sample {\n    let a = nope\n    let b = x * 2\n    return b\n}";
    let (_, checked, errors) = analyze(src);
    assert_eq!(errors, ["unknown name `nope`"]);
    let b = checked.bindings.iter().find(|b| b.name == "b").unwrap();
    assert_eq!(b.ty, Type::Sample);
}

#[test]
fn bindings_and_their_scopes() {
    let src = "rill f<N>(x: [Sample; N], g: Gain = 0dB) Sample {\n    state s: Sample = 0\n    let a = sum(x)\n    if a > 0 {\n        let inner = a\n        s = inner\n    }\n    let h = fn(v: Sample) Sample { v * 2 }\n    return h(a + s)\n}";
    let (_, checked, errors) = analyze(src);
    assert!(errors.is_empty(), "{errors:?}");
    let get = |name: &str| checked.bindings.iter().find(|b| b.name == name).unwrap();

    assert_eq!(get("N").kind, BindingKind::Size);
    assert_eq!(get("x").kind, BindingKind::Param);
    assert_eq!(get("g").ty, Type::Gain);
    assert_eq!(get("s").kind, BindingKind::State);
    assert_eq!(get("a").kind, BindingKind::Let);
    assert_eq!(get("v").kind, BindingKind::FnParam);
    for b in &checked.bindings {
        assert_eq!(b.def, 0);
        assert_eq!(&src[b.span.start as usize..b.span.end as usize], b.name);
    }

    // `inner` is visible from the end of its `let` to the end of the `if` body.
    let inner = get("inner");
    let let_end = src.find("let inner = a").unwrap() + "let inner = a".len();
    assert_eq!(inner.scope.start as usize, let_end);
    let close = src[let_end..].find('}').unwrap() + let_end + 1;
    assert_eq!(inner.scope.end as usize, close);
    // Parameters see the whole body.
    assert_eq!(get("x").scope.end as usize, src.len());
}

#[test]
fn every_kind_of_name_resolves() {
    let src = "
fn half(x: Sample) Sample { x / 2 }
rill f<N>(xs: [Sample; N], freq: Freq = 440Hz) Sample {
    state level: Sample = 0
    level = half(sum(xs))
    let t = equal(A4, a4: freq) / RATE
    let k = f2(y: level)
    return k + t * PI
}
rill f2(y: Sample) Sample { return y |> half }
";
    let (program, checked, errors) = analyze(src);
    assert!(errors.is_empty(), "{errors:?}");
    let binding = |name: &str| {
        checked
            .bindings
            .iter()
            .position(|b| b.name == name)
            .unwrap()
    };
    let r = |needle: &str, n: usize| resolution_at(&checked, nth(src, needle, n)).clone();

    assert_eq!(r("Sample", 0), Resolution::Type);
    assert_eq!(r("N", 1), Resolution::Binding(binding("N")));
    assert_eq!(r("level", 1), Resolution::Binding(binding("level")));
    assert_eq!(r("half", 1), Resolution::Def(0));
    assert_eq!(r("sum", 0), Resolution::Builtin("sum".into()));
    assert_eq!(r("A4", 0), Resolution::Note);
    // A named argument resolves to the parameter it names; built-ins have
    // no bindings for theirs.
    let a4 = nth(src, "a4", 0);
    assert!(checked.resolutions.iter().all(|(s, _)| *s != a4));
    assert_eq!(r("RATE", 0), Resolution::Constant("RATE".into()));
    assert_eq!(r("y", 0), Resolution::Binding(binding("y")));
    assert_eq!(r("f2", 0), Resolution::Def(2));
    assert_eq!(r("half", 2), Resolution::Def(0));
    assert_eq!(names(&program), ["half", "f", "f2"]);
}
