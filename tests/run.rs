//! Running Rill source end to end: build stage, bytecode and engine.

use rill::lang;
use rill::offline::{self, Blocks};
use rill::{Config, Engine, Graph};

const RATE: u32 = 48_000;

const SINE: &str = "
rill sine(freq: Hz) -> sample {
    state phase: f32 = 0
    phase = wrap(phase + freq / RATE)
    return sin(phase * TAU)
}
";

fn config(channels: usize) -> Config {
    Config {
        sample_rate: RATE,
        max_frames: 256,
        out_channels: channels,
    }
}

fn graph(src: &str, channels: usize) -> Graph {
    match lang::load(src, &config(channels)) {
        Ok((graph, _)) => graph,
        Err(diags) => {
            let rendered: String = diags.iter().map(|d| d.render("test.rill", src)).collect();
            panic!("{rendered}");
        }
    }
}

fn errors(src: &str, channels: usize) -> Vec<String> {
    match lang::load(src, &config(channels)) {
        Ok(_) => Vec::new(),
        Err(diags) => diags.into_iter().map(|d| d.message).collect(),
    }
}

fn render_with(src: &str, channels: usize, frames: usize, blocks: Blocks) -> Vec<f32> {
    let mut engine = Engine::new(graph(src, channels), config(channels)).unwrap();
    offline::render(&mut engine, frames, &blocks)
}

/// Mono render.
fn render(src: &str, frames: usize) -> Vec<f32> {
    render_with(src, 1, frames, Blocks::Fixed(64))
}

fn close(a: &[f32], b: &[f32], tol: f32) {
    assert_eq!(a.len(), b.len());
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        assert!((x - y).abs() <= tol, "frame {i}: {x} vs {y}");
    }
}

#[test]
fn rill_sine_matches_the_native_oscillator() {
    let src = format!("{SINE}\nout(sine(440Hz) * 0.3)");
    let n = RATE as usize / 2;
    let ours = render(&src, n);
    let mut native = Engine::new(rill::patches::sine(440.0, 0.3), config(1)).unwrap();
    let expected = offline::render(&mut native, n + 1, &Blocks::Fixed(64));
    // The rill advances its phase before reading it, so it runs one sample
    // ahead. It also keeps the phase in f32 where the native node uses f64.
    close(&ours, &expected[1..], 1e-3);
}

#[test]
fn design_doc_example_has_the_right_vibrato() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/sketch.rill"))
        .unwrap();
    let out = render(&src, RATE as usize * 2);
    let crossings: Vec<usize> = (1..out.len())
        .filter(|&i| out[i - 1] < 0.0 && out[i] >= 0.0)
        .collect();
    let freqs: Vec<f32> = crossings
        .windows(2)
        .map(|w| RATE as f32 / (w[1] - w[0]) as f32)
        .collect();
    let lo = freqs.iter().cloned().fold(f32::INFINITY, f32::min);
    let hi = freqs.iter().cloned().fold(0.0, f32::max);
    assert!(
        (415.0..425.0).contains(&lo) && (455.0..465.0).contains(&hi),
        "{lo}..{hi}"
    );
    let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!((peak - 0.3).abs() < 1e-3);
}

#[test]
fn examples_are_block_size_invariant() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rill") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let reference = render_with(&src, 2, 10_000, Blocks::Fixed(256));
        for blocks in [
            Blocks::Fixed(1),
            Blocks::Fixed(1000),
            Blocks::Random {
                min: 0,
                max: 700,
                seed: 7,
            },
        ] {
            let got = render_with(&src, 2, 10_000, blocks.clone());
            assert!(got == reference, "{path:?} differs with {blocks:?}");
        }
    }
}

// ---- the build stage ----------------------------------------------------

#[test]
fn constant_arguments_are_specialised_away() {
    // One node, with the frequency folded into its code.
    assert_eq!(graph(&format!("{SINE}\nout(sine(440Hz))"), 1).len(), 1);
}

#[test]
fn pure_code_on_constants_folds_to_nothing() {
    let src = "
        fn double(x: sample) -> sample { x * 2 }
        fn abs2(x: sample) -> sample { if x < 0 { -x } else { x } }
        out(double(abs2(-0.125)) + sum([0.1, 0.15]))
    ";
    assert_eq!(graph(src, 1).len(), 0);
    close(&render(src, 4), &[0.5; 4], 1e-6);
}

#[test]
fn a_bound_stream_is_one_node() {
    let g = graph(&format!("{SINE}\nlet s = sine(1Hz)\nout(s + s)"), 1);
    assert_eq!(g.len(), 2); // sine and the add
    let g = graph(&format!("{SINE}\nout(sine(1Hz) + sine(1Hz))"), 1);
    assert_eq!(g.len(), 3); // two separate instances
}

#[test]
fn unused_bindings_cost_nothing() {
    let src = format!("{SINE}\nlet unused = sine(3Hz)\nout(sine(1Hz))");
    let engine = Engine::new(graph(&src, 1), config(1)).unwrap();
    assert_eq!(engine.node_count(), 1);
}

// ---- rill bodies --------------------------------------------------------

#[test]
fn state_persists_and_branches_work() {
    let src = "
        rill counter(x: sample) -> sample {
            state n: f32 = 0
            n = n + 1
            if n > 3 { n = 0 }
            return n + x
        }
        out(counter(0))
    ";
    let expected: Vec<f32> = (0..20).map(|i| ((i + 1) % 4) as f32).collect();
    for blocks in [Blocks::Fixed(1), Blocks::Fixed(7), Blocks::Fixed(64)] {
        assert_eq!(render_with(src, 1, 20, blocks), expected);
    }
}

