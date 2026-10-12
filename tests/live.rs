//! Swapping a running program for a new build: state carried across by
//! name, a crossfade, and nothing allocated on the audio side.

use rill::Config;
use rill::lang::build::Options;
use rill::live::{Live, Player};

const RATE: u32 = 48_000;

fn config() -> Config {
    Config {
        sample_rate: RATE,
        max_frames: 64,
        out_channels: 2,
    }
}

fn start(src: &str) -> (Live, Player) {
    let options = Options {
        seed: Some(7),
        ..Default::default()
    };
    let (live, player, _) = Live::from_text(src, config(), "main", options)
        .unwrap_or_else(|f| panic!("{:#?}", f.errors));
    (live, player)
}

fn render(player: &mut Player, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0; frames * 2];
    player.render_interleaved(&mut out);
    out
}

/// Play `before` for `at` frames, reload with `after`, play on to `frames`.
fn swapped(before: &str, after: &str, at: usize, frames: usize, fade: usize) -> (Vec<f32>, usize) {
    let (mut live, mut player) = start(before);
    live.set_fade(fade);
    let mut out = render(&mut player, at);
    let report = live
        .reload_text(after)
        .unwrap_or_else(|f| panic!("{:#?}", f.errors))
        .report;
    out.extend(render(&mut player, frames - at));
    (out, report.fresh)
}

fn unswapped(src: &str, frames: usize) -> Vec<f32> {
    let (_live, mut player) = start(src);
    render(&mut player, frames)
}

/// Oscillators, a delay full of echoes, noise, `random()`, a sequence
/// playing and voices holding its notes.
const SONG: &str = "
import \"@gen/osc\"
import \"@effects/delay\"
import \"@effects/filter\"

seq riff(step: 1/16, tempo: 140bpm, loop: true) { C4, E4, G4, _, B4 }

rill voice() Sample {
    state pitch: Pitch = C4
    state level: Float = 0
    on riff_note_on(note) claim { pitch = note.pitch
        level = note.velocity }
    on riff_note_off release { level = 0 }
    let tone = saw(pitch |> equal, offset: random())
    return lowpass(tone, 2kHz) * level
}

rill main() [Sample; 2] {
    on start { invoke riff }
    let voices = sum([voice(); 4])
    let lead = saw(110Hz) * 0.1
    let hiss = noise() * 0.01
    let wet = comb(voices + lead, 30ms, feedback: 0.6)
    return [voices + wet + hiss, lead + wet]
}
";

#[test]
fn the_same_program_carries_on_exactly() {
    for fade in [0, 960] {
        let (out, fresh) = swapped(SONG, SONG, 7_000, 20_000, fade);
        assert_eq!(fresh, 0);
        assert!(out == unswapped(SONG, 20_000), "fade {fade}");
    }
}

#[test]
fn moved_and_renamed_lets_keep_their_state() {
    // `lead` and `hiss` swapped around, and `wet` renamed: the same program.
    let moved = SONG
        .replace(
            "    let lead = saw(110Hz) * 0.1\n    let hiss = noise() * 0.01\n",
            "    let hiss = noise() * 0.01\n    let lead = saw(110Hz) * 0.1\n",
        )
        .replace("wet", "echoes");
    assert_ne!(moved, SONG);
    let (out, fresh) = swapped(SONG, &moved, 9_000, 20_000, 960);
    assert_eq!(fresh, 0);
    assert!(out == unswapped(SONG, 20_000));
}

#[test]
fn an_edit_changes_only_what_it_touches() {
    // A different cutoff: the oscillators and the sequence carry on.
    let edited = SONG.replace("lowpass(tone, 2kHz)", "lowpass(tone, 1kHz)");
    let (out, fresh) = swapped(SONG, &edited, 9_000, 60_000, 960);
    assert_eq!(fresh, 0);
    let before = unswapped(SONG, 60_000);
    let after = unswapped(&edited, 60_000);
    // Up to the swap, the old program; once the old sound has left the
    // filters and the echoes (which carry on, so they take a while), what
    // the edited program would have played had it run from the start.
    assert!(out[..9_000 * 2] == before[..9_000 * 2]);
    let off = out[40_000 * 2..]
        .iter()
        .zip(&after[40_000 * 2..])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max);
    assert!(off < 1e-3, "{off}");
}

#[test]
fn the_crossfade_has_no_jumps() {
    // A sound that changes completely: the fade glides between them.
    let a = "import \"@gen/osc\"\nrill main() [Sample; 2] {\n    let x = sine(220Hz) * 0.5\n    return [x, x]\n}";
    let b = "import \"@gen/osc\"\nrill main() [Sample; 2] {\n    let x = sine(220Hz) * 0.5 + 0.4\n    return [x, x]\n}";
    let (out, _) = swapped(a, b, 1_000, 4_000, 960);
    let left: Vec<f32> = out.iter().step_by(2).copied().collect();
    let biggest = left
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    // A 220Hz sine at 0.5 moves at most 0.0144 per sample; the 0.4 offset
    // fades in over 960 samples, adding at most 0.4 · π / 2 / 960.
    assert!(biggest < 0.0144 + 0.001, "{biggest}");
    // Without a fade the offset arrives at once.
    let (out, _) = swapped(a, b, 1_000, 4_000, 0);
    let left: Vec<f32> = out.iter().step_by(2).copied().collect();
    let biggest = left
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    assert!(biggest > 0.35, "{biggest}");
}

