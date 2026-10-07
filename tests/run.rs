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
    // A shorter frame lines up with the outer layer: one gain per bus.
    let src = "rill main() [Sample; 2] { return sum([[1, 2], [3, 4]] * [10, 100]) }";
    assert_eq!(stereo(src, 1), [[310.0, 420.0]]);

    // One level per voice.
    let src = "rill main() [Sample; 2] { return sum([[1, 1], [1, 1]] + [0dB, -6dB]) }";
    let out = stereo(src, 1);
    assert!((out[0][0] - 1.501).abs() < 1e-3, "{out:?}");

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
