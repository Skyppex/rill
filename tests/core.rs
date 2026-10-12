//! The core library: built in, imported with `@`, and checked like any
//! other Rill.

use rill::lang::{self, core};
use rill::offline::{self, Blocks};
use rill::{Config, Engine};

const RATE: f32 = 48_000.0;

fn render(src: &str, frames: usize) -> Vec<f32> {
    let config = Config {
        sample_rate: RATE as u32,
        max_frames: 256,
        out_channels: 1,
    };
    let options = lang::build::Options {
        seed: Some(0),
        ..Default::default()
    };
    let (graph, _) = lang::load_with(src, &config, "main", &options).unwrap_or_else(|d| {
        panic!(
            "{}",
            d.iter()
                .map(|d| d.render("test.rill", src))
                .collect::<String>()
        )
    });
    let mut engine = Engine::new(graph, config).unwrap();
    offline::render(&mut engine, frames, &Blocks::Fixed(64))
}

/// `main` returning `expr`, with `@gen/osc` imported.
fn osc(expr: &str) -> String {
    format!("import \"@gen/osc\"\nrill main() Sample {{\n    return {expr}\n}}")
}

#[test]
fn every_core_module_checks_cleanly() {
    assert!(!core::FILES.is_empty());
    for (name, src) in core::FILES {
        let (_, checked) = lang::compile(src).unwrap_or_else(|d| {
            panic!(
                "@{name}: {}",
                d.iter().map(|d| d.render(name, src)).collect::<String>()
            )
        });
        assert!(
            checked.warnings.is_empty(),
            "@{name}: {:?}",
            checked.warnings
        );
    }
}

#[test]
fn core_is_there_without_any_files() {
    // A program from a string has no folder, but core is built in.
    let out = render(&osc("sine(1kHz)"), 4);
    assert_eq!(out[0], 0.0);
    assert!((out[1] - (std::f32::consts::TAU / 48.0).sin()).abs() < 1e-5);
    // Anything else needs a file.
    let errs = lang::compile("import \"osc\"\nrill main() Sample { return 0 }").unwrap_err();
    assert_eq!(errs[0].message, "imports need a file to look next to");
}

#[test]
fn unknown_core_modules() {
    let errs = lang::compile("import \"@gen/oscs\"\nrill main() Sample { return 0 }").unwrap_err();
    assert_eq!(errs[0].message, "there is no core module \"@gen/oscs\"");
    assert_eq!(
        errs[0].help.as_deref(),
        Some("did you mean \"@gen/osc\"? `rill core` lists them all")
    );
}

#[test]
fn a_phasor_counts_cycles() {
    let out = render(&osc("phasor(12kHz)"), 6);
    assert_eq!(out, [0.0, 0.25, 0.5, 0.75, 0.0, 0.25]);
    let out = render(&osc("phasor(12kHz, offset: 0.5)"), 3);
    assert_eq!(out, [0.5, 0.75, 0.0]);
}

#[test]
fn warp_moves_the_middle_of_the_cycle() {
    let w = |p: f32, width: f32| render(&osc(&format!("warp({p}, {width})")), 1)[0];
    assert_eq!(w(0.3, 0.5), 0.3);
    assert_eq!(w(0.25, 0.25), 0.5);
    assert_eq!(w(0.125, 0.25), 0.25);
    assert!((w(0.625, 0.25) - 0.75).abs() < 1e-6);
    // Out of range widths are kept inside the cycle.
    assert!(w(0.5, 2.0) < 1.0);
}

#[test]
fn shapes_play_the_phase_they_are_given() {
    // The same as the plain oscillators.
    assert_eq!(
        render(&osc("phasor(110Hz) |> as_saw()"), 512),
        render(&osc("saw(110Hz)"), 512)
    );
    assert_eq!(
        render(&osc("phasor(110Hz) |> as_sine()"), 512),
        render(&osc("sine(110Hz)"), 512)
    );
    assert_eq!(
        render(&osc("phasor(110Hz) |> as_square()"), 512),
        render(&osc("square(110Hz)"), 512)
    );
}

#[test]
fn warp_gives_a_square_its_pulse_width() {
    // At 100Hz, a cycle is 480 samples; a width of 0.25 keeps it high for
    // a quarter of them.
    let out = render(&osc("phasor(100Hz) |> warp(0.25) |> as_square()"), 4800);
    let high = out.iter().filter(|&&x| x > 0.0).count();
    assert!((high as f32 / 4800.0 - 0.25).abs() < 0.01, "{high}");
}

/// The biggest change from one sample to the next.
fn biggest_step(out: &[f32]) -> f32 {
    out.windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn jumps_are_band_limited() {
    // A naive saw jumps by 2 in one sample; the corrected one spreads it.
    let naive = render(&osc("phasor(2kHz) * 2 - 1"), 2000);
    assert!(biggest_step(&naive) > 1.9);
    for shape in ["saw(2kHz)", "square(2kHz)"] {
        let out = render(&osc(shape), 2000);
        assert!(biggest_step(&out) < 1.3, "{shape}: {}", biggest_step(&out));
    }
    // Still true when the phase is warped by a width that moves every
    // sample: the shapes measure the step they are given.
    let moving = "phasor(2kHz) |> warp(phasor(3Hz) * 0.6 + 0.2) |> as_saw()";
    assert!(biggest_step(&render(&osc(moving), 4000)) < 1.3);
}

#[test]
fn copies_keep_their_own_phase() {
    // Two saws at once are two phases; an offset starts one later.
    let out = render(&osc("saw(1kHz) - saw(1kHz, offset: 0.5)"), 48);
    assert!(out.iter().any(|&x| x.abs() > 0.5));
    let same = render(&osc("saw(1kHz) - saw(1kHz)"), 48);
    assert!(same.iter().all(|&x| x == 0.0));
}
