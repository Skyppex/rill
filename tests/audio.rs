//! Audio files: loading, resampling, their fields, `read`, and the core
//! sampler. The files are written by the tests themselves.

use std::path::{Path, PathBuf};

use rill::lang;
use rill::offline::{self, Blocks};
use rill::{Config, Engine};

const RATE: u32 = 48_000;

/// A folder of its own for one test, emptied first.
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rill-audio-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 16-bit WAV of `channels` (one buffer each) at `rate`, with a `smpl`
/// chunk for `sampler` (root note, and a loop's first and last frame).
fn wav(path: &Path, rate: u32, channels: &[Vec<f32>], sampler: Option<(u32, Option<(u32, u32)>)>) {
    std::fs::write(path, wav_bytes(rate, channels, sampler)).unwrap();
}

fn wav_bytes(rate: u32, channels: &[Vec<f32>], sampler: Option<(u32, Option<(u32, u32)>)>) -> Vec<u8> {
    let chunk = |id: &[u8], body: &[u8]| {
        let mut c = id.to_vec();
        c.extend((body.len() as u32).to_le_bytes());
        c.extend(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    };
    let n = channels.len() as u16;
    let mut fmt = Vec::new();
    for v in [1u16, n] {
        fmt.extend(v.to_le_bytes());
    }
    fmt.extend(rate.to_le_bytes());
    fmt.extend((rate * 2 * u32::from(n)).to_le_bytes());
    fmt.extend((2 * n).to_le_bytes());
    fmt.extend(16u16.to_le_bytes());
    let mut data = Vec::new();
    for i in 0..channels[0].len() {
        for c in channels {
            data.extend(((c[i] * 32767.0).round() as i16).to_le_bytes());
        }
    }
    let mut body = b"WAVE".to_vec();
    body.extend(chunk(b"fmt ", &fmt));
    body.extend(chunk(b"data", &data));
    if let Some((root, looped)) = sampler {
        let mut smpl = Vec::new();
        let loops = u32::from(looped.is_some());
        for v in [0, 0, 0, root, 0, 0, 0, loops, 0] {
            smpl.extend(u32::to_le_bytes(v));
        }
        if let Some((start, end)) = looped {
            for v in [0, 0, start, end, 0, 0] {
                smpl.extend(u32::to_le_bytes(v));
            }
        }
        body.extend(chunk(b"smpl", &smpl));
    }
    let mut file = b"RIFF".to_vec();
    file.extend((body.len() as u32).to_le_bytes());
    file.extend(body);
    file
}

/// A sine of `freq` and `amp`, `frames` long at `rate`.
fn sine(freq: f32, amp: f32, rate: u32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| amp * (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin())
        .collect()
}

fn config(channels: usize) -> Config {
    Config {
        sample_rate: RATE,
        max_frames: 256,
        out_channels: channels,
    }
}

/// Build `src`, written as `song.rill` in `dir`, and render `frames`; one
/// buffer per channel.
fn render_in(dir: &Path, src: &str, channels: usize, frames: usize, stream: bool) -> Vec<Vec<f32>> {
    let path = dir.join("song.rill");
    std::fs::write(&path, src).unwrap();
    let options = lang::build::Options {
        seed: Some(0),
        stream,
        ..Default::default()
    };
    let (map, result) = lang::load_file(&path, &lang::Disk, &config(channels), "main", &options);
    let (graph, _) = result.unwrap_or_else(|d| {
        panic!("{}", d.iter().map(|d| map.render(d)).collect::<String>())
    });
    let mut engine = Engine::new(graph, config(channels)).unwrap();
    let out = offline::render(&mut engine, frames, &Blocks::Fixed(64));
    (0..channels)
        .map(|c| out.iter().skip(c).step_by(channels).copied().collect())
        .collect()
}

fn render(dir: &Path, src: &str, channels: usize, frames: usize) -> Vec<Vec<f32>> {
    render_in(dir, src, channels, frames, true)
}

/// The errors building `src` in `dir` gives.
fn errors(dir: &Path, src: &str) -> Vec<String> {
    let path = dir.join("song.rill");
    std::fs::write(&path, src).unwrap();
    let (_, result) = lang::load_file(
        &path,
        &lang::Disk,
        &config(2),
        "main",
        &lang::build::Options::default(),
    );
    match result {
        Ok(_) => Vec::new(),
        Err(d) => d.into_iter().filter(|d| d.is_error()).map(|d| d.message).collect(),
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Upward zero crossings per second.
fn crossings(x: &[f32]) -> f32 {
    let n = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    n as f32 * RATE as f32 / x.len() as f32
}

#[test]
fn a_file_plays_at_its_own_pitch_and_level_at_another_rate() {
    let dir = folder("pitch");
    // 44.1kHz, played at 48kHz: still 441Hz, still one second.
    wav(
        &dir.join("tone.wav"),
        44_100,
        &[sine(441.0, 0.5, 44_100, 44_100), sine(441.0, 0.25, 44_100, 44_100)],
        None,
    );
    let out = render(
        &dir,
        "const TONE = ./tone.wav
        rill main() [Sample; 2] {
            state t: Time = 0s
            let out = read(TONE, t)
            t = t + 1 / RATE
            return out
        }",
        2,
        58_000,
    );
    let middle = 2_400..45_600;
    assert!((crossings(&out[0][middle.clone()]) - 441.0).abs() < 1.5);
    assert!((rms(&out[0][middle.clone()]) - 0.5 / 2f32.sqrt()).abs() < 1e-3);
    assert!((rms(&out[1][middle]) - 0.25 / 2f32.sqrt()).abs() < 1e-3);
    // Silence once it is over.
    assert!(out[0][48_100..].iter().all(|v| *v == 0.0));
}

#[test]
fn a_file_at_the_host_rate_reads_back_exactly() {
    let dir = folder("exact");
    let x: Vec<f32> = (0..1000).map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5).collect();
    wav(&dir.join("x.wav"), RATE, std::slice::from_ref(&x), None);
    let out = render(
        &dir,
        "rill main() Sample {
            state frame: Float = 0
            let out = read(./x.wav, frame, 0)
            frame = frame + 1
            return out
        }",
        1,
        1000,
    );
    for (i, (got, want)) in out[0].iter().zip(&x).enumerate() {
        // Written as 16 bits, read back over 32768.
        let want = (want * 32767.0).round() / 32768.0;
        assert!((got - want).abs() < 1e-6, "frame {i}: {got} vs {want}");
    }
}

#[test]
fn fields_say_what_the_file_is() {
    let dir = folder("fields");
    wav(
        &dir.join("loop.wav"),
        44_100,
        &[vec![0.0; 4410], vec![0.0; 4410]],
        Some((57, Some((200, 799)))),
    );
    wav(&dir.join("plain.wav"), 22_050, &[vec![0.0; 2205]], None);
    let out = render(
        &dir,
        "const LOOP = ./loop.wav
        const PLAIN = ./plain.wav
        rill main() [Sample; 8] {
            let sized: [Sample; LOOP.channels] = [1, 2]
            return [
                (LOOP.root - C4) / 1st,
                LOOP.loop_start * 44100Hz,
                LOOP.loop_end * 44100Hz,
                LOOP.length / 1s,
                PLAIN.rate / 1Hz,
                PLAIN.frames as Float,
                PLAIN.loop_end / 1s,
                sized[1],
            ]
        }",
        8,
        1,
    );
    let got: Vec<f32> = out.iter().map(|c| c[0]).collect();
    let want = [-3.0, 200.0, 800.0, 0.1, 22_050.0, 2205.0, 0.1, 2.0];
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() < 1e-3, "{got:?}");
    }
}

