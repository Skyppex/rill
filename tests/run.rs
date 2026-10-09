//! Running Rill source end to end: build stage, bytecode and engine.

use rill::lang;
use rill::offline::{self, Blocks};
use rill::{Config, Dispatch, Engine, Event, Graph, ParamEvent, Payload, RillEvent};

const RATE: u32 = 48_000;

const SINE: &str = "
rill sine(freq: Freq) Sample {
    state phase: Float = 0
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

/// `rill main() Sample { return <expr> }`
fn main_returning(expr: &str) -> String {
    format!("rill main() Sample {{\n    return {expr}\n}}")
}

fn graph(src: &str, channels: usize) -> Graph {
    graph_from(src, channels, "main")
}

/// Built with a fixed seed, so programs using `random()` build the same
/// every time.
fn graph_from(src: &str, channels: usize, entry: &str) -> Graph {
    let options = lang::build::Options {
        seed: Some(0),
        ..Default::default()
    };
    match lang::load_with(src, &config(channels), entry, &options) {
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
        rill main() Sample {{
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
        "fn double(x: Sample) Sample {{ x * 2 }}
        fn abs2(x: Sample) Sample {{ if x < 0 {{ -x }} else {{ x }} }}
        {}",
        main_returning("double(abs2(-0.125)) + sum([0.1, 0.15])")
    );
    assert_eq!(graph(&src, 1).len(), 0);
    close(&render(&src, 4), &[0.5; 4], 1e-6);
}

#[test]
fn entry_parameters_are_live_controls() {
    let src = "rill main(gain: Sample = 0.25) Sample { return gain }";
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
    let src = "rill main(gain: Sample = 0) Sample { return gain }";
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
        event keys note_on
        rill main() Sample {
            state pitch: Freq = 440Hz
            on keys(note) {
                pitch = note.pitch |> equal(12)
            }
            return pitch / 1Hz
        }
    ";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    let mut out = [0.0f32; 6];
    let e5 = rill::lang::check::pitch_literal("E5").unwrap();
    engine.render_interleaved_with_rill_events(
        &mut out,
        |x| x,
        &[RillEvent {
            frame_offset: 2,
            dispatch: Dispatch::Incoming(Event {
                sender: 0,
                channel: 0,
                payload: Payload::NoteOn {
                    pitch: e5,
                    velocity: 1.0,
                    instance: 0,
                },
            }),
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
        rill main() [Sample; 4] {
            let equal12 = fn(p: Pitch) Freq { equal(p, 12) }
            let just_c = fn(p: Pitch) Freq { just(p, C) }
            let pyth_c = fn(p: Pitch) Freq { pythagorean(p, C) }
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
        rill main() [Sample; 3] {
            let tuned = [A4, C5, E5] |> equal(12, a4: 432Hz)
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
        rill counter(x: Sample) Sample {
            state n: Float = 0
            n = n + 1
            if n > 3 { n = 0 }
            return n + x
        }
        rill main() Sample { return counter(0) }
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
        rill sign(x: Sample) Sample {{
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
        rill delay1(x: Sample) Sample {{
            state s: Sample = 0
            let old = s
            s = x
            return old
        }}
        rill main() [Sample; 2] {{
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
        rill flip(x: Sample) Sample {
            state s: [Float; 2] = [1, 2]
            s = [s[1], s[0]]
            return s[0] + x
        }
        rill main() Sample { return flip(0) }
    ";
    assert_eq!(render(src, 6), [2.0, 1.0, 2.0, 1.0, 2.0, 1.0]);
}

#[test]
fn each_call_site_has_its_own_state() {
    let src = "
        rill counter(x: Sample) Sample {
            state n: Float = 0
            n = n + x
            return n
        }
        rill two(x: Sample) Sample {
            return counter(x) + counter(x * 10)
        }
        rill main() Sample { return two(1) }
    ";
    assert_eq!(render(src, 3), [11.0, 22.0, 33.0]);
}

#[test]
fn rills_lift_over_channels_with_independent_state() {
    let src = "
        rill acc(x: Sample) Sample {
            state total: Sample = 0
            total = total + x
            return total
        }
        rill main() [Sample; 2] { return acc([1, 2]) }
    ";
    let out = render_with(src, 2, 3, Blocks::Fixed(2));
    assert_eq!(out, [1.0, 2.0, 2.0, 4.0, 3.0, 6.0]);
}

#[test]
fn generic_rills_and_reductions() {
    let src = "
        rill mix_down<N>(x: [Sample; N]) [Sample; 1] {
            return [sum(x) / N]
        }
        rill widest<N>(x: [Sample; N]) Sample {
            return max(x) - min(x)
        }
        rill main() Sample { return mix_down([0.1, 0.2, 0.6])[0] + widest([0.5, -0.25]) }
    ";
    close(&render(src, 2), &[0.3 + 0.75; 2], 1e-6);
}

#[test]
fn peak_decays_by_60_db_over_its_release() {
    let src = "
        rill impulse(x: Sample) Sample {
            state first: Bool = true
            let y = if first { 1 } else { 0 }
            first = false
            return y + x
        }
        rill peak(x: Sample, release: Time = 300ms) Sample {
            state level: Sample = 0
            level = if abs(x) > level { abs(x) } else { level * decay(release) }
            return level
        }
        rill main() Sample { return impulse(0) |> peak(release: 100ms) }
    ";
    let out = render(src, 4801);
    assert_eq!(out[0], 1.0);
    // 100 ms after the impulse the level is 0.001 (-60 dB).
    assert!((out[4800] - 0.001).abs() < 2e-5, "{}", out[4800]);
}

#[test]
fn dynamic_channel_index() {
    let src = "
        rill step(x: Sample) Int {
            state n: Int = 0
            let out = n
            n = (n + 1) % 3
            return out
        }
        rill pick(i: Int) Sample {
            let options = [10, 20, 30]
            return options[i]
        }
        rill main() Sample { return pick(step(0)) }
    ";
    assert_eq!(render(src, 5), [10.0, 20.0, 30.0, 10.0, 20.0]);
}

#[test]
fn conditions_on_streams() {
    let src = format!(
        "{SINE}
        rill main() Sample {{
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
        rill counter() Sample {
            state n: Float = 0
            n = n + 1
            return n
        }
        rill main() Sample {
            state t: Float = 0
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
    let src = "rill main() [Sample; 2] { return [0.1, 0.2] }";
    close(&render_with(src, 2, 1, Blocks::Fixed(1)), &[0.1, 0.2], 1e-6);
    // One channel in a frame counts as mono.
    let src = "rill main() [Sample; 1] { return [0.5] }";
    assert_eq!(render_with(src, 2, 1, Blocks::Fixed(1)), [0.5, 0.5]);

    assert_eq!(
        errors("rill main() [Sample; 3] { return [0.1, 0.2, 0.3] }", 2),
        ["`main` returns 3 channels, but the output has 2"]
    );
}

#[test]
fn entry_parameters_run_at_their_defaults() {
    let src = "rill main(level: Sample = 0.25, gain: Float = 2) Sample { return level * gain }";
    assert_eq!(render(src, 2), [0.5, 0.5]);
}

#[test]
fn another_rill_can_be_the_entry() {
    let src = "
        rill main() Sample { return 0.1 }
        rill other() Sample { return 0.2 }
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
        "rill decimate(x: Sample) Sample @ rate / 2 {{ return x }}\n{}",
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
        rill main() [Sample; 4] {
            let et = fn(p: Pitch) Freq { equal(p) }
            let quarter = fn(p: Pitch) Freq { equal(p, 24) }
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
fn intervals_transpose_frequencies() {
    let src = "
        rill main() [Sample; 3] {
            return [
                (440Hz + 12st) / 1Hz,
                (440Hz - 12st) / 1Hz,
                (440Hz + 30cents) / 1Hz,
            ]
        }
    ";
    let out = render_with(src, 3, 1, Blocks::Fixed(1));
    let expected = [880.0, 220.0, 440.0 * 2f32.powf(0.3 / 12.0)];
    for (got, want) in out.iter().zip(expected) {
        assert!((got - want).abs() < 1e-4, "{got} vs {want}");
    }
}

#[test]
fn pitch_bends_glide_at_run_time() {
    // The interval changes every tick, so tuning happens in the VM rather
    // than at build time, and must not snap to semitones.
    let src = "
        rill main() Sample {
            state bend: Interval = 0st
            let t = fn(p: Pitch) Freq { equal(p) }
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
fn a_function_can_be_chosen_while_playing() {
    // `s` flips every tick, so the tuning alternates: just C4 is 264Hz,
    // equal-tempered C4 is 261.63Hz.
    let src = "
        rill main() Sample {
            state s: Sample = 1
            s = -s
            let et: fn(Pitch) Freq = equal
            let jc = fn(p: Pitch) Freq { just(p, C) }
            let t = if s > 0 { et } else { jc }
            return (C4 |> t) / 1Hz
        }
    ";
    let out = render(src, 4);
    for (i, got) in out.iter().enumerate() {
        let want = if i % 2 == 0 { 264.0 } else { 261.6256 };
        assert!((got - want).abs() < 0.01, "tick {i}: {got} vs {want}");
    }
}

#[test]
fn functions_are_inlined_where_they_are_called() {
    let src = format!(
        "{SINE}
        fn a432(p: Pitch) Freq {{
            432Hz * pow(2, (p - A4) / 12st)
        }}
        fn tuned(steps: Int) fn(Pitch) Freq {{
            fn(p) {{ equal(p, steps) }}
        }}
        rill voice(pitch: Pitch, tune: fn(Pitch) Freq) Sample {{
            return (pitch |> tune) / 1kHz
        }}
        rill main() [Sample; 4] {{
            let detune = 3Hz
            return [
                voice(A4, a432),
                voice(A4, fn(p) {{ equal(p) + detune }}),
                voice(A4 + 50cents, tuned(24)),
                voice(A4, equal),
            ]
        }}"
    );
    let out = render_with(&src, 4, 1, Blocks::Fixed(1));
    let expected = [0.432, 0.443, 0.452_893, 0.44];
    for (got, want) in out.iter().zip(expected) {
        assert!((got - want).abs() < 1e-5, "{got} vs {want}");
    }
    // Everything above is constant, so it folds away entirely.
    assert_eq!(graph(&src, 4).len(), 0);
}

#[test]
fn captured_values_can_change_over_time() {
    // `level` is state captured by the fn; each tick the fn sees the value
    // it had when the fn was made.
    let src = "
        rill main() Sample {
            state level: Sample = 0
            level = level + 1
            let scale = fn(x: Sample) Sample { x * level }
            level = level + 100
            return scale(2)
        }
    ";
    // Tick 1: level is 1 when captured, so 2. Then level = 101; tick 2
    // captures 102, and so on.
    assert_eq!(render(src, 3), [2.0, 204.0, 406.0]);
}

#[test]
fn ratio_tunings_put_a4_on_the_reference() {
    let src = "
        rill main() [Sample; 4] {
            let just_c = fn(p: Pitch) Freq { just(p, C) }
            let pyth_d = fn(p: Pitch) Freq { pythagorean(p, D, a4: 432Hz) }
            let mean_c = fn(p: Pitch) Freq { meantone(p, C) }
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

#[test]
fn levels_scale_amplitude() {
    let db = |x: f32| 10f32.powf(x / 20.0);
    let src = "
        rill main() [Sample; 8] {
            let x = 0.5
            return [
                x - 6dB,
                x + 20dB,
                x - 6dB - 6dB,
                x - (-12dB * 0.5),
                amp(-6dB),
                -12dB / -6dB,
                amp(level(0.25)),
                amp(level(0)),
            ]
        }
    ";
    let out = render_with(src, 8, 1, Blocks::Fixed(1));
    let expected = [
        0.5 * db(-6.0),
        5.0,
        0.5 * db(-12.0),
        0.5 * db(6.0),
        db(-6.0),
        2.0,
        0.25,
        1e-6, // silence is held at -120dB
    ];
    for (i, (got, want)) in out.iter().zip(expected).enumerate() {
        assert!((got - want).abs() < 1e-5, "{i}: {got} vs {want}");
    }
    // All constant: folded away.
    assert_eq!(graph(src, 8).len(), 0);
}

#[test]
fn levels_can_change_while_playing() {
    // A fade moving 20dB down per tick, applied to a frame.
    let src = "
        rill main() [Sample; 2] {
            state fade: Gain = 0dB
            let out = [1, 0.5] + fade
            fade = fade - 20dB
            return out
        }
    ";
    let out = render_with(src, 2, 3, Blocks::Fixed(1));
    assert_eq!(out.len(), 6);
    let expected = [1.0, 0.5, 0.1, 0.05, 0.01, 0.005];
    for (i, (got, want)) in out.iter().zip(expected).enumerate() {
        assert!((got - want).abs() < 1e-6, "{i}: {got} vs {want}");
    }
}

#[test]
fn a_gain_parameter_is_a_live_control() {
    let src = "rill main(volume: Gain = -6dB) Sample { return 1 + volume }";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    let mut out = [0.0f32; 4];
    engine.render_interleaved(&mut out[..1]);
    assert!(
        (out[0] - 10f32.powf(-6.0 / 20.0)).abs() < 1e-6,
        "{}",
        out[0]
    );
    // Hosts send gains as plain amplitude factors.
    assert!(engine.set_param("volume", 0.25));
    let mut later = [0.0f32; 4800];
    engine.render_interleaved(&mut later);
    assert!((later[4799] - 0.25).abs() < 1e-6, "{}", later[4799]);
}

#[test]
fn different_functions_from_different_returns_is_an_error() {
    let src = "
        fn pick(x: Sample) fn(Sample) Sample {
            if x > 0 { return fn(v) { v * 2 } }
            return fn(v) { v }
        }
        rill main() Sample {
            state s: Sample = 1
            s = -s
            let f = pick(s)
            return f(0.5)
        }
    ";
    assert_eq!(
        errors(src, 1),
        ["returning different functions from different branches is not supported yet"]
    );
}

#[test]
fn levels_per_channel_and_negated_while_playing() {
    // `fade` drops 20dB per tick. The right channel sits 6dB lower than the
    // left, and `-fade` turns the fade into a boost.
    let db = |x: f32| 10f32.powf(x / 20.0);
    let src = "
        rill main() [Sample; 3] {
            state fade: Gain = 0dB
            let out = [1, 1] - [fade, fade + 6dB]
            let boost = 1 + (-fade)
            fade = fade - 20dB
            return [out[0], out[1], boost]
        }
    ";
    let out = render_with(src, 3, 2, Blocks::Fixed(1));
    let expected = [
        1.0,
        db(-6.0),
        1.0, // tick 0: fade is 0dB
        10.0,
        10.0 * db(-6.0),
        10.0, // tick 1: fade is -20dB
    ];
    for (i, (got, want)) in out.iter().zip(expected).enumerate() {
        assert!((got - want).abs() < 1e-4, "{i}: {got} vs {want}");
    }
}

#[test]
fn casting_to_int_truncates_toward_zero() {
    let src = "
        rill main() [Sample; 4] {
            state x: Float = 2.75
            x = -x
            return [
                (x as Int) as Sample,
                (2.75 as Int) as Sample,
                (-2.75 as Int) as Sample,
                (7 as Int / 2 as Int) as Sample,
            ]
        }
    ";
    // `x` is -2.75 on the first tick, read while playing.
    assert_eq!(
        render_with(src, 4, 1, Blocks::Fixed(1)),
        [-2.0, 2.0, -2.0, 3.0]
    );
}

// ---- nested frames ------------------------------------------------------

/// Stereo frames of `src`, as `[left, right]` pairs.
fn stereo(src: &str, frames: usize) -> Vec<[f32; 2]> {
    render_with(src, 2, frames, Blocks::Fixed(3))
        .chunks(2)
        .map(|c| [c[0], c[1]])
        .collect()
}

#[test]
fn a_chord_through_a_stereo_voice_mixes_to_stereo() {
    let voice = "
        rill pan(x: Sample, pos: Float) [Sample; 2] { return [x * (1 - pos), x * pos] }
    ";
    let lifted = format!(
        "{SINE}{voice}
        rill main() [Sample; 2] {{
            return [C4, E4, G4] |> equal |> sine |> pan(0.25) |> sum
        }}"
    );
    let by_hand = format!(
        "{SINE}{voice}
        rill main() [Sample; 2] {{
            let a = pan(sine(equal(C4)), 0.25)
            let b = pan(sine(equal(E4)), 0.25)
            let c = pan(sine(equal(G4)), 0.25)
            return [a[0] + b[0] + c[0], a[1] + b[1] + c[1]]
        }}"
    );
    let ours = render_with(&lifted, 2, 2_000, Blocks::Fixed(64));
    close(
        &ours,
        &render_with(&by_hand, 2, 2_000, Blocks::Fixed(64)),
        1e-6,
    );
    assert!(ours.iter().any(|x| x.abs() > 0.5), "the chord is audible");
}

#[test]
fn every_lifted_element_has_its_own_state() {
    let src = "
        rill count(step: Sample) Sample {
            state n: Sample = 0
            n = n + step
            return n
        }
        rill main() [Sample; 2] { return sum(count([[1, 2], [3, 4]])) }
    ";
    assert_eq!(stereo(src, 3), [[4.0, 6.0], [8.0, 12.0], [12.0, 18.0]]);

    // A rill taking a frame runs once per bus, each with its own state.
    let src = "
        rill hold(x: [Sample; 2]) [Sample; 2] {
            state last: [Sample; 2] = [0, 0]
            let out = last
            last = x
            return out
        }
        rill main() [Sample; 2] {
            state t: Sample = 0
            t = t + 1
            return sum(hold([[t, 2 * t], [10 * t, 0]]))
        }
    ";
    assert_eq!(stereo(src, 3), [[0.0, 0.0], [11.0, 2.0], [22.0, 4.0]]);
}

#[test]
fn nested_frames_in_operators_state_and_indexing() {
    // A shorter frame lines up with the end it matches: one gain per bus,
    // or one per channel of every bus.
    let src = "rill main() [Sample; 2] { return sum([[1, 2], [3, 4], [5, 6]] * [10, 100, 1000]) }";
    assert_eq!(stereo(src, 1), [[5310.0, 6420.0]]);
    let src = "rill main() [Sample; 2] { return sum([[1, 2], [3, 4], [5, 6]] * [10, 100]) }";
    assert_eq!(stereo(src, 1), [[90.0, 1200.0]]);
    // A square shape needs saying which.
    let src = "rill main() [Sample; 2] { return sum([[1, 2], [3, 4]] * [[10, 100]; 2]) }";
    assert_eq!(stereo(src, 1), [[40.0, 600.0]]);

    // One level per voice.
    let src =
        "rill main() [Sample; 2] { return sum([[1, 1], [1, 1], [1, 1]] + [0dB, -6dB, -6dB]) }";
    let out = stereo(src, 1);
    assert!((out[0][0] - 2.002).abs() < 1e-3, "{out:?}");

    // Reductions take the outer layer off.
    let src = "rill main() [Sample; 2] { return max([[1, 5], [3, 2]]) + min([[1, 5], [3, 2]]) }";
    assert_eq!(stereo(src, 1), [[4.0, 7.0]]);

    // Nested state.
    let src = "
        rill main() [Sample; 2] {
            state s: [[Sample; 2]; 2] = [[0, 0], [0, 0]]
            s = s + [[1, 2], [3, 4]]
            return sum(s)
        }
    ";
    assert_eq!(stereo(src, 2), [[4.0, 6.0], [8.0, 12.0]]);

    // An index known only while playing picks a whole inner frame.
    let src = "
        rill main() [Sample; 2] {
            state k: Int = 0
            let bus = [[1, 2], [3, 4], [5, 6]][k]
            k = (k + 1) % 3
            return bus
        }
    ";
    assert_eq!(
        stereo(src, 4),
        [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0], [1.0, 2.0]]
    );
}

// ---- events -------------------------------------------------------------

const EVENTS: &str = "
event keys note_on(sender: 5, channel: 1)
event any_note note_on
event lifts note_off(sender: 5)
event mod control_change(channel: 1)
event bend control_change(channel: 2)

rill main() [Sample; 4] {
    state a: Sample = 0
    state b: Sample = 0
    state c: Sample = 0
    state d: Sample = 0
    on keys(note) { a = a + note.velocity }
    on any_note(note) { b = b + 1 }
    on lifts(note) { c = note.release }
    on mod(value) { d = value }
    on bend { d = -1 }
    return [a, b, c, d]
}
";

fn note_on(sender: u32, channel: u32, velocity: f32) -> Event {
    Event {
        sender,
        channel,
        payload: Payload::NoteOn {
            pitch: 60.0,
            velocity,
            instance: 0,
        },
    }
}

/// One frame of [`EVENTS`] after sending `events`.
fn after(events: &[Event]) -> Vec<f32> {
    let mut engine = Engine::new(graph(EVENTS, 4), config(4)).unwrap();
    for e in events {
        engine.send(e);
    }
    let mut out = [0.0f32; 4];
    engine.render_interleaved(&mut out);
    out.to_vec()
}

#[test]
fn declarations_filter_by_sender_and_channel() {
    // Matches both `keys` and `any_note`.
    assert_eq!(after(&[note_on(5, 1, 0.5)]), [0.5, 1.0, 0.0, 0.0]);
    // Wrong sender or channel: only the unfiltered `any_note`.
    assert_eq!(after(&[note_on(4, 1, 0.5)]), [0.0, 1.0, 0.0, 0.0]);
    assert_eq!(after(&[note_on(5, 2, 0.5)]), [0.0, 1.0, 0.0, 0.0]);
    // An omitted filter matches any channel.
    let off = Event {
        sender: 5,
        channel: 9,
        payload: Payload::NoteOff {
            pitch: 60.0,
            release: 0.25,
            instance: 0,
        },
    };
    assert_eq!(after(&[off]), [0.0, 0.0, 0.25, 0.0]);
    // Control changes go by channel; the value arrives as it is sent.
    let cc = |channel, value| Event {
        sender: 0,
        channel,
        payload: Payload::Control(value),
    };
    assert_eq!(after(&[cc(1, 0.75)]), [0.0, 0.0, 0.0, 0.75]);
    assert_eq!(after(&[cc(2, 0.75)]), [0.0, 0.0, 0.0, -1.0]);
    assert_eq!(after(&[cc(3, 0.75)]), [0.0; 4]);
    // One event fires every matching declaration, each once.
    assert_eq!(
        after(&[note_on(5, 1, 0.5), note_on(5, 1, 0.25)]),
        [0.75, 2.0, 0.0, 0.0]
    );
}

#[test]
fn sending_to_a_declared_event_skips_its_filters() {
    let mut engine = Engine::new(graph(EVENTS, 4), config(4)).unwrap();
    let keys = engine.event_id("keys").unwrap();
    let on = Payload::NoteOn {
        pitch: 60.0,
        velocity: 0.5,
        instance: 0,
    };
    assert!(engine.send_to(keys, on));
    // A payload of another kind is refused.
    assert!(!engine.send_to(keys, Payload::Control(1.0)));
    assert_eq!(engine.event_id("nope"), None);
    let mut out = [0.0f32; 4];
    engine.render_interleaved(&mut out);
    // Only `keys` ran: `any_note` was not sent to.
    assert_eq!(out, [0.5, 0.0, 0.0, 0.0]);
}

#[test]
fn every_lifted_instance_handles_the_event() {
    let src = "
        event hit note_on
        rill counter(step: Sample) Sample {
            state n: Sample = 0
            on hit { n = n + step }
            return n
        }
        rill main() [Sample; 2] { return counter([1, 10]) }
    ";
    let mut engine = Engine::new(graph(src, 2), config(2)).unwrap();
    engine.send(&note_on(0, 0, 1.0));
    engine.send(&note_on(0, 0, 1.0));
    let mut out = [0.0f32; 2];
    engine.render_interleaved(&mut out);
    assert_eq!(out, [2.0, 20.0]);
}

// ---- sequences ----------------------------------------------------------

/// A rill that shows the pitch of the note playing (semitones above C4, plus
/// one) and 0 when none is, for `seq` started by `on start { start }`.
fn sequence_probe(seq: &str, start: &str) -> String {
    format!(
        "{seq}
        event on_ note_on(sender: s)
        event off_ note_off(sender: s)
        rill main() Sample {{
            state p: Float = 0
            on start {{ {start} }}
            on on_(note) {{ p = (note.pitch - C4) / 1st + 1 }}
            on off_ {{ p = 0 }}
            return p
        }}"
    )
}

/// Where the output changes: (sample, new value).
fn changes(out: &[f32]) -> Vec<(usize, f32)> {
    let mut last = 0.0;
    let mut found = Vec::new();
    for (i, &x) in out.iter().enumerate() {
        if x != last {
            found.push((i, x));
            last = x;
        }
    }
    found
}

#[test]
fn sequence_steps_land_on_exact_samples() {
    // 120bpm in 4/4: an eighth is a quarter of a second, 12000 samples.
    let src = sequence_probe(
        "seq s(step: 1/8, tempo: 120bpm, gate: 0.5) { C4, D4, _, E4 }",
        "invoke s",
    );
    let want = [
        (0, 1.0),
        (6_000, 0.0),
        (12_000, 3.0),
        (18_000, 0.0),
        (36_000, 5.0),
        (42_000, 0.0),
    ];
    for blocks in [Blocks::Fixed(64), Blocks::Fixed(1), Blocks::Fixed(997)] {
        assert_eq!(
            changes(&render_with(&src, 1, 60_000, blocks.clone())),
            want,
            "{blocks:?}"
        );
    }

    // In 7/8 a beat is an eighth: at 120bpm a step is half a second.
    let src = sequence_probe(
        "seq s(meter: 7/8, step: 1/8, tempo: 120bpm, gate: 1) { C4, D4 }",
        "invoke s",
    );
    assert_eq!(
        changes(&render(&src, 60_000)),
        [(0, 1.0), (24_000, 3.0), (48_000, 0.0)]
    );

    // Dotted steps: three sixteenths at 60bpm are 0.75s.
    let src = sequence_probe(
        "seq s(step: 3/16, tempo: 60bpm, gate: 1) { C4, D4 }",
        "invoke s",
    );
    assert_eq!(
        changes(&render(&src, 80_000)),
        [(0, 1.0), (36_000, 3.0), (72_000, 0.0)]
    );
}

#[test]
fn chords_velocities_and_repeats() {
    let src = "
        seq s(step: 1/4, tempo: 240bpm, velocity: 0.5, repeat: 2) { [C4, E4, G4], C4@0.25 }
        event on_ note_on(sender: s)
        rill main() [Sample; 2] {
            state notes: Float = 0
            state level: Float = 0
            on start { invoke s }
            on on_(note) { notes = notes + 1; level = note.velocity }
            return [notes, level]
        }
    ";
    // A quarter at 240bpm is 12000 samples; the sequence plays twice.
    let out = render_with(src, 2, 60_000, Blocks::Fixed(64));
    let at = |i: usize| [out[2 * i], out[2 * i + 1]];
    assert_eq!(at(0), [3.0, 0.5]);
    assert_eq!(at(12_000), [4.0, 0.25]);
    assert_eq!(at(24_000), [7.0, 0.5]);
    assert_eq!(at(36_000), [8.0, 0.25]);
    assert_eq!(at(59_999), [8.0, 0.25], "two passes, then it stops");
}

/// Counts note-ons from `s`, after `handlers` ran.
fn note_counter(seqs: &str, handlers: &str) -> String {
    format!(
        "{seqs}
        event on_ note_on(sender: s)
        event pad note_on(sender: 1)
        rill main() [Sample; 2] {{
            state count: Float = 0
            state last: Float = 0
            {handlers}
            on on_(note) {{ count = count + 1; last = note.instance as Float }}
            return [count, last]
        }}"
    )
}

fn pad(engine: &mut Engine) {
    engine.send(&Event {
        sender: 1,
        channel: 0,
        payload: Payload::NoteOn {
            pitch: 60.0,
            velocity: 1.0,
            instance: 0,
        },
    });
}

/// Run `src`, pressing the pad at each of `presses` (samples), and return
/// the last frame.
fn run_with_pads(src: &str, presses: &[usize], frames: usize) -> [f32; 2] {
    let mut engine = Engine::new(graph(src, 2), config(2)).unwrap();
    let mut out = vec![0.0f32; 2];
    let mut done = 0;
    for &at in presses.iter().chain(std::iter::once(&frames)) {
        let mut chunk = vec![0.0f32; 2 * (at - done)];
        engine.render_interleaved(&mut chunk);
        if let [.., l, r] = chunk[..] {
            out = vec![l, r];
        }
        done = at;
        if at < frames {
            pad(&mut engine);
        }
    }
    [out[0], out[1]]
}

#[test]
fn instances_and_ids() {
    let seq = "seq s(step: 1/4, tempo: 60bpm) { C4, D4 }";
    // Fresh ids: two overlapping copies, the second numbered -2.
    let src = note_counter(seq, "on pad { invoke s }");
    assert_eq!(run_with_pads(&src, &[0, 100], 1_000), [2.0, -2.0]);
    // An id of your own is left alone while it plays.
    let src = note_counter(seq, "on pad { invoke 7 s }");
    assert_eq!(run_with_pads(&src, &[0, 100], 1_000), [1.0, 7.0]);
    // ... and starts again once it has finished.
    assert_eq!(run_with_pads(&src, &[0, 100_000], 101_000), [3.0, 7.0]);
    // The same id with another sequence is another instance.
    let src = note_counter(
        &format!("{seq}\nseq t(step: 1/4) {{ C4 }}\nevent t_on note_on(sender: t)"),
        "on pad { invoke 7 s; invoke 7 t }\non t_on { count = count + 10 }",
    );
    assert_eq!(run_with_pads(&src, &[0], 1_000), [11.0, 7.0]);
    // `invoke` returns the id it used.
    let src = note_counter(
        seq,
        "state id: Int = 0\non pad { id = invoke s; last = id as Float }",
    );
    assert_eq!(run_with_pads(&src, &[0], 10), [1.0, -1.0]);
}

#[test]
fn trigger_starts_at_a_step_and_halt_stops() {
    let seq = "seq s(step: 1/4, tempo: 60bpm, gate: 1) { C4, D4, E4, F4 }";
    // `trigger 3` starts at E4.
    let src = sequence_probe(seq, "trigger 3 s");
    assert_eq!(
        changes(&render(&src, 100_000)),
        [(0, 5.0), (48_000, 6.0), (96_000, 0.0)]
    );
    // A step computed while playing wraps around: 6 of 4 is 2.
    let src = sequence_probe(seq, "let k = 6\ntrigger k s");
    assert_eq!(changes(&render(&src, 10))[0], (0, 3.0));
    // `trigger` restarts an instance that is playing.
    let src = note_counter(seq, "on pad { trigger 1 5 s }");
    assert_eq!(run_with_pads(&src, &[0, 100], 1_000), [2.0, 5.0]);
    // `halt` ends the notes it holds.
    let src = "
        seq s(step: 1/4, tempo: 60bpm, loop: true) { C4 }
        event on_ note_on(sender: s)
        event off_ note_off(sender: s)
        event pad note_on(sender: 1)
        rill main() Sample {
            state p: Float = 0
            on start { invoke 3 s }
            on pad { halt 3 s }
            on on_ { p = 1 }
            on off_ { p = 0 }
            return p
        }
    ";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    let mut out = [0.0f32; 100];
    engine.render_interleaved(&mut out);
    assert_eq!(out[99], 1.0);
    pad(&mut engine);
    engine.render_interleaved(&mut out);
    assert!(out.iter().all(|&x| x == 0.0));
}

#[test]
fn tempo_can_follow_a_stream() {
    // `speed` is a live control; the sequence follows it while playing.
    let src = "
        seq s(step: 1/4, gate: 1, loop: true) { C4, D4 }
        event on_ note_on(sender: s)
        rill main(speed: Freq = 60bpm) Sample {
            state p: Float = 0
            on start { invoke s(tempo: speed) }
            on on_(note) { p = (note.pitch - C4) / 1st + 1 }
            return p
        }
    ";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    let mut out = vec![0.0f32; 48_000];
    engine.render_interleaved(&mut out);
    // One step per second at 60bpm.
    assert_eq!(changes(&out), [(0, 1.0)]);
    engine.set_param("speed", 4.0); // 240bpm, in Hz
    let mut out = vec![0.0f32; 48_000];
    engine.render_interleaved(&mut out);
    // The change is smoothed, but the steps soon come four times as fast.
    let steps = changes(&out).len();
    assert!(steps >= 3, "{:?}", changes(&out));
}

#[test]
fn invoking_at_zero_bpm_starts_nothing() {
    // The tempo comes from a live control at 0: no instance, no sound.
    let src = "
        seq s(step: 1/4, gate: 1) { C4 }
        event on_ note_on(sender: s)
        rill main(speed: Freq = 0Hz) Sample {
            state count: Float = 0
            state id: Int = 0
            on start { id = invoke s(tempo: speed) }
            on on_ { count = count + 1 }
            return count + id as Float * 0
        }
    ";
    assert!(render(src, 1_000).iter().all(|&x| x == 0.0));
}

#[test]
fn captured_values_stay_with_their_instance() {
    // The tempo mixes a stream (`speed`) with the pad's velocity, captured
    // when each instance starts.
    let src = "
        seq s(step: 1/4, gate: 1) { C4, C4, C4, C4 }
        event on_ note_on(sender: s)
        event pad note_on(sender: 1)
        rill main(speed: Freq = 60bpm) Sample {
            state count: Float = 0
            on pad(hit) { invoke s(tempo: speed * hit.velocity * 4) }
            on on_ { count = count + 1 }
            return count
        }
    ";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    engine.send(&Event {
        sender: 1,
        channel: 0,
        payload: Payload::NoteOn {
            pitch: 60.0,
            velocity: 0.5,
            instance: 0,
        },
    });
    // 60bpm × 0.5 × 4 = 120bpm: a step every half second.
    let mut out = vec![0.0f32; 48_000];
    engine.render_interleaved(&mut out);
    assert_eq!(changes(&out), [(0, 1.0), (24_000, 2.0)]);
}

#[test]
fn notes_carry_their_instance() {
    let src = "
        seq s { C4 }
        event any note_on
        rill main() Sample {
            state last: Float = 99
            on start { invoke 4 s }
            on any(note) { last = note.instance as Float }
            return last
        }
    ";
    let mut engine = Engine::new(graph(src, 1), config(1)).unwrap();
    let mut out = [0.0f32; 1];
    engine.render_interleaved(&mut out);
    assert_eq!(out[0], 4.0);
    pad(&mut engine);
    engine.render_interleaved(&mut out);
    assert_eq!(out[0], 0.0, "host notes have instance 0");
}

#[test]
fn start_and_invoked_events() {
    let src = "
        event ping control_change(channel: 1)
        event pong control_change(channel: 2)
        rill main() [Sample; 2] {
            state a: Float = 0
            state b: Float = 0
            on start { a = 1; invoke ping(value: 0.5) }
            on ping(v) { b = v; invoke pong(value: v * 2) }
            on pong(v) { a = a + v }
            return [a, b]
        }
    ";
    assert_eq!(render_with(src, 2, 1, Blocks::Fixed(1)), [2.0, 0.5]);
}

// ---- voices -------------------------------------------------------------

const VOICE: &str = "
event keys_on note_on(sender: 1)
event keys_off note_off(sender: 1)
rill voice() Sample {
    state pitch: Float = 0
    state level: Float = 0
    state gate: Float = 0
    on keys_on(note) claim { pitch = (note.pitch - C4) / 1st + 1; level = 1; gate = 1 }
    on keys_off release { gate = 0 }
    // After release the sound dies away slowly, like a reverb tail.
    level = if gate > 0 { level } else { level * 0.5 }
    return pitch * level
}
";

fn key(engine: &mut Engine, on: bool, pitch: f32) {
    let payload = if on {
        Payload::NoteOn {
            pitch,
            velocity: 1.0,
            instance: 0,
        }
    } else {
        Payload::NoteOff {
            pitch,
            release: 0.0,
            instance: 0,
        }
    };
    engine.send(&Event {
        sender: 1,
        channel: 0,
        payload,
    });
}

fn voices(engine: &mut Engine) -> Vec<f32> {
    let mut out = [0.0f32; 2];
    engine.render_interleaved(&mut out[..2]);
    out.to_vec()
}

#[test]
fn claimed_notes_go_to_one_voice_each() {
    let src = format!("{VOICE}\nrill main() [Sample; 2] {{ return [voice(); 2] }}");
    let mut engine = Engine::new(graph(&src, 2), config(2)).unwrap();
    let (c4, e4, g4) = (60.0, 64.0, 67.0);
    key(&mut engine, true, c4);
    key(&mut engine, true, e4);
    assert_eq!(voices(&mut engine), [1.0, 5.0], "one note per voice");
    // The release goes to the voice holding E4 only.
    key(&mut engine, false, e4);
    let after = voices(&mut engine);
    assert_eq!(after[0], 1.0);
    assert!(after[1] < 5.0, "E4 is fading: {after:?}");
    // The fading voice is still busy, so a new note while both are taken
    // steals the voice released longest ago: the fading one.
    key(&mut engine, true, g4);
    assert_eq!(voices(&mut engine), [1.0, 8.0]);
    // With everything held, the oldest note is stolen.
    key(&mut engine, true, e4);
    assert_eq!(voices(&mut engine), [5.0, 8.0]);
}

#[test]
fn a_voice_is_free_once_silent_for_its_tail() {
    let src = format!(
        "{}\nrill main() [Sample; 3] {{ return [voice(); 3] }}",
        VOICE.replace("claim {", "claim(tail: 1ms) {")
    );
    let mut engine = Engine::new(graph(&src, 3), config(3)).unwrap();
    key(&mut engine, true, 60.0);
    key(&mut engine, false, 60.0);
    // The tail halves every sample: below -90dB after about 30 samples, then
    // 1ms (48 samples) of silence.
    let mut out = vec![0.0f32; 3 * 60];
    engine.render_interleaved(&mut out);
    key(&mut engine, true, 62.0);
    let mut out = [0.0f32; 3];
    engine.render_interleaved(&mut out);
    // Voice 1 is still releasing, so the free voice 2 takes the note.
    assert_eq!(out[1..], [3.0, 0.0], "{out:?}");
    let mut out = vec![0.0f32; 3 * 100];
    engine.render_interleaved(&mut out);
    key(&mut engine, true, 64.0);
    let mut out = [0.0f32; 3];
    engine.render_interleaved(&mut out);
    // Voice 1 became free, so it takes the next note before voice 3.
    assert_eq!(out, [5.0, 3.0, 0.0]);
}

#[test]
fn sequences_play_chords_over_voices() {
    let src = "
        seq s(step: 1/4, gate: 0.5) { [C4, E4, G4] }
        event on_ note_on(sender: s)
        event off_ note_off(sender: s)
        rill voice() Sample {
            state p: Float = 0
            on on_(note) claim { p = (note.pitch - C4) / 1st + 1 }
            on off_ release { p = 0 }
            return p
        }
        rill main() [Sample; 3] {
            on start { invoke s }
            return [voice(); 3]
        }
    ";
    assert_eq!(render_with(src, 3, 1, Blocks::Fixed(1)), [1.0, 5.0, 8.0]);
}

// ---- running repeated code together --------------------------------------

/// Render `src` with and without running repeated code as lanes, sending
/// `events` at the given frames, and check the two agree bit for bit.
/// Returns how many instructions per sample each build has.
fn same_either_way(
    src: &str,
    channels: usize,
    frames: usize,
    events: &[(usize, Event)],
) -> (usize, usize) {
    let run = |vectorize: bool| {
        let options = lang::build::Options {
            vectorize,
            seed: Some(7),
        };
        let (graph, _) = lang::load_with(src, &config(channels), "main", &options)
            .unwrap_or_else(|d| panic!("{d:#?}"));
        let mut engine = Engine::new(graph, config(channels)).unwrap();
        let size = engine.program_size().0;
        let mut out = vec![0.0f32; frames * channels];
        let mut done = 0;
        for &(at, event) in events
            .iter()
            .chain(std::iter::once(&(frames, note_on(0, 0, 0.0))))
        {
            engine.render_interleaved(&mut out[done * channels..at * channels]);
            if at < frames {
                engine.send(&event);
            }
            done = at;
        }
        (out, size)
    };
    let (vector, vector_size) = run(true);
    let (scalar, scalar_size) = run(false);
    for (i, (v, s)) in vector.iter().zip(&scalar).enumerate() {
        assert_eq!(
            v.to_bits(),
            s.to_bits(),
            "sample {i}: {v} (lanes) vs {s} (scalar)"
        );
    }
    (vector_size, scalar_size)
}

#[test]
fn running_together_changes_nothing_in_the_examples() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rill") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        // Examples being worked on may not build; they have their own test.
        if lang::load(&src, &config(2), "main").is_err() {
            continue;
        }
        same_either_way(&src, 2, 20_000, &[]);
    }
}

#[test]
fn running_together_with_branches_state_and_loops() {
    let src = "
        fn blep(t: Float, dt: Float) Float {
            if t < dt {
                let x = t / dt
                x + x - x * x - 1
            } else if t > 1 - dt {
                let x = (t - 1) / dt
                x * x + x + x + 1
            } else {
                0
            }
        }
        rill saw(freq: Freq, offset: Float = 0) Sample {
            state phase: Float = 0
            phase = wrap(phase + freq / RATE)
            let p = wrap(phase + offset)
            return p * 2 - 1 - blep(p, freq / RATE)
        }
        // State changed only on some ticks, differently per copy.
        rill gate(x: Sample, every: Int) Sample {
            state n: Int = 0
            state held: Sample = 0
            n = n + 1
            if n % every == 0 {
                held = x
            }
            return held
        }
        rill main() [Sample; 2] {
            let freqs = [C4, E4, G4, B4] |> equal
            let voices: [Sample; 8]
            for i in 0..8 {
                voices[i] = saw(freqs[i % 4] * (1 + i as Float * 0.001), i as Float * 0.1)
            }
            let gated = gate(voices, [1, 2, 3, 4, 5, 6, 7, 8])
            let scaled = gated * [1, 0.5, 0.25, 1, 2, 1, 0.75, 1]
            return [sum(scaled), sum(voices)] * 0.1
        }
    ";
    let (lanes, scalar) = same_either_way(src, 2, 30_000, &[]);
    assert!(
        lanes < scalar / 2,
        "{lanes} vs {scalar}: the copies did not run together"
    );
}

#[test]
fn running_together_with_voices_and_events() {
    let src = "
        event keys_on note_on(sender: 1)
        event keys_off note_off(sender: 1)
        event knob control_change(sender: 1, channel: 7)
        rill voice() Sample {
            state phase: Float = 0
            state pitch: Pitch = C4
            state level: Float = 0
            state gate: Float = 0
            state bright: Float = 0.5
            on keys_on(note) claim(tail: 2ms) { pitch = note.pitch; level = note.velocity; gate = 1 }
            on keys_off release { gate = 0 }
            on knob(v) { bright = v }
            phase = wrap(phase + (pitch |> equal) / RATE)
            level = if gate > 0 { level } else { level * 0.99 }
            return (phase * 2 - 1) * level * bright
        }
        rill main() Sample {
            return sum([voice(); 6])
        }
    ";
    let key = |on: bool, pitch: f32| Event {
        sender: 1,
        channel: 0,
        payload: if on {
            Payload::NoteOn {
                pitch,
                velocity: 0.8,
                instance: 0,
            }
        } else {
            Payload::NoteOff {
                pitch,
                release: 0.0,
                instance: 0,
            }
        },
    };
    let knob = Event {
        sender: 1,
        channel: 7,
        payload: Payload::Control(0.9),
    };
    let events = [
        (10, key(true, 60.0)),
        (500, key(true, 64.0)),
        (900, key(true, 67.0)),
        (2_000, key(false, 64.0)),
        (2_500, knob),
        (3_000, key(true, 71.0)),
        (6_000, key(false, 60.0)),
        (6_100, key(false, 67.0)),
        (9_000, key(true, 72.0)),
    ];
    let (lanes, scalar) = same_either_way(src, 1, 12_000, &events);
    assert!(
        lanes < scalar,
        "{lanes} vs {scalar}: the voices did not run together"
    );
}

#[test]
fn running_together_with_sequences() {
    let src = "
        seq riff(step: 1/16, tempo: 140bpm, gate: 0.7) { C4, E4@0.5, [G4, B4], _, C5, A4@1 }
        event on_ note_on(sender: riff)
        event off_ note_off(sender: riff)
        rill voice() Sample {
            state phase: Float = 0
            state pitch: Pitch = C4
            state level: Float = 0
            on on_(note) claim { pitch = note.pitch; level = note.velocity }
            on off_ release { level = 0 }
            phase = wrap(phase + (pitch |> equal) / RATE)
            return sin(phase * TAU) * level
        }
        rill main() Sample {
            on start { invoke riff(loop: true) }
            return sum([voice(); 4]) * 0.25
        }
    ";
    same_either_way(src, 1, 48_000, &[]);
}

// ---- lining shapes up, `each` and `random()` -------------------------------

/// Render `src` built with `seed` for `random()`.
fn render_seeded(src: &str, channels: usize, frames: usize, seed: Option<u64>) -> Vec<f32> {
    let options = lang::build::Options {
        seed,
        ..Default::default()
    };
    let (graph, _) = lang::load_with(src, &config(channels), "main", &options)
        .unwrap_or_else(|d| panic!("{d:#?}"));
    let mut engine = Engine::new(graph, config(channels)).unwrap();
    offline::render(&mut engine, frames, &Blocks::Fixed(1))
}

#[test]
fn shorter_shapes_reach_the_right_copies() {
    let src = "
        rill mul(x: Sample, g: Float) Sample { return x * g }
        rill main() [Sample; 4] {
            let b: [[Sample; 2]; 3] = [[1, 2], [3, 4], [5, 6]]
            let inner = b * [10, 100]
            let outer = b * [1, 2, 3]
            let lifted_inner = mul(b, [10, 100])
            let lifted_outer = mul(b, [1, 2, 3])
            return [inner[2][1], outer[2][0], lifted_inner[1][1], lifted_outer[1][0]]
        }
    ";
    assert_eq!(
        render_with(src, 4, 1, Blocks::Fixed(1)),
        [600.0, 15.0, 400.0, 6.0]
    );
}

const PICK: &str = "
rill pick(x: Sample, r: Float) Sample { return r }
";

#[test]
fn each_makes_a_value_per_copy() {
    let shared = format!(
        "{PICK}
        rill main() [Sample; 4] {{
            let v: [Sample; 4] = [0, 0, 0, 0]
            return pick(v, random())
        }}"
    );
    let out = render_seeded(&shared, 4, 1, Some(1));
    assert!(out.iter().all(|&x| x == out[0]), "{out:?}");

    let each = format!(
        "{PICK}
        rill main() [Sample; 6] {{
            let v: [[Sample; 2]; 3] = [[0, 0], [0, 0], [0, 0]]
            let p = v |> pick(r: each random())
            return [p[0][0], p[0][1], p[1][0], p[1][1], p[2][0], p[2][1]]
        }}"
    );
    let out = render_seeded(&each, 6, 1, Some(1));
    for (i, x) in out.iter().enumerate() {
        assert!((0.0..1.0).contains(x), "{out:?}");
        assert!(out[..i].iter().all(|y| y != x), "{out:?}");
    }
}

#[test]
fn each_gives_every_copy_its_own_rill_instance() {
    // A shared instance would count up once per copy every tick.
    let src = format!(
        "{PICK}
        rill acc(step: Float) Float {{
            state t: Float = 0
            t += step
            return t
        }}
        rill main() [Sample; 4] {{
            let v: [Sample; 4] = [0, 0, 0, 0]
            return pick(v, r: each acc(random()))
        }}"
    );
    let out = render_seeded(&src, 4, 3, Some(3));
    let (first, third) = (&out[..4], &out[8..]);
    for (a, c) in first.iter().zip(third) {
        assert!((c - 3.0 * a).abs() < 1e-5, "{out:?}");
    }
    assert!(first.iter().skip(1).any(|x| *x != first[0]), "{out:?}");
}

#[test]
fn random_is_picked_from_the_seed() {
    let src = main_returning("random()");
    let one = render_seeded(&src, 1, 2, Some(42));
    assert_eq!(one, render_seeded(&src, 1, 2, Some(42)));
    assert_ne!(one, render_seeded(&src, 1, 2, Some(43)));
    // Without a seed, every build picks a new one.
    assert_ne!(
        render_seeded(&src, 1, 1, None),
        render_seeded(&src, 1, 1, None)
    );
    // Picked once: the same number every sample.
    assert_eq!(one[0], one[1]);
    assert!((0.0..1.0).contains(&one[0]));
}

#[test]
fn random_picks_within_its_range() {
    let src = "
        rill main() [Sample; 4] {
            return [
                random(lo: 100Hz, hi: 200Hz) / 1Hz,
                amp(random(-12dB, 0dB)),
                random(-1, 1),
                random(),
            ]
        }
    ";
    for seed in 0..50 {
        let out = render_seeded(src, 4, 1, Some(seed));
        assert!((100.0..200.0).contains(&out[0]), "{out:?}");
        assert!((0.25..=1.0).contains(&out[1]), "{out:?}");
        assert!((-1.0..1.0).contains(&out[2]), "{out:?}");
        assert!((0.0..1.0).contains(&out[3]), "{out:?}");
    }
    // Each `random()` picks its own number.
    let out = render_seeded(&main_returning("random() - random()"), 1, 1, Some(5));
    assert_ne!(out[0], 0.0);
}

#[test]
fn random_numbers_are_constants() {
    let src = "
        rill main() Sample {
            let x = random(lo: 0.2, hi: 0.4)
            return sin(x * TAU) * random()
        }
    ";
    let options = lang::build::Options {
        seed: Some(9),
        ..Default::default()
    };
    let (graph, _) = lang::load_with(src, &config(1), "main", &options).unwrap();
    let engine = Engine::new(graph, config(1)).unwrap();
    assert_eq!(engine.program_size().0, 0);
}

#[test]
fn running_together_with_each() {
    let src = "
        fn blep(t: Float, dt: Float) Float {
            if t < dt { let x = t / dt; x + x - x * x - 1 }
            else if t > 1 - dt { let x = (t - 1) / dt; x * x + x + x + 1 }
            else { 0 }
        }
        rill saw(freq: Freq, offset: Float = 0) Sample {
            state phase: Float = 0
            phase = wrap(phase + freq / RATE)
            let p = wrap(phase + offset)
            return p * 2 - 1 - blep(p, freq / RATE)
        }
        rill flip(x: Sample, r: Float) Sample {
            if r < 0.5 { return -x }
            return x
        }
        rill main() [Sample; 2] {
            let freqs = [[110Hz, 111Hz, 112Hz, 113Hz], [220Hz, 221Hz, 222Hz, 223Hz]]
            let voices = freqs |> saw(offset: each random()) |> flip(each random())
            return [sum(voices[0]), sum(voices[1])] * [0.25, 0.25]
        }
    ";
    same_either_way(src, 2, 2000, &[]);
}

// ---- the events sequences make -------------------------------------------

/// Handlers that log what a sequence called `riff` does, as digits, one
/// per event in the order they arrive within a sample. `life`: note on 1,
/// note off 2, start 3, finished 4, halted 5, replaced 6, end 7, repeated
/// 8. `time`: bar 1, beat 2, step 3, rest 4, note on 5.
const RIFF_LOG: &str = "
    state life: Float = 0
    state time: Float = 0
    on riff_note_on { life = life * 10 + 1; time = time * 10 + 5 }
    on riff_note_off { life = life * 10 + 2 }
    on riff_start { life = life * 10 + 3 }
    on riff_finished { life = life * 10 + 4 }
    on riff_halted { life = life * 10 + 5 }
    on riff_replaced { life = life * 10 + 6 }
    on riff_end { life = life * 10 + 7 }
    on riff_repeated { life = life * 10 + 8 }
    on riff_bar { time = time * 10 + 1 }
    on riff_beat { time = time * 10 + 2 }
    on riff_step { time = time * 10 + 3 }
    on riff_rest { time = time * 10 + 4 }
    let out = [life, time]
    life = 0
    time = 0
";

/// Render `main` (with [`RIFF_LOG`] at the top of its body and `body`
/// after) for `frames`, sending a control change `value` from sender 1 at
/// each `(frame, value)`. Returns the frames where something happened,
/// with what.
fn riff_log(
    head: &str,
    body: &str,
    frames: usize,
    sends: &[(usize, f32)],
) -> Vec<(usize, u32, u32)> {
    let src = format!("{head}\nrill main() [Sample; 2] {{\n{RIFF_LOG}\n{body}\nreturn out\n}}");
    let mut engine = Engine::new(graph(&src, 2), config(2)).unwrap();
    let mut out = vec![0.0f32; frames * 2];
    let mut done = 0;
    for &(at, value) in sends.iter().chain(std::iter::once(&(frames, 0.0))) {
        engine.render_interleaved(&mut out[done * 2..at * 2]);
        if at < frames {
            engine.send(&Event {
                sender: 1,
                channel: 0,
                payload: Payload::Control(value),
            });
        }
        done = at;
    }
    out.chunks(2)
        .enumerate()
        .filter(|(_, f)| f[0] != 0.0 || f[1] != 0.0)
        .map(|(i, f)| (i, f[0] as u32, f[1] as u32))
        .collect()
}

// At 48000bpm a beat is 60 samples, so an eighth-note step in 4/4 is 30.

#[test]
fn a_sequence_tells_what_it_does() {
    let head = "seq riff(step: 1/8, tempo: 48000bpm, gate: 0.5, repeat: 2) { C4, _, E4 }";
    let log = riff_log(head, "on start { invoke riff }", 400, &[]);
    assert_eq!(
        log,
        [
            // Start, then bar 1, beat 1, step 1 and its note.
            (0, 31, 1235),
            (15, 2, 0),
            // Step 2 is a rest.
            (30, 0, 34),
            // Beat 2, step 3.
            (60, 1, 235),
            (75, 2, 0),
            // The pattern is 1.5 beats; the second pass starts on bar 1.
            (90, 81, 1235),
            (105, 2, 0),
            (120, 0, 34),
            (150, 1, 235),
            (165, 2, 0),
            // Finished once the last step is over, then the end.
            (180, 47, 0),
        ]
    );
}

#[test]
fn sequences_restart_replace_and_halt() {
    let head = "
        seq riff(step: 1/8, tempo: 48000bpm, gate: 0.5, loop: true, instances: 2) { C4, D4 }
        event go control_change(sender: 1)
    ";
    let body = "
        on start { invoke 7 riff }
        on go(v) {
            if v == 1 { trigger 2 7 riff }
            if v == 2 { invoke riff; invoke riff }
            if v == 3 { halt riff }
        }
    ";
    let log = riff_log(head, body, 100, &[(10, 1.0), (50, 2.0), (70, 3.0)]);
    assert_eq!(
        log,
        [
            (0, 31, 1235),
            // Restarting at step 2: the held note ends, `halted` and `end`,
            // then `start` and step 2 (no beat starts there).
            (10, 25731, 35),
            (25, 2, 0),
            (40, 81, 1235),
            // Two starts with room for one more: the first takes the free
            // instance, the second replaces the oldest (ending its note).
            // Both then play step 1.
            (50, 3267311, 12351235),
            (65, 22, 0),
            // Halting both.
            (70, 5757, 0),
        ]
    );
}

#[test]
fn bars_and_beats_follow_the_meter() {
    // 3/4 with eighth-note steps: 8 steps are 4 beats, a bar and a short one.
    let head =
        "seq riff(meter: 3/4, step: 1/8, tempo: 48000bpm, repeat: 2) { C4, _, _, _, _, _, _, _ }";
    let src = format!(
        "{head}
        rill main() [Sample; 4] {{
            state beat: Float = 0
            state bar: Float = 0
            state pass: Float = 0
            state start: Float = 0
            on start {{ trigger 3 riff }}
            on riff_start(s) {{ start = (s.step * 100 + s.instance) as Float }}
            on riff_beat(b) {{
                beat = b.beat as Float
                bar = b.bar as Float
                pass = b.pass as Float
            }}
            return [beat, bar, pass, start]
        }}"
    );
    let out = render_with(&src, 4, 400, Blocks::Fixed(64));
    let at = |f: usize| [out[f * 4], out[f * 4 + 1], out[f * 4 + 2]];
    // `trigger 3` starts on a beat: beat 2 of bar 1, at once.
    assert_eq!(out[3], 300.0 - 1.0, "step 3, fresh id -1");
    assert_eq!(at(0), [2.0, 1.0, 1.0]);
    assert_eq!(at(60), [3.0, 1.0, 1.0]);
    assert_eq!(at(120), [1.0, 2.0, 1.0]);
    // The second pass starts on bar 1 again.
    assert_eq!(at(180), [1.0, 1.0, 2.0]);
    assert_eq!(at(300), [3.0, 1.0, 2.0]);
    assert_eq!(at(360), [1.0, 2.0, 2.0]);
}

#[test]
fn beats_can_fall_between_steps() {
    // Dotted eighths in 4/4: steps every 0.75 beats.
    let head = "seq riff(step: 3/16, tempo: 48000bpm, gate: 0.5) { C4, C4, C4, C4 }";
    let log = riff_log(head, "on start { invoke riff }", 300, &[]);
    let times: Vec<(usize, u32)> = log
        .iter()
        .map(|&(f, _, t)| (f, t))
        .filter(|&(_, t)| t != 0)
        .collect();
    assert_eq!(
        times,
        [(0, 1235), (45, 35), (60, 2), (90, 35), (120, 2), (135, 35)]
    );
    assert_eq!(log.last(), Some(&(180, 47, 0)));
}

#[test]
fn voices_claim_a_sequences_own_notes() {
    let src = "
        seq riff(step: 1/4, tempo: 480bpm) { [C4, E4, G4] }
        rill voice() Sample {
            state level: Float = 0
            on riff_note_on(n) claim { level = n.velocity }
            on riff_note_off release { level = 0 }
            return level
        }
        rill main() [Sample; 3] {
            on start { invoke riff(velocity: 0.5) }
            return [voice(); 3]
        }
    ";
    assert_eq!(render_with(src, 3, 1, Blocks::Fixed(1)), [0.5, 0.5, 0.5]);
}

#[test]
fn restarting_when_finished() {
    // Each run finishes, and its handler starts the next one.
    let src = "
        seq riff(step: 1/8, tempo: 48000bpm) { C4, C4 }
        rill main() Sample {
            state runs: Float = 0
            on start { invoke riff }
            on riff_finished { invoke riff }
            on riff_start { runs = runs + 1 }
            return runs
        }
    ";
    let out = render(src, 200);
    // A run lasts 60 samples.
    assert_eq!((out[0], out[59], out[60], out[199]), (1.0, 1.0, 2.0, 4.0));
}

#[test]
fn sequence_facts_are_constants() {
    let src = "
        seq riff(meter: 3/4, step: 1/16, instances: 8) { C4, _, E4, _, G4, _ }
        rill main() [Sample; 4] {
            let accents = [0.5; riff.step_count]
            let pads: [Float; riff.instances / 4] = [1, 1]
            return [sum(accents), sum(pads), riff.bar_count, riff.steps_per_beat]
        }
    ";
    assert_eq!(
        render_with(src, 4, 1, Blocks::Fixed(1)),
        [3.0, 2.0, 0.5, 4.0]
    );
}

#[test]
fn running_together_with_sequence_events() {
    let src = "
        seq riff(meter: 3/4, step: 1/8, tempo: 300bpm, gate: 0.8, repeat: 3) {
            [C4, E4], _, G4, [D4, F4, A4], _, B4,
        }
        rill sine(freq: Freq) Sample {
            state phase: Float = 0
            phase = wrap(phase + freq / RATE)
            return sin(phase * TAU)
        }
        rill voice() Sample {
            state pitch: Pitch = C4
            state level: Float = 0
            state accent: Float = 1
            on riff_note_on(n) claim { pitch = n.pitch; level = n.velocity }
            on riff_note_off release { level = 0 }
            on riff_bar { accent = 1.5 }
            on riff_beat(b) { accent = if b.beat == 1 { 1.5 } else { 1 } }
            return sine(pitch |> equal) * level * accent
        }
        rill main() [Sample; 2] {
            state steps: Float = 0
            on start { invoke riff }
            on riff_finished { invoke riff(tempo: 400bpm) }
            on riff_step(s) { steps = s.step as Float / riff.step_count as Float }
            let mix = sum([voice(); 4]) * 0.2
            return [mix, mix * steps]
        }
    ";
    same_either_way(src, 2, 48_000, &[]);
}
