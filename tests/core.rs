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

// ---- the intrinsics -------------------------------------------------------

/// `main` returning `expr`, with the core modules `imports` imported.
fn with(imports: &[&str], expr: &str) -> String {
    let imports: String = imports
        .iter()
        .map(|m| format!("import \"{m}\"\n"))
        .collect();
    format!("{imports}rill main() Sample {{\n    return {expr}\n}}")
}

/// A one on the first sample, then zeros.
const IMPULSE: &str = "
rill impulse() Sample {
    state first: Bool = true
    let y = if first { 1 } else { 0 }
    first = false
    return y
}
";

fn rms(out: &[f32]) -> f32 {
    (out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt()
}

/// The level of a sine at `freq` after `filter` (an expression using `x`),
/// relative to the sine, in dB, once it has settled.
fn response(imports: &[&str], filter: &str, freq: f32) -> f32 {
    let src = format!(
        "{}\nrill main() Sample {{\n    let x = sine({freq}Hz)\n    return {filter}\n}}",
        with(imports, "0").replace("rill main() Sample {\n    return 0\n}", "")
    );
    let src = format!("import \"@gen/osc\"\n{src}");
    let out = render(&src, 48_000);
    let tail = &out[24_000..];
    20.0 * (rms(tail) / std::f32::consts::FRAC_1_SQRT_2).log10()
}

#[test]
fn delay_reads_back_each_tap() {
    let src = format!(
        "{IMPULSE}\nrill main() Sample {{\n    let taps = delay(impulse(), [3 / RATE, 5.5 / RATE])\n    return taps[0] * 10 + taps[1]\n}}"
    );
    let out = render(&src, 8);
    assert_eq!(out, [0.0, 0.0, 0.0, 10.0, 0.0, 0.5, 0.5, 0.0]);
}

#[test]
fn every_delay_has_its_own_memory() {
    // Two calls, two lines; a stereo input, a line per channel.
    let src = format!(
        "{IMPULSE}\nrill main() Sample {{\n    let a = delay(impulse(), 2 / RATE)\n    let b = delay(impulse() * 3, 2 / RATE)\n    let s = delay([impulse(), impulse() * 5], 1 / RATE)\n    return a + b * 10 + s[1] * 100\n}}"
    );
    assert_eq!(render(&src, 4), [0.0, 500.0, 31.0, 0.0]);
}

/// The warnings from checking `src`.
fn warnings(src: &str) -> Vec<String> {
    match lang::compile(src) {
        Ok((_, checked)) => checked.warnings.into_iter().map(|d| d.message).collect(),
        Err(d) => panic!("{d:#?}"),
    }
}

#[test]
fn a_moving_delay_should_say_its_max() {
    let main = |body: &str| format!("rill main() Sample {{\n    {body}\n}}");
    assert_eq!(
        warnings(&main("return delay(1, 10ms + noise() * 1ms)")),
        ["`time` changes while playing, but `max` is not given"]
    );
    assert!(warnings(&main("return delay(1, 10ms + noise() * 1ms, max: 20ms)")).is_empty());
    assert!(warnings(&main("return delay(1, 10ms)")).is_empty());
    // Without `max`, a moving delay still works, holding up to 2s.
    let out = render(
        &format!(
            "{IMPULSE}\n{}",
            main("return delay(impulse(), 1.5s + noise() * 0s)")
        ),
        72_001,
    );
    assert_eq!(out[72_000], 1.0);
    // `max` itself must be a constant.
    assert_eq!(
        lang::compile(&main("return delay(1, 10ms, max: noise() * 1s)")).unwrap_err()[0].message,
        "`max` must be a constant"
    );
    // A rill passing its parameter on is fine when its caller passes a
    // constant, and an error at build time otherwise.
    let pass = "rill d(x: Sample, m: Time) Sample { return delay(x, 1ms, max: m) }\n";
    assert!(lang::compile(&format!("{pass}rill main() Sample {{ return d(1, 5ms) }}")).is_ok());
    let config = rill::Config {
        sample_rate: 48_000,
        max_frames: 64,
        out_channels: 1,
    };
    let moving = format!("{pass}rill main() Sample {{ return d(1, 5ms + noise() * 1ms) }}");
    let errs = lang::load(&moving, &config, "main").err().unwrap();
    assert!(
        errs[0].message.starts_with("`max` must be a constant"),
        "{errs:?}"
    );
}

#[test]
fn any_max_parameter_warns_the_same_way() {
    // A rill of your own with a `max` gets the same warning.
    let src = "rill wobble(x: Sample, time: Time, max: Time = 1s) Sample { return delay(x, time, max: max) }\nrill main() Sample { return wobble(1, 5ms + noise() * 1ms) }";
    assert_eq!(
        warnings(src),
        ["`time` changes while playing, but `max` is not given"]
    );
    let src = "import \"@effects/filter\"\nrill main() Sample { return lowpass(noise(), 1kHz, slope: 24dB / 1oct + noise() * (12dB / 1oct)) }";
    assert_eq!(
        warnings(src),
        ["`slope` changes while playing, but `max` is not given"]
    );
}

#[test]
fn slopes_are_levels_over_intervals() {
    // 6dB over 6 semitones is 12 dB per octave.
    let v = |e: &str| render(&with(&[], &format!("({e}) / (1dB / 1oct)")), 1)[0];
    assert!((v("6dB / 6st") - 12.0).abs() < 1e-4, "{}", v("6dB / 6st"));
    assert!((v("24dB / 1oct") - 24.0).abs() < 1e-4);
    assert!((v("12dB / 1oct * 2 + 6dB / 1oct") - 30.0).abs() < 1e-4);
    // A slope over an interval is a level again: 12 dB/oct over 2 octaves.
    let g = render(&with(&[], "amp(12dB / 1oct * 2oct)"), 1)[0];
    assert!((g - 10f32.powf(24.0 / 20.0)).abs() < 1e-3, "{g}");
    // A plain number is not a slope.
    let errs = lang::compile(
        "import \"@effects/filter\"\nrill main() Sample { return lowpass(0, 1kHz, slope: 24) }",
    )
    .unwrap_err();
    assert!(errs[0].message.contains("Slope"), "{errs:?}");
}

#[test]
fn filters_fall_as_steeply_as_their_slope() {
    let f = &["@effects/filter"];
    for slope in [6.0f32, 12.0, 18.0, 24.0, 48.0] {
        // Well above the cutoff (and well below half the sample rate,
        // where the filter falls faster still), each octave falls by the
        // slope.
        let filter = format!("lowpass(x, 300Hz, slope: {slope}dB / 1oct)");
        let one = response(f, &filter, 900.0);
        let two = response(f, &filter, 1800.0);
        let fall = one - two;
        assert!((fall - slope).abs() < 1.5, "{slope}: {fall} dB per octave");
        let high = format!("highpass(x, 3kHz, slope: {slope}dB / 1oct)");
        let fall = response(f, &high, 1000.0) - response(f, &high, 500.0);
        assert!(
            (fall - slope).abs() < 1.5,
            "highpass {slope}: {fall} dB per octave"
        );
    }
    // Between whole poles, between their slopes.
    let fall = |slope: f32| {
        let filter = format!("lowpass(x, 300Hz, slope: {slope}dB / 1oct)");
        response(f, &filter, 900.0) - response(f, &filter, 1800.0)
    };
    let mid = fall(21.0);
    assert!(mid > fall(18.0) && mid < fall(24.0), "{mid}");
    // Resonance rings at every slope from two poles up.
    for slope in [12.0f32, 24.0, 48.0] {
        let peak = response(
            f,
            &format!("lowpass(x, 1kHz, resonance: 0.9, slope: {slope}dB / 1oct)"),
            1000.0,
        );
        assert!(peak > 6.0, "{slope}: {peak}");
    }
}

#[test]
fn a_moving_slope_stays_finite() {
    let src = "import \"@gen/osc\"\nimport \"@effects/filter\"\nrill main() Sample {\n    let s = phasor(7Hz) * 96dB / 1oct\n    let c = 200Hz + phasor(3Hz) * 8kHz\n    return lowpass(saw(110Hz), c, resonance: 0.9, slope: s, max: 96dB / 1oct)\n}";
    let out = render(src, 48_000);
    assert!(out.iter().all(|x| x.is_finite() && x.abs() < 100.0));
}

#[test]
fn delays_keep_state_so_only_rills_use_them() {
    let msg = |src: &str| lang::compile(src).unwrap_err()[0].message.clone();
    assert_eq!(
        msg("fn f(x: Float) Float { noise() }\nrill main() Sample { return 0 }"),
        "`noise` keeps state between samples, so only a rill can use it"
    );
    assert_eq!(
        msg("rill main() Sample {\n    let d = delay\n    return 0\n}"),
        "`delay` keeps state between samples, so it cannot be used as a value"
    );
    assert_eq!(
        msg("const N = noise()\nrill main() Sample { return 0 }"),
        "the value of `N` must be a constant"
    );
}

#[test]
fn noise_is_new_every_sample() {
    let out = render(&with(&[], "noise()"), 48_000);
    assert!(out.iter().all(|x| (-1.0..1.0).contains(x)));
    let mean = out.iter().sum::<f32>() / out.len() as f32;
    assert!(mean.abs() < 0.01, "{mean}");
    // Uniform in [-1, 1): an RMS of 1/√3.
    assert!((rms(&out) - 0.577).abs() < 0.01, "{}", rms(&out));
    // Two calls are two sequences.
    let two = render(&with(&[], "noise() - noise()"), 100);
    assert!(two.iter().any(|&x| x != 0.0));
}

// ---- the core modules -----------------------------------------------------

#[test]
fn noise_colours() {
    for (name, lo, hi) in [("pink()", 0.05, 0.4), ("brown()", 0.05, 0.6)] {
        let out = render(&with(&["@gen/noise"], name), 96_000);
        let level = rms(&out[48_000..]);
        assert!((lo..hi).contains(&level), "{name}: {level}");
        assert!(out.iter().all(|x| x.abs() < 1.5), "{name}");
    }
}

#[test]
fn perlin_wanders_smoothly_between_0_and_1() {
    // 20 seconds at 4 steps a second.
    let out = render(&with(&["@gen/noise"], "perlin(4Hz)"), 960_000);
    assert!(out.iter().all(|x| (0.0..=1.0).contains(x)));
    let mean = out.iter().sum::<f32>() / out.len() as f32;
    assert!((mean - 0.5).abs() < 0.05, "{mean}");
    // It moves: well away from the middle, both ways.
    let (lo, hi) = out
        .iter()
        .fold((1.0f32, 0.0f32), |(l, h), &x| (l.min(x), h.max(x)));
    assert!(lo < 0.3 && hi > 0.7, "{lo}..{hi}");
    // Gradually: no step bigger than its steepest slope allows, and no
    // corners (the slope barely changes between samples).
    assert!(biggest_step(&out) < 1e-3, "{}", biggest_step(&out));
    let slopes: Vec<f32> = out.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(biggest_step(&slopes) < 1e-5, "{}", biggest_step(&slopes));
    // Back through the middle at every step (give or take a little
    // rounding in where steps fall).
    for k in 1..80 {
        let near = &out[k * 12_000 - 600..k * 12_000 + 600];
        let closest = near.iter().map(|x| (x - 0.5).abs()).fold(1.0, f32::min);
        assert!(closest < 1e-3, "step {k}: {closest}");
    }
    // Copies wander on their own, and the same run alone or together.
    let together = render(&with(&["@gen/noise"], "sum([perlin(4Hz); 4]) / 4"), 48_000);
    let apart = render(
        &with(
            &["@gen/noise"],
            "(perlin(4Hz) + perlin(4Hz) + perlin(4Hz) + perlin(4Hz)) / 4",
        ),
        48_000,
    );
    let one = render(&with(&["@gen/noise"], "perlin(4Hz)"), 48_000);
    assert!(together.iter().zip(&one).any(|(a, b)| (a - b).abs() > 0.05));
    assert!(apart.iter().zip(&one).any(|(a, b)| (a - b).abs() > 0.05));
}

#[test]
fn filters_respond_where_they_should() {
    let f = &["@effects/filter"];
    // 3 dB down at the cutoff, flat well below, falling 12 dB per octave
    // well above.
    let at = response(f, "lowpass(x, 1kHz)", 1000.0);
    assert!((at + 3.0).abs() < 0.5, "{at}");
    assert!(response(f, "lowpass(x, 1kHz)", 100.0).abs() < 0.2);
    let above = response(f, "lowpass(x, 1kHz)", 8000.0);
    assert!(above < -30.0, "{above}");
    let high = response(f, "highpass(x, 1kHz)", 100.0);
    assert!(high < -30.0, "{high}");
    assert!(response(f, "highpass(x, 1kHz)", 10000.0).abs() < 0.5);
    assert!(response(f, "notch(x, 1kHz, resonance: 0.5)", 1000.0) < -30.0);
    // Resonance makes a peak.
    assert!(response(f, "lowpass(x, 1kHz, resonance: 0.9)", 1000.0) > 10.0);
}

#[test]
fn a_swept_resonant_filter_stays_finite() {
    let src = "import \"@gen/osc\"\nimport \"@effects/filter\"\nrill main() Sample {\n    let sweep = phasor(30Hz) * 20000Hz\n    return lowpass(saw(110Hz), sweep, resonance: 1)\n}";
    let out = render(src, 48_000);
    assert!(out.iter().all(|x| x.is_finite() && x.abs() < 100.0));
}

#[test]
fn envelopes_take_their_times() {
    // A gate open for 0.5s: attack 10ms, decay to 0.5, release 100ms.
    let src = "import \"@effects/env\"\nrill main() Sample {\n    state t: Float = 0\n    t = t + 1 / RATE / 1s\n    let gate = if t < 0.5 { 1.0 } else { 0.0 }\n    return ahdsr(gate, 10ms, 0s, 100ms, 0.5, 100ms)\n}";
    let out = render(src, 36_000);
    let reached = out.iter().position(|&x| x >= 1.0).unwrap();
    assert!((470..=490).contains(&reached), "{reached}");
    assert!((out[23_000] - 0.5).abs() < 0.01, "{}", out[23_000]);
    // 60 dB below the sustain 100ms after the gate closes.
    let after = out[24_000 + 4_800];
    assert!((after - 0.0005).abs() < 0.0002, "{after}");
    // A hold keeps it at 1 for 50ms before the decay starts.
    let src =
        "import \"@effects/env\"\nrill main() Sample { return ahdsr(1, 10ms, 50ms, 100ms, 0.5) }";
    let out = render(src, 4_000);
    let top = out.iter().position(|&x| x >= 1.0).unwrap();
    assert!(out[top..top + 2_400].iter().all(|&x| x == 1.0));
    assert!(out[top + 2_400 + 10] < 1.0);
    // `slew` gets 99.9% of the way in its time.
    let src = "import \"@effects/env\"\nrill main() Sample { return slew(1, 10ms, 10ms) }";
    let out = render(src, 481);
    assert!((out[479] - 0.999).abs() < 0.001, "{}", out[479]);
}

#[test]
fn dynamics_move_levels() {
    let d = &["@effects/dynamics"];
    // 4:1 above -20dB: a sine at -2dB (18 dB over) comes out 4.5 dB over.
    let level = response(d, "dynamics(x * amp(-2dB), -20dB, above: 0.25)", 440.0);
    assert!((level + 15.5).abs() < 1.0, "{level}");
    // A limiter holds the level at its threshold.
    let level = response(d, "dynamics(x, -12dB, above: 0, attack: 1ms)", 440.0);
    assert!((level + 12.0).abs() < 1.0, "{level}");
    // A gate shuts out what is below its threshold.
    let level = response(d, "dynamics(x * amp(-60dB), -40dB, below: 100)", 440.0);
    assert!(level < -120.0, "{level}");
    // Nothing changes with both slopes at 1.
    let level = response(d, "dynamics(x, -20dB)", 440.0);
    assert!(level.abs() < 0.1, "{level}");
}

#[test]
fn echoes_repeat_and_fade() {
    let src = format!(
        "import \"@effects/delay\"\n{IMPULSE}\nrill main() Sample {{\n    return comb(impulse(), 10ms, feedback: 0.5)\n}}"
    );
    let out = render(&src, 2000);
    // Repeats every 480 samples, each half the last.
    assert_eq!(out[480], 1.0);
    assert_eq!(out[960], 0.5);
    assert_eq!(out[1440], 0.25);
    assert_eq!(out[1920], 0.125);
    // And nothing in between.
    assert_eq!(out.iter().filter(|&&x| x != 0.0).count(), 4);
}

#[test]
fn an_allpass_keeps_every_level() {
    let level = response(
        &["@effects/delay"],
        "comb(x, 7ms, feedback: 0.6, blend: -0.6)",
        300.0,
    );
    assert!(level.abs() < 0.2, "{level}");
    let level = response(
        &["@effects/delay"],
        "comb(x, 7ms, feedback: 0.6, blend: -0.6)",
        2777.0,
    );
    assert!(level.abs() < 0.2, "{level}");
}

/// The pitch of `out` (between 50Hz and 1kHz), from the lag at which it
/// best matches itself, refined between samples.
fn pitch(out: &[f32]) -> f32 {
    let corr = |lag: usize| -> f32 { out.iter().zip(&out[lag..]).map(|(a, b)| a * b).sum() };
    let (lo, hi) = ((RATE / 1000.0) as usize, (RATE / 50.0) as usize);
    let best = (lo..hi)
        .max_by(|&a, &b| corr(a).total_cmp(&corr(b)))
        .unwrap();
    let (a, b, c) = (corr(best - 1), corr(best), corr(best + 1));
    let shift = 0.5 * (a - c) / (a - 2.0 * b + c);
    RATE / (best as f32 + shift)
}

#[test]
fn a_comb_rings_at_its_pitch() {
    for freq in [110.0f32, 220.0, 440.0] {
        let src = format!(
            "import \"@effects/delay\"\n{IMPULSE}\nrill main() Sample {{\n    return comb(impulse(), 1 / {freq}Hz, feedback: 0.99)\n}}"
        );
        let out = render(&src, 24_000);
        let p = pitch(&out[4_800..]);
        let cents = 1200.0 * (p / freq).log2();
        assert!(cents.abs() < 5.0, "{freq}: {p} ({cents} cents)");
    }
}

#[test]
fn pan_keeps_the_power() {
    let src = |pos: f32| {
        format!("import \"@mix/stereo\"\nrill main() [Sample; 2] {{\n    return pan(1, {pos})\n}}")
    };
    for pos in [-1.0f32, -0.3, 0.0, 0.5, 1.0] {
        let config = rill::Config {
            sample_rate: 48_000,
            max_frames: 64,
            out_channels: 2,
        };
        let (graph, _) = lang::load(&src(pos), &config, "main").unwrap();
        let mut engine = Engine::new(graph, config).unwrap();
        let out = offline::render(&mut engine, 1, &Blocks::Fixed(64));
        assert!(
            (out[0] * out[0] + out[1] * out[1] - 1.0).abs() < 1e-5,
            "{pos}: {out:?}"
        );
    }
}

#[test]
fn control_helpers() {
    let u = &["@util/control"];
    assert_eq!(render(&with(u, "map(0.5, -1, 1, 0, 10)"), 1)[0], 7.5);
    // `rising` once per rise; `sample_hold` keeps what it caught.
    let src = "import \"@util/control\"\nimport \"@gen/osc\"\nrill main() Sample {\n    let clock = square(1kHz)\n    return sample_hold(phasor(100Hz), clock)\n}";
    let out = render(src, 480);
    let changes = out.windows(2).filter(|w| w[0] != w[1]).count();
    assert!((9..=11).contains(&changes), "{changes}");
}