#[test]
fn samples_and_play_from_the_core_library() {
    let dir = folder("sampler");
    wav(&dir.join("tone.wav"), RATE, &[sine(480.0, 0.5, RATE, 4800)], None);
    // Twice as fast, looping: an octave up, on and on.
    let out = render(
        &dir,
        "import @gen/sampler
        rill main() Sample { return ./tone.wav |> samples(speed: 2, loop: true) }",
        1,
        48_000,
    );
    // The first crossing is sample 0, which has nothing before it.
    let c = crossings(&out[0]);
    assert!((c - 960.0).abs() <= 1.0, "{c}");
    // Without a loop, once: a tenth of a second at speed 1.
    let out = render(
        &dir,
        "import @gen/sampler
        rill main() Sample { return ./tone.wav |> samples() }",
        1,
        9_600,
    );
    assert!(rms(&out[0][..4800]) > 0.3);
    assert!(out[0][4801..].iter().all(|v| *v == 0.0));
    // A hit for every note of a sequence, each in a voice of its own.
    let out = render(
        &dir,
        "import @gen/sampler
        const TONE = ./tone.wav
        seq hits(step: 1/4, tempo: 240bpm) { C4, _, C4, _ }
        rill voice() Sample {
            state gate: Float = 0
            on hits_note_on claim { gate = 1 }
            on hits_note_off release { gate = 0 }
            return play(TONE, gate)
        }
        rill main() Sample {
            on start { invoke hits }
            return sum([voice(); 2])
        }",
        1,
        48_000,
    );
    // Steps are 12000 samples apart; each hit lasts 4800.
    for start in [0, 24_000] {
        assert!(rms(&out[0][start..start + 4800]) > 0.3, "a hit at {start}");
        assert!(rms(&out[0][start + 4900..start + 12_000]) == 0.0);
    }
    assert!(rms(&out[0][12_000..24_000]) == 0.0, "a rest");
}