#[test]
fn early_returns() {
    let src = format!(
        "{SINE}
        rill sign(x: sample) -> sample {{
            if x > 0 {{ return 1 }}
            if x < 0 {{ return -1 }}
            return 0
        }}
        out(sign(sine(1000Hz)))"
    );
    // The sine is one sample ahead, so it is positive for frames 0..23.
    let out = render(&src, 96);
    assert!(out[0..22].iter().all(|&x| x == 1.0), "{:?}", &out[..24]);
    assert!(out[24..46].iter().all(|&x| x == -1.0), "{:?}", &out[22..48]);
}

#[test]
fn reading_state_before_assigning_it_gives_the_old_value() {
    // A one-sample delay: `let old = s` must not see the new value.
    let src = format!(
        "{SINE}
        rill delay1(x: sample) -> sample {{
            state s: sample = 0
            let old = s
            s = x
            return old
        }}
        let x = sine(1000Hz)
        out([x, delay1(x)])"
    );
    let out = render_with(&src, 2, 200, Blocks::Fixed(13));
    assert_eq!(out[1], 0.0);
    for i in 1..100 {
        assert_eq!(out[i * 2 + 1], out[(i - 1) * 2], "frame {i}");
    }
}

#[test]
fn frame_state_swaps_atomically() {
    let src = "
        rill flip(x: sample) -> sample {
            state s: [f32; 2] = [1, 2]
            s = [s[1], s[0]]
            return s[0] + x
        }
        out(flip(0))
    ";
    assert_eq!(render(src, 6), [2.0, 1.0, 2.0, 1.0, 2.0, 1.0]);
}

#[test]
fn each_call_site_has_its_own_state() {
    let src = "
        rill counter(x: sample) -> sample {
            state n: f32 = 0
            n = n + x
            return n
        }
        rill two(x: sample) -> sample {
            return counter(x) + counter(x * 10)
        }
        out(two(1))
    ";
    assert_eq!(render(src, 3), [11.0, 22.0, 33.0]);
}

#[test]
fn rills_lift_over_channels_with_independent_state() {
    let src = "
        rill acc(x: sample) -> sample {
            state total: sample = 0
            total = total + x
            return total
        }
        out(acc([1, 2]))
    ";
    let out = render_with(src, 2, 3, Blocks::Fixed(2));
    assert_eq!(out, [1.0, 2.0, 2.0, 4.0, 3.0, 6.0]);
}

#[test]
fn generic_rills_and_reductions() {
    let src = "
        rill mix_down<N>(x: [sample; N]) -> [sample; 1] {
            return [sum(x) / N]
        }
        rill widest<N>(x: [sample; N]) -> sample {
            return max(x) - min(x)
        }
        out(mix_down([0.1, 0.2, 0.6])[0] + widest([0.5, -0.25]))
    ";
    close(&render(src, 2), &[0.3 + 0.75; 2], 1e-6);
}

#[test]
fn peak_decays_by_60_db_over_its_release() {
    let src = "
        rill impulse(x: sample) -> sample {
            state first: bool = true
            let y = if first { 1 } else { 0 }
            first = false
            return y + x
        }
        rill peak(x: sample, release: Time = 300ms) -> sample {
            state level: sample = 0
            level = if abs(x) > level { abs(x) } else { level * decay(release) }
            return level
        }
        out(impulse(0) |> peak(release: 100ms))
    ";
    let out = render(src, 4801);
    assert_eq!(out[0], 1.0);
    // 100 ms after the impulse the level is 0.001 (-60 dB).
    assert!((out[4800] - 0.001).abs() < 2e-5, "{}", out[4800]);
}

#[test]
fn dynamic_channel_index() {
    let src = "
        rill step(x: sample) -> i32 {
            state n: i32 = 0
            let out = n
            n = (n + 1) % 3
            return out
        }
        rill pick(i: i32) -> sample {
            let options = [10, 20, 30]
            return options[i]
        }
        out(pick(step(0)))
    ";
    assert_eq!(render(src, 5), [10.0, 20.0, 30.0, 10.0, 20.0]);
}

// ---- top level ----------------------------------------------------------

#[test]
fn stream_conditions_select_per_frame() {
    let src = format!("{SINE}\nlet s = sine(1000Hz)\nout(if s > 0 {{ 1 }} else {{ -1 }})");
    let out = render(&src, 48);
    assert!(out[0..22].iter().all(|&x| x == 1.0));
    assert!(out[24..46].iter().all(|&x| x == -1.0));
}

#[test]
fn outputs_mix_and_route() {
    // Two mono outs mix; a stereo out goes channel by channel.
    let out = render_with("out(0.25)\nout(0.5)", 2, 2, Blocks::Fixed(2));
    assert_eq!(out, [0.75; 4]);
    let out = render_with("out([0.1, 0.2])\nout(1)", 2, 1, Blocks::Fixed(1));
    close(&out, &[1.1, 1.2], 1e-6);

    assert_eq!(
        errors("out([0.1, 0.2, 0.3])", 2),
        ["`out` got 3 channels, but the output has 2"]
    );
}

#[test]
fn rate_changing_rills_are_rejected_for_now() {
    let src = "
        rill decimate(x: sample) -> sample @ rate / 2 { return x }
        out(decimate(0.5))
    ";
    assert_eq!(
        errors(src, 1),
        ["`decimate` changes the sample rate, which is not supported yet"]
    );
}

#[test]
fn silent_programs_warn() {
    let (_, warnings) = lang::load("let x = 1", &config(1)).unwrap();
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].message,
        "nothing is sent to `out`, so this plays silence"
    );
}
