//! Running Rill source end to end: build stage, bytecode and engine.

use rill::lang;
use rill::offline::{self, Blocks};
use rill::{Config, Engine, EventValue, Graph, ParamEvent, RillEvent};

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

/// `rill main() -> sample { return <expr> }`
fn main_returning(expr: &str) -> String {
    format!("rill main() -> sample {{\n    return {expr}\n}}")
}

fn graph(src: &str, channels: usize) -> Graph {
    graph_from(src, channels, "main")
}

fn graph_from(src: &str, channels: usize, entry: &str) -> Graph {
    match lang::load(src, &config(channels), entry) {
        Ok((graph, _)) => graph,
        Err(diags) => {
            let rendered: String = diags.iter().map(|d| d.render("test.rill", src)).collect();
            panic!("{rendered}");
        }
    }
}

fn errors(src: &str, channels: usize) -> Vec<String> {
    match lang::load(src, &config(channels), "main") {
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
    let src = format!("{SINE}\n{}", main_returning("sine(440Hz) * 0.3"));
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
    let src = format!(
        "{SINE}
        rill main() -> sample {{
            let lfo   = sine(0.5Hz) * 20Hz + 440Hz
            let voice = sine(lfo) * 0.3
            return voice
        }}"
    );
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
fn a_program_is_one_node() {
    assert_eq!(
        graph(
            &format!("{SINE}\n{}", main_returning("sine(440Hz) * sine(3Hz)")),
            1
        )
        .len(),
        1
    );
}

#[test]
fn constant_programs_fold_to_nothing() {
    let src = format!(
        "fn double(x: sample) -> sample {{ x * 2 }}
        fn abs2(x: sample) -> sample {{ if x < 0 {{ -x }} else {{ x }} }}
        {}",
        main_returning("double(abs2(-0.125)) + sum([0.1, 0.15])")
    );
    assert_eq!(graph(&src, 1).len(), 0);
    close(&render(&src, 4), &[0.5; 4], 1e-6);
}

#[test]
fn entry_parameters_are_live_controls() {
    let src = "rill main(gain: sample = 0.25) -> sample { return gain }";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    assert_eq!(engine.params().collect::<Vec<_>>(), vec!["gain"]);

    let mut out = [0.0f32; 4];
    engine.render_planar(&mut [&mut out]);
    close(&out, &[0.25; 4], 1e-6);

    assert!(engine.set_param("gain", 0.75));
    assert!(!engine.set_param("missing", 1.0));

    let mut ramp = [0.0f32; 240];
    engine.render_planar(&mut [&mut ramp]);
    assert!(ramp[0] > 0.25 && ramp[0] < 0.75);
    assert!((ramp[239] - 0.75).abs() < 1e-6);
}

#[test]
fn parameter_events_split_blocks_at_sample_offsets() {
    let src = "rill main(gain: sample = 0) -> sample { return gain }";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    let mut out = [0.0f32; 12];
    engine.render_planar_with_events(
        &mut [&mut out],
        &[ParamEvent {
            frame_offset: 4,
            name: "gain",
            value: 1.0,
        }],
    );
    assert_eq!(&out[..4], &[0.0; 4]);
    assert!(out[4] > 0.0);
    assert!(out[5] > out[4]);
}

#[test]
fn rill_events_run_handlers_at_sample_offsets() {
    let src = "
        rill main() -> sample {
            state pitch: Hz = 440Hz
            on note_on(note) {
                let tuning = equal(12)
                pitch = note.pitch |> tuning
            }
            return pitch / 1Hz
        }
    ";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    let mut out = [0.0f32; 6];
    let values = [EventValue {
        name: "pitch".to_owned(),
        value: rill::lang::check::pitch_literal("E5").unwrap(),
    }];
    engine.render_interleaved_with_rill_events(
        &mut out,
        |x| x,
        &[RillEvent {
            frame_offset: 2,
            name: "note_on",
            values: &values,
        }],
    );
    for x in &out[..2] {
        assert!((*x - 440.0).abs() < 1e-4);
    }
    for x in &out[2..] {
        assert!((*x - 659.255).abs() < 0.01);
    }
}

#[test]
fn pitch_literals_and_callable_tunings_resolve_to_hz() {
    let src = "
        rill main() -> [sample; 4] {
            let equal12 = equal(12)
            let just_c = just(C)
            let pyth_c = pythagorean(C)
            return [
                (A4 |> equal12) / 1Hz,
                ((A4 + 12st) |> equal12) / 1Hz,
                (C4 |> just_c) / 1Hz,
                (C4 |> pyth_c) / 1Hz,
            ]
        }
    ";
    let out = render_with(src, 4, 1, Blocks::Fixed(1));
    assert!((out[0] - 440.0).abs() < 1e-4);
    assert!((out[1] - 880.0).abs() < 1e-4);
    // A4 sits on 440Hz and the root follows from the scale's ratio for A:
    // 5/3 in just intonation, 27/16 in Pythagorean tuning.
    assert!((out[2] - 264.0).abs() < 0.01);
    assert!((out[3] - 260.7407).abs() < 0.01);

    let src = "
        rill main() -> [sample; 3] {
            let tuning = equal(12, a4: 432Hz)
            let tuned = [A4, C5, E5] |> tuning
            return [tuned[0] / 1Hz, tuned[1] / 1Hz, tuned[2] / 1Hz]
        }
    ";
    let out = render_with(src, 3, 1, Blocks::Fixed(1));
    assert!((out[0] - 432.0).abs() < 1e-4);
    assert!((out[1] - 513.738).abs() < 0.01);
    assert!((out[2] - 647.269).abs() < 0.01);
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
        rill main() -> sample { return counter(0) }
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
        {}",
        main_returning("sign(sine(1000Hz))")
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
        rill main() -> [sample; 2] {{
            let x = sine(1000Hz)
            return [x, delay1(x)]
        }}"
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
        rill main() -> sample { return flip(0) }
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
        rill main() -> sample { return two(1) }
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
        rill main() -> [sample; 2] { return acc([1, 2]) }
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
        rill main() -> sample { return mix_down([0.1, 0.2, 0.6])[0] + widest([0.5, -0.25]) }
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
        rill main() -> sample { return impulse(0) |> peak(release: 100ms) }
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
        rill main() -> sample { return pick(step(0)) }
    ";
    assert_eq!(render(src, 5), [10.0, 20.0, 30.0, 10.0, 20.0]);
}

#[test]
fn conditions_on_streams() {
    let src = format!(
        "{SINE}
        rill main() -> sample {{
            let s = sine(1000Hz)
            return if s > 0 {{ 1 }} else {{ -1 }}
        }}"
    );
    let out = render(&src, 48);
    assert!(out[0..22].iter().all(|&x| x == 1.0));
    assert!(out[24..46].iter().all(|&x| x == -1.0));
}

#[test]
fn rills_in_a_branch_only_advance_when_it_runs() {
    let src = "
        rill counter() -> sample {
            state n: f32 = 0
            n = n + 1
            return n
        }
        rill main() -> sample {
            state t: f32 = 0
            t = t + 1
            if t > 2 { return counter() }
            return 0
        }
    ";
    assert_eq!(render(src, 5), [0.0, 0.0, 1.0, 2.0, 3.0]);
}

// ---- the entry rill -----------------------------------------------------

#[test]
fn entry_output_routing() {
    // A scalar plays on every channel; a frame goes channel by channel.
    let out = render_with(&main_returning("0.25"), 2, 2, Blocks::Fixed(2));
    assert_eq!(out, [0.25; 4]);
    let src = "rill main() -> [sample; 2] { return [0.1, 0.2] }";
    close(&render_with(src, 2, 1, Blocks::Fixed(1)), &[0.1, 0.2], 1e-6);
    // One channel in a frame counts as mono.
    let src = "rill main() -> [sample; 1] { return [0.5] }";
    assert_eq!(render_with(src, 2, 1, Blocks::Fixed(1)), [0.5, 0.5]);

    assert_eq!(
        errors("rill main() -> [sample; 3] { return [0.1, 0.2, 0.3] }", 2),
        ["`main` returns 3 channels, but the output has 2"]
    );
}

#[test]
fn entry_parameters_run_at_their_defaults() {
    let src = "rill main(level: sample = 0.25, gain: f32 = 2) -> sample { return level * gain }";
    assert_eq!(render(src, 2), [0.5, 0.5]);
}

#[test]
fn another_rill_can_be_the_entry() {
    let src = "
        rill main() -> sample { return 0.1 }
        rill other() -> sample { return 0.2 }
    ";
    let mut engine = Engine::new(graph_from(src, 1, "other"), config(1)).unwrap();
    close(
        &offline::render(&mut engine, 1, &Blocks::Fixed(1)),
        &[0.2],
        1e-6,
    );
}

#[test]
fn rate_changing_rills_are_rejected_for_now() {
    let src = format!(
        "rill decimate(x: sample) -> sample @ rate / 2 {{ return x }}\n{}",
        main_returning("decimate(0.5)")
    );
    assert_eq!(
        errors(&src, 1),
        ["`decimate` changes the sample rate, which is not supported yet"]
    );
}

#[test]
fn equal_temperament_keeps_pitches_between_notes() {
    let src = "
        rill main() -> [sample; 4] {
            let et = equal(12)
            let quarter = equal(24)
            return [
                ((A4 + 50cents) |> et) / 1Hz,
                ((A4 - 30cents) |> et) / 1Hz,
                ((A4 + 50cents) |> quarter) / 1Hz,
                (C5 |> quarter) / 1Hz,
            ]
        }
    ";
    let out = render_with(src, 4, 1, Blocks::Fixed(1));
    let expected = [
        440.0 * 2f32.powf(0.5 / 12.0),
        440.0 * 2f32.powf(-0.3 / 12.0),
        440.0 * 2f32.powf(0.5 / 12.0),
        523.2511,
    ];
    for (got, want) in out.iter().zip(expected) {
        assert!((got - want).abs() < 0.01, "{got} vs {want}");
    }
}

#[test]
fn pitch_bends_glide_at_run_time() {
    // The interval changes every tick, so tuning happens in the VM rather
    // than at build time, and must not snap to semitones.
    let src = "
        rill main() -> sample {
            state bend: Interval = 0st
            let t = equal(12)
            let f = (A4 + bend) |> t
            bend = bend + 25cents
            return f / 1Hz
        }
    ";
    let out = render(src, 4);
    for (i, got) in out.iter().enumerate() {
        let want = 440.0 * 2f32.powf(0.25 * i as f32 / 12.0);
        assert!((got - want).abs() < 0.01, "tick {i}: {got} vs {want}");
    }
}

#[test]
fn a_tuning_picked_while_playing_is_an_error() {
    let src = "
        rill pick(x: sample) -> Tuning {
            if x > 0 { return equal(12) }
            return just(C)
        }
        rill main() -> sample {
            state s: sample = 1
            s = -s
            let t = pick(s)
            return (A4 |> t) / 1kHz
        }
    ";
    assert_eq!(errors(src, 1), ["a tuning cannot be chosen while playing"]);
}

#[test]
fn ratio_tunings_put_a4_on_the_reference() {
    let src = "
        rill main() -> [sample; 4] {
            let just_c = just(C)
            let pyth_d = pythagorean(D, a4: 432Hz)
            let mean_c = meantone(C)
            return [
                (A4 |> just_c) / 1Hz,
                (A4 |> pyth_d) / 1Hz,
                (A4 |> mean_c) / 1Hz,
                ((A4 + 50cents) |> just_c) / 1Hz,
            ]
        }
    ";
    let out = render_with(src, 4, 1, Blocks::Fixed(1));
    assert!((out[0] - 440.0).abs() < 1e-3, "{}", out[0]);
    assert!((out[1] - 432.0).abs() < 1e-3, "{}", out[1]);
    assert!((out[2] - 440.0).abs() < 1e-3, "{}", out[2]);
    // Cents on top of a scale note stay continuous.
    assert!(
        (out[3] - 440.0 * 2f32.powf(0.5 / 12.0)).abs() < 0.01,
        "{}",
        out[3]
    );
}