#[test]
fn far_into_a_long_file_the_pitch_holds() {
    let dir = folder("long");
    // 40 seconds of 1kHz: a time 39s in is not exact as one number, so the
    // sampler keeps a whole frame and a part.
    let seconds = 40;
    wav(
        &dir.join("long.wav"),
        RATE,
        &[sine(1000.0, 0.5, RATE, RATE as usize * seconds)],
        None,
    );
    let out = render(
        &dir,
        "import @gen/sampler
        rill main() Sample { return ./long.wav |> samples(speed: 1.5) }",
        1,
        RATE as usize * 26,
    );
    // At 1.5 times, 39s of the file is 26s in.
    let last = &out[0][RATE as usize * 25..];
    let c = crossings(last);
    assert!((c - 1500.0).abs() <= 1.0, "{c}");
}

#[test]
fn reading_runs_the_same_in_stages_as_sample_by_sample() {
    let dir = folder("stages");
    wav(&dir.join("tone.wav"), 44_100, &[sine(300.0, 0.5, 44_100, 30_000)], None);
    let src = "import @gen/sampler
        const TONE = ./tone.wav
        rill main(speed: Float = 1.5) Sample {
            return (TONE |> samples(speed, loop: true)) + read(TONE, 10ms)
        }";
    let a = render_in(&dir, src, 1, 20_000, true);
    let b = render_in(&dir, src, 1, 20_000, false);
    assert!(a == b);
}

#[test]
fn audio_files_have_their_own_errors() {
    let dir = folder("errors");
    wav(&dir.join("x.wav"), RATE, &[vec![0.0; 10]], None);
    let e = errors(&dir, "rill main() Sample { return read(./nothere.wav, 0s) }");
    assert!(e.iter().any(|m| m.starts_with("cannot read `./nothere.wav`")), "{e:?}");
    let e = errors(&dir, "rill main() Sample { return read(./notes.txt, 0s) }");
    assert_eq!(e, ["`./notes.txt` is not an audio file or an image"]);
    let e = errors(&dir, "rill main() Sample { state a: Audio = ./x.wav\nreturn 0 }");
    assert!(e.contains(&"an audio file cannot be `state`".to_owned()), "{e:?}");
    let e = errors(&dir, "rill main(a: Audio = ./x.wav) Sample { return 0 }");
    assert!(e.contains(&"`a` cannot be an audio file".to_owned()), "{e:?}");
    let e = errors(&dir, "rill main() Sample { return ((./x.wav).root - C4) / 1st }");
    assert!(e.iter().any(|m| m.ends_with("does not say which note it plays")), "{e:?}");
    let e = errors(&dir, "rill main() Sample { return (./x.wav).lenght / 1s }");
    assert_eq!(e, ["an audio file has no field `lenght`"]);
    // A path goes on through dots, so a field needs parentheses.
    let path = lang::compile("rill main() Sample { return ./x.wav.length / 1s }").unwrap_err();
    assert_eq!(
        path[0].help.as_deref(),
        Some("a path goes on through `.`; to read a field, put the path in parentheses: `(./x.wav).length`")
    );
    // Bytes that are not audio.
    std::fs::write(dir.join("bad.wav"), b"not a wav at all").unwrap();
    let e = errors(&dir, "rill main() Sample { return read(./bad.wav, 0s) }");
    assert!(e.iter().any(|m| m.starts_with("cannot read `./bad.wav`")), "{e:?}");
}

/// A program and its files, all in memory: no disk at all.
struct Memory {
    files: std::collections::HashMap<PathBuf, Vec<u8>>,
    /// The stamp every file gets, if any.
    stamp: Option<u64>,
    /// Samples made up for `./made.wav`, instead of reading a file.
    made: bool,
    /// How often `audio` was asked for, to see the cache.
    asked: std::sync::atomic::AtomicUsize,
}

