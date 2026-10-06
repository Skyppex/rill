//! End-to-end behaviour through the simulated callback loop.

use rill::offline::{self, Blocks};
use rill::wav::{self, WavFormat};
use rill::{Config, Engine, Graph, patches};

const RATE: u32 = 48_000;

fn engine(graph: Graph, max_frames: usize, channels: usize) -> Engine {
    Engine::new(
        graph,
        Config {
            sample_rate: RATE,
            max_frames,
            out_channels: channels,
        },
    )
    .unwrap()
}

fn render(graph: Graph, frames: usize, blocks: Blocks) -> Vec<f32> {
    offline::render(&mut engine(graph, 256, 1), frames, &blocks)
}

/// Upward zero crossings per second.
fn frequency(x: &[f32]) -> f32 {
    let crossings = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    crossings as f32 * RATE as f32 / x.len() as f32
}

#[test]
fn block_size_is_invisible() {
    let frames = 20_000;
    for patch in patches::NAMES {
        let make = || patches::by_name(patch, 440.0, 0.3).unwrap();
        let reference = render(make(), frames, Blocks::Fixed(32));
        for blocks in [
            Blocks::Fixed(1),
            Blocks::Fixed(1024),
            // Larger than max_frames: the engine has to split it.
            Blocks::Fixed(5000),
            Blocks::Pattern(vec![0, 7, 0, 300, 1, 256, 257]),
            Blocks::Random {
                min: 0,
                max: 2048,
                seed: 0x5eed,
            },
        ] {
            let got = render(make(), frames, blocks.clone());
            assert!(got == reference, "{patch} differs with {blocks:?}");
        }
    }
}

#[test]
fn max_frames_is_invisible() {
    let frames = 10_000;
    let a = offline::render(
        &mut engine(patches::vibrato(440.0, 0.3), 16, 1),
        frames,
        &Blocks::Fixed(512),
    );
    let b = offline::render(
        &mut engine(patches::vibrato(440.0, 0.3), 4096, 1),
        frames,
        &Blocks::Fixed(512),
    );
    assert!(a == b);
}

#[test]
fn sine_matches_the_closed_form() {
    let freq = 440.0;
    let gain = 0.3;
    let out = render(
        patches::sine(freq, gain),
        RATE as usize * 2,
        Blocks::Fixed(128),
    );
    for (n, &y) in out.iter().enumerate() {
        let t = n as f64 / f64::from(RATE);
        let expected = (gain as f64) * (std::f64::consts::TAU * freq as f64 * t).sin();
        assert!(
            (f64::from(y) - expected).abs() < 1e-5,
            "frame {n}: {y} vs {expected}"
        );
    }
}

#[test]
fn sine_has_the_right_pitch_and_level() {
    for freq in [55.0, 440.0, 1000.0, 7040.0] {
        let out = render(patches::sine(freq, 0.5), RATE as usize, Blocks::Fixed(64));
        assert!((frequency(&out) - freq).abs() <= 1.0, "{freq} Hz");
        let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 1e-3, "peak {peak}");
    }
}

#[test]
fn vibrato_sweeps_420_to_460_hz() {
    // Over one full 0.5 Hz LFO cycle (2 s) the pitch swings 440 +/- 20 Hz.
    let out = render(
        patches::vibrato(440.0, 0.3),
        RATE as usize * 2,
        Blocks::Fixed(256),
    );
    let periods: Vec<f32> = out
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0)
        .map(|(i, _)| i as f32)
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| RATE as f32 / (w[1] - w[0]))
        .collect();
    let lo = periods.iter().cloned().fold(f32::INFINITY, f32::min);
    let hi = periods.iter().cloned().fold(0.0, f32::max);
    assert!((415.0..425.0).contains(&lo), "lowest {lo}");
    assert!((455.0..465.0).contains(&hi), "highest {hi}");
    assert!((frequency(&out) - 440.0).abs() < 2.0);
}

#[test]
fn modulated_frequency_input_is_followed() {
    // An audio-rate frequency stream (constant 440 Hz, but routed through
    // a buffer) must give the same result as the constant fast path.
    let mut g = Graph::new();
    let f = g.add(400.0, 40.0);
    let s = g.sine(f);
    g.out(s);
    let via_buffer = render(g, 5000, Blocks::Fixed(100));
    let constant = render(patches::sine(440.0, 1.0), 5000, Blocks::Fixed(100));
    assert!(via_buffer == constant);
}

#[test]
fn planar_and_interleaved_agree() {
    let mut a = engine(patches::vibrato(440.0, 0.3), 128, 2);
    let mut b = engine(patches::vibrato(440.0, 0.3), 128, 2);
    let mut left = vec![0.0; 1000];
    let mut right = vec![0.0; 1000];
    a.render_planar(&mut [&mut left, &mut right]);
    let mut interleaved = vec![0.0; 2000];
    b.render_interleaved(&mut interleaved);
    for (i, frame) in interleaved.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(*frame, [left[i], right[i]]);
    }
}

#[test]
fn integer_output_formats() {
    let mut e = engine(patches::sine(1000.0, 1.0), 64, 1);
    let mut out = vec![0i16; 480];
    e.render_interleaved(&mut out);
    assert_eq!(out[0], 0);
    assert_eq!(*out.iter().max().unwrap(), i16::MAX);
    assert_eq!(*out.iter().min().unwrap(), -i16::MAX);
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rill-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn wav_round_trips_through_hound() {
    let mut e = engine(patches::vibrato(440.0, 0.3), 256, 2);
    let samples = offline::render(&mut e, 4800, &Blocks::Fixed(256));

    for (format, bits, tolerance) in [
        (WavFormat::Float32, 32, 0.0),
        (WavFormat::Pcm24, 24, 1.0 / 8_000_000.0),
        (WavFormat::Pcm16, 16, 1.0 / 32_000.0),
    ] {
        let path = scratch(&format!("{format:?}.wav"));
        wav::write_file(&path, RATE, 2, format, &samples).unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, RATE);
        assert_eq!(spec.bits_per_sample, bits);
        assert_eq!(reader.len() as usize, samples.len());

        let read: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().map(Result::unwrap).collect(),
            hound::SampleFormat::Int => {
                let scale = ((1i64 << (bits - 1)) - 1) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.unwrap() as f32 / scale)
                    .collect()
            }
        };
        for (a, b) in read.iter().zip(&samples) {
            assert!((a - b).abs() <= tolerance, "{format:?}: {a} vs {b}");
        }
        std::fs::remove_file(path).unwrap();
    }
}