#[test]
fn noise_running_together_carries_on_exactly() {
    // Noise in a pool runs as one vector block; a rebuild carries every
    // generator over by its key, as it does one copy at a time.
    let src = "
import \"@gen/noise\"
rill main() [Sample; 2] {
    let x = sum([pink(); 32]) * 0.1
    return [x, sum([noise(); 8]) * 0.1]
}";
    let (out, fresh) = swapped(src, src, 3_000, 10_000, 960);
    assert_eq!(fresh, 0);
    assert!(out == unswapped(src, 10_000));
}

#[test]
fn a_broken_edit_keeps_the_old_program() {
    let (mut live, mut player) = start(SONG);
    let mut out = render(&mut player, 5_000);
    assert!(
        live.reload_text("rill main() [Sample; 2] { return oops }")
            .is_err()
    );
    out.extend(render(&mut player, 5_000));
    assert!(out == unswapped(SONG, 10_000));
}

#[test]
fn a_slope_past_its_max_grows_the_filter() {
    let src = "
import \"@gen/osc\"
import \"@effects/filter\"
rill main(slope: Slope = 12dB / 1oct) [Sample; 2] {
    let y = lowpass(sine(2kHz), 500Hz, slope: slope, max: 24dB / 1oct)
    return [y, y]
}";
    let (mut live, mut player) = start(src);
    live.set_fade(0);
    render(&mut player, 4_000);
    assert!(live.grow().is_none(), "nothing asked for more yet");
    // 96 dB/oct is 16 poles, past the 4 built for 24.
    player.set_param("slope", 96.0);
    let held = rms(&render(&mut player, 4_000)[4_000..]);
    // Held at 24 dB/oct: two octaves above, about 48 dB down.
    assert!(held > 1e-4, "{held}");
    let grown = live.grow().expect("a ceiling was passed").unwrap();
    // The stages already built keep their state; the new ones start fresh.
    assert_eq!(grown.report.carried, 7, "{:?}", grown.report);
    assert!(grown.report.fresh > 20, "{:?}", grown.report);
    // 16 poles asked, so room for 24.
    assert!(
        live.raised().values().all(|&v| (v - 24.0).abs() < 1e-3),
        "{:?}",
        live.raised()
    );
    let after = rms(&render(&mut player, 8_000)[8_000..]);
    // Now 96 dB/oct: nearly nothing gets through.
    assert!(after < held / 1000.0, "{after} vs {held}");
    // Growing again soon after, with nothing more asked, does nothing.
    assert!(live.grow().is_none());
}

#[test]
fn a_delay_past_its_line_grows_and_keeps_its_echoes() {
    let src = "
import \"@gen/osc\"
rill main(time: Time = 10ms) [Sample; 2] {
    let x = sine(440Hz)
    let y = delay(x, time, max: 20ms)
    return [x, y]
}";
    let (mut live, mut player) = start(src);
    live.set_fade(0);
    render(&mut player, 2_000);
    player.set_param("time", 0.1);
    render(&mut player, 1_000);
    live.grow().expect("the line was too short").unwrap();
    let ceiling = live.raised().values().copied().fold(0.0, f32::max);
    assert!((ceiling - 0.15).abs() < 1e-3, "{ceiling}");
    // Once 100ms have gone by, the right channel is the left one 100ms ago.
    let out = render(&mut player, 12_000);
    let (left, right): (Vec<f32>, Vec<f32>) = out.chunks(2).map(|f| (f[0], f[1])).unzip();
    for i in 6_000..12_000 {
        assert!(
            (right[i] - left[i - 4_800]).abs() < 1e-3,
            "{i}: {} vs {}",
            right[i],
            left[i - 4_800]
        );
    }
}

fn rms(out: &[f32]) -> f32 {
    (out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt()
}

#[test]
fn a_different_channel_count_is_refused() {
    // The device's stream is set up for two channels; the old program plays on.
    let (mut live, mut player) = start(SONG);
    let mut out = render(&mut player, 5_000);
    let three = "import \"@gen/osc\"\nrill main() [Sample; 3] {\n    let x = sine(220Hz)\n    return [x, x, x]\n}";
    let Err(failed) = live.reload_text(three) else {
        panic!("a 3-channel program was swapped into a 2-channel stream");
    };
    assert!(
        failed.errors[0].message.contains("returns 3 channels"),
        "{:?}",
        failed.errors
    );
    out.extend(render(&mut player, 5_000));
    assert!(out == unswapped(SONG, 10_000));
}