impl Memory {
    fn new(files: &[(&str, Vec<u8>)]) -> Memory {
        Memory {
            files: files.iter().map(|(p, b)| (PathBuf::from(p), b.clone())).collect(),
            stamp: None,
            made: false,
            asked: Default::default(),
        }
    }
}

impl lang::Sources for Memory {
    fn read(&self, path: &Path) -> std::io::Result<String> {
        let bytes = self.read_bytes(path)?;
        String::from_utf8(bytes).map_err(|_| std::io::ErrorKind::InvalidData.into())
    }

    fn read_bytes(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::ErrorKind::NotFound.into())
    }

    fn stamp(&self, _: &Path) -> Option<u64> {
        self.stamp
    }

    fn audio(&self, path: &Path) -> Result<std::sync::Arc<rill::audio::Audio>, String> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.made && path.ends_with("made.wav") {
            // A ramp, made here: no bytes, no decoder.
            return Ok(std::sync::Arc::new(rill::audio::Audio {
                rate: RATE,
                samples: vec![(0..100).map(|i| i as f32 / 100.0).collect()],
                root: Some(60.0),
                looped: None,
            }));
        }
        let bytes = self.read_bytes(path).map_err(|e| format!("cannot read it: {e}"))?;
        rill::audio::decode_bytes(bytes, path).map(std::sync::Arc::new)
    }

    fn watch(&self, _: &Path) -> Option<PathBuf> {
        None
    }
}

fn render_from(sources: &Memory, frames: usize) -> Vec<f32> {
    let options = lang::build::Options {
        seed: Some(0),
        ..Default::default()
    };
    let path = Path::new("/mem/song.rill");
    let (map, result) = lang::load_file(path, sources, &config(1), "main", &options);
    let (graph, _) = result.unwrap_or_else(|d| {
        panic!("{}", d.iter().map(|d| map.render(d)).collect::<String>())
    });
    let mut engine = Engine::new(graph, config(1)).unwrap();
    offline::render(&mut engine, frames, &Blocks::Fixed(64))
}

#[test]
fn files_come_from_the_sources_a_program_is_built_with() {
    let x: Vec<f32> = (0..64).map(|i| (i as f32 / 64.0) - 0.5).collect();
    let song = "rill main() Sample {
        state frame: Float = 0
        let out = read(./hit.wav, frame, 0)
        frame = frame + 1
        return out
    }";
    let mut memory = Memory::new(&[
        ("/mem/song.rill", song.as_bytes().to_vec()),
        ("/mem/hit.wav", wav_bytes(RATE, std::slice::from_ref(&x), None)),
    ]);
    let out = render_from(&memory, 64);
    for (got, want) in out.iter().zip(&x) {
        assert!((got - (want * 32767.0).round() / 32768.0).abs() < 1e-6);
    }
    // Without a stamp, every build reads it again; with one, once.
    let asked = |m: &Memory| m.asked.load(std::sync::atomic::Ordering::Relaxed);
    let before = asked(&memory);
    render_from(&memory, 1);
    assert_eq!(asked(&memory), before + 1);
    memory.stamp = Some(7);
    render_from(&memory, 1);
    render_from(&memory, 1);
    assert_eq!(asked(&memory), before + 2);
}

#[test]
fn sources_can_make_audio_with_no_file_at_all() {
    let song = "rill main() Sample {
        state frame: Float = 0
        let out = read(./made.wav, frame, 0) + ((./made.wav).root - C4) / 1st
        frame = frame + 1
        return out
    }";
    let mut memory = Memory::new(&[("/mem/song.rill", song.as_bytes().to_vec())]);
    memory.made = true;
    let out = render_from(&memory, 100);
    for (i, v) in out.iter().enumerate() {
        assert!((v - i as f32 / 100.0).abs() < 1e-6, "{i}: {v}");
    }
}

#[test]
fn sources_say_what_hot_reload_watches() {
    let song = "rill main() Sample { return read(./hit.wav, 0s) }";
    let memory = Memory::new(&[
        ("/mem/song.rill", song.as_bytes().to_vec()),
        ("/mem/hit.wav", wav_bytes(RATE, &[vec![0.5; 10]], None)),
    ]);
    let (live, _player, _) = rill::live::Live::from_file_with(
        Path::new("/mem/song.rill"),
        config(1),
        "main",
        lang::build::Options::default(),
        Box::new(memory),
    )
    .unwrap_or_else(|f| panic!("{:?}", f.errors));
    assert!(live.files().is_empty());
}
