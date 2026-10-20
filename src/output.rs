//! Where `rill render` writes, and how: `--out song.wav:i16`,
//! `--out song.png:1000x500,range=80dB`.
//!
//! The file's extension picks what is written: a WAV, or a spectrogram in
//! any image format Rill reads. Options for it follow the last `:`, which
//! Windows does not allow in a file name. They count as options only when
//! the part before them names a file of a kind Rill writes and the whole
//! does not; otherwise it is all a path, so `notes:draft.png` and
//! `C:\out\song.png` are files.

use std::path::PathBuf;

use crate::lang::lexer::Unit;
use crate::lang::types::Type;
use crate::lang::value::Value;
use crate::wav::WavFormat;

/// What to write, and where.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Wav { path: PathBuf, format: WavFormat },
    Spectrogram { path: PathBuf, options: Spectrogram },
}

impl Output {
    pub fn path(&self) -> &PathBuf {
        match self {
            Output::Wav { path, .. } | Output::Spectrogram { path, .. } => path,
        }
    }
}

/// How a spectrogram is drawn. Its mapping is the one `@gen/spectral`
/// plays: every column is one step in time (the width over the length),
/// every row one pitch, evenly spaced from `high` at the top to `low` at
/// the bottom, and white is full level and black `range` below. The
/// image's size is its resolution: a wider one has finer time steps, a
/// taller one finer pitches.
#[derive(Clone, Debug, PartialEq)]
pub struct Spectrogram {
    /// Columns; `None` is one per 10ms of audio.
    pub width: Option<u32>,
    pub height: u32,
    /// How far below full level is black, in dB.
    pub range: f32,
    pub low: f32,
    pub high: f32,
    pub colors: Colors,
}

impl Default for Spectrogram {
    fn default() -> Self {
        Spectrogram {
            width: None,
            height: 512,
            range: 60.0,
            low: 20.0,
            high: 20_000.0,
            colors: Colors::Stereo,
        }
    }
}

/// How levels become colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colors {
    /// Red the left channel, blue the right, green what they share: mono is
    /// grey to white. Reads back as the same sound.
    Stereo,
    /// The channels mixed, grey to white.
    Grey,
    /// The channels mixed, on a colour scale: for looks; it does not read
    /// back as the same sound.
    Magma,
    Viridis,
}

impl Colors {
    const NAMES: &[(&str, Colors)] = &[
        ("stereo", Colors::Stereo),
        ("grey", Colors::Grey),
        ("gray", Colors::Grey),
        ("magma", Colors::Magma),
        ("viridis", Colors::Viridis),
    ];
}

/// The image extensions a spectrogram can be written as.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tga", "ico", "qoi", "pnm", "pbm", "pgm", "ppm",
    "pam", "tif", "tiff", "hdr", "exr",
];

/// Whether `path` names an image (`Some(true)`), a WAV (`Some(false)`), or
/// neither.
fn kind(path: &str) -> Option<bool> {
    let ext = crate::audio::extension(path)?;
    if ext == "wav" || ext == "wave" {
        Some(false)
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(true)
    } else {
        None
    }
}

/// `--out`, as written.
pub fn parse(arg: &str) -> Result<Output, String> {
    let (path, options) = match arg.rsplit_once(':') {
        Some((path, options)) if kind(path).is_some() && kind(arg).is_none() => (path, options),
        _ => (arg, ""),
    };
    let Some(image) = kind(path) else {
        return Err(format!(
            "`{path}` is not a file `render` writes: give it `.wav` for sound, or an image extension ({}) for a spectrogram",
            IMAGE_EXTENSIONS.join(", ")
        ));
    };
    let options: Vec<&str> = options.split(',').filter(|o| !o.is_empty()).collect();
    let path = PathBuf::from(path);
    if !image {
        let mut format = WavFormat::Float32;
        for o in options {
            format = match o {
                "f32" => WavFormat::Float32,
                "i16" => WavFormat::Pcm16,
                "i24" => WavFormat::Pcm24,
                other => {
                    return Err(format!(
                        "a WAV file takes `f32`, `i16` or `i24` after `:`, not `{other}`"
                    ));
                }
            };
        }
        return Ok(Output::Wav { path, format });
    }
    let mut s = Spectrogram::default();
    for o in options {
        let Some((name, value)) = o.split_once('=') else {
            // `1000x500`, `1000` or `x500`.
            let (w, h) = o.split_once('x').unwrap_or((o, ""));
            let number = |t: &str| {
                t.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| {
                    format!("`{o}` is not a size: write it as `1000x500`, `1000` or `x500`")
                })
            };
            if w.is_empty() && h.is_empty() {
                number("")?;
            }
            if !w.is_empty() {
                s.width = Some(number(w)?);
            }
            if !h.is_empty() {
                s.height = number(h)?;
            }
            continue;
        };
        let freq = |v: &str| {
            Value::parse(v)
                .and_then(|v| v.resolve(Some(&Type::Freq)))
                .and_then(|f| {
                    if f > 0.0 {
                        Ok(f)
                    } else {
                        Err("a frequency above 0Hz".into())
                    }
                })
                .map_err(|e| format!("`{name}`: {e}"))
        };
        match name {
            "range" => {
                let db = match Value::parse(value)? {
                    Value::Number { value, unit: None } => value as f32,
                    Value::Number {
                        value,
                        unit: Some(Unit::Db),
                    } => value as f32,
                    _ => {
                        return Err(format!(
                            "`range` is in dB, as in `range=80dB`, not `{value}`"
                        ));
                    }
                };
                if db <= 0.0 || !db.is_finite() {
                    return Err("`range` is how far below full level black is: above 0dB".into());
                }
                s.range = db;
            }
            "low" => s.low = freq(value)?,
            "high" => s.high = freq(value)?,
            "colors" | "colours" => {
                s.colors = Colors::NAMES
                    .iter()
                    .find(|(n, _)| *n == value)
                    .map(|(_, c)| *c)
                    .ok_or_else(|| {
                        format!("`colors` is `stereo`, `grey`, `magma` or `viridis`, not `{value}`")
                    })?;
            }
            other => {
                return Err(format!(
                    "a spectrogram takes a size (`1000x500`), `range`, `low`, `high` and `colors`, not `{other}`"
                ));
            }
        }
    }
    if s.low >= s.high {
        return Err(format!(
            "`low` ({}Hz) must be below `high` ({}Hz)",
            s.low, s.high
        ));
    }
    Ok(Output::Spectrogram { path, options: s })
}

/// Red, green and blue, 0 to 1, per pixel, row by row from the top: the
/// spectrogram of `samples` (interleaved, `channels` of them) at `rate`.
/// Returns its width, height and pixels.
#[cfg(feature = "images")]
pub fn spectrogram(
    samples: &[f32],
    channels: usize,
    rate: u32,
    s: &Spectrogram,
) -> (u32, u32, Vec<[f32; 3]>) {
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    let seconds = frames as f64 / f64::from(rate);
    let width = s
        .width
        .unwrap_or_else(|| ((seconds * 100.0).round() as u32).max(1));
    let height = s.height;
    let (w, h) = (width as usize, height as usize);
    // Samples per column: the image's own time step.
    let column = frames as f64 / f64::from(width);
    // The levels of one signal, row by row from the top, from 0 (black) to
    // 1 (white).
    //
    // Every row listens at exactly its own pitch, over a Hann window
    // centred on each column and exactly one column long, so a column holds
    // what happened in its own step and nothing from its neighbours: time is
    // as fine as the image is wide. Only a row that cannot fit two of its
    // own cycles in a column (a low one on a very wide image) listens for
    // those two cycles instead. The price is pitch: a short column cannot
    // tell close pitches apart, so low rows light up together. A full-level
    // sine at a row's pitch reads as 1, which is white.
    //
    // A Hann window is a sum of three plain ones at three frequencies (the
    // row's, and one window's cycle either side), so each row needs three
    // running sums of the turned signal, and then every column is a few
    // subtractions.
    let levels = |signal: &dyn Fn(usize) -> f32| -> Vec<f32> {
        type C = (f64, f64);
        let mut out = vec![0.0f32; w * h];
        let mut sums: [Vec<C>; 3] = std::array::from_fn(|_| vec![(0.0, 0.0); frames + 1]);
        let rate = f64::from(rate);
        for y in 0..h {
            let along = if h == 1 {
                0.5
            } else {
                y as f64 / (h - 1) as f64
            };
            let freq = f64::from(s.high) * f64::from(s.low / s.high).powf(along);
            if freq >= rate / 2.0 {
                continue;
            }
            let n = column
                .max(2.0 * rate / freq)
                .min(frames as f64)
                .round()
                .max(1.0);
            let omega = std::f64::consts::TAU * freq / rate;
            let shift = std::f64::consts::TAU / n;
            for (k, offset) in [0.0, -shift, shift].into_iter().enumerate() {
                let step = omega + offset;
                let sum = &mut sums[k];
                for i in 0..frames {
                    let (sin, cos) = (step * i as f64).sin_cos();
                    let x = f64::from(signal(i));
                    sum[i + 1] = (sum[i].0 + x * cos, sum[i].1 - x * sin);
                }
            }
            for col in 0..w {
                let start = ((col as f64 + 0.5) * column - n / 2.0).round();
                let a = start.clamp(0.0, frames as f64) as usize;
                let b = (start + n).clamp(0.0, frames as f64) as usize;
                if b <= a {
                    continue;
                }
                let part = |k: usize| -> C {
                    let (s0, s1) = (sums[k][a], sums[k][b]);
                    (s1.0 - s0.0, s1.1 - s0.1)
                };
                // The cos of the window starts at `start`.
                let (sin, cos) = (shift * start).sin_cos();
                let turn = |c: C, sin: f64| (c.0 * cos - c.1 * sin, c.0 * sin + c.1 * cos);
                let (mid, below, above) = (part(0), turn(part(1), -sin), turn(part(2), sin));
                let x = (
                    0.5 * mid.0 - 0.25 * (below.0 + above.0),
                    0.5 * mid.1 - 0.25 * (below.1 + above.1),
                );
                let amp = 4.0 * x.0.hypot(x.1) / n;
                let db = 20.0 * amp.max(1e-12).log10();
                out[y * w + col] = (1.0 + db as f32 / s.range).clamp(0.0, 1.0);
            }
        }
        out
    };
    let channel = |c: usize| move |i: usize| samples[i * channels + c];
    let mixed = |i: usize| {
        (0..channels)
            .map(|c| samples[i * channels + c])
            .sum::<f32>()
            / channels as f32
    };
    let stereo = s.colors == Colors::Stereo && channels >= 2;
    let (left, right) = if stereo {
        (levels(&channel(0)), Some(levels(&channel(1))))
    } else {
        (levels(&mixed), None)
    };
    let pixels = left
        .iter()
        .enumerate()
        .map(|(i, &a)| match (&right, s.colors) {
            (Some(r), _) => {
                let b = r[i];
                [a, a.min(b), b]
            }
            (None, Colors::Magma) => scale_colour(&MAGMA, a),
            (None, Colors::Viridis) => scale_colour(&VIRIDIS, a),
            (None, _) => [a; 3],
        })
        .collect();
    (width, height, pixels)
}

#[cfg(not(feature = "images"))]
pub fn spectrogram(_: &[f32], _: usize, _: u32, _: &Spectrogram) -> (u32, u32, Vec<[f32; 3]>) {
    (0, 0, Vec::new())
}

/// Write `pixels` (`width` by `height`) to `path`, in the format its
/// extension says.
#[cfg(feature = "images")]
pub fn write_image(
    path: &std::path::Path,
    width: u32,
    height: u32,
    pixels: &[[f32; 3]],
) -> Result<(), String> {
    let float = image::Rgb32FImage::from_fn(width, height, |x, y| {
        image::Rgb(pixels[(y * width + x) as usize])
    });
    let ext = crate::audio::extension(&path.to_string_lossy()).unwrap_or_default();
    let image = image::DynamicImage::ImageRgb32F(float);
    let image = match ext.as_str() {
        "hdr" | "exr" => image,
        _ => image::DynamicImage::ImageRgb8(image.to_rgb8()),
    };
    image
        .save(path)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(not(feature = "images"))]
pub fn write_image(_: &std::path::Path, _: u32, _: u32, _: &[[f32; 3]]) -> Result<(), String> {
    Err("this build of Rill draws no images: it was built without the `images` feature".into())
}

/// Colour scales, as points from dark to bright, blended between.
#[cfg_attr(not(feature = "images"), allow(dead_code))]
const MAGMA: [[f32; 3]; 6] = [
    [0.001, 0.000, 0.014],
    [0.231, 0.060, 0.439],
    [0.550, 0.161, 0.506],
    [0.868, 0.288, 0.409],
    [0.994, 0.624, 0.427],
    [0.987, 0.991, 0.750],
];
#[cfg_attr(not(feature = "images"), allow(dead_code))]
const VIRIDIS: [[f32; 3]; 6] = [
    [0.267, 0.005, 0.329],
    [0.254, 0.265, 0.530],
    [0.164, 0.471, 0.558],
    [0.135, 0.659, 0.518],
    [0.478, 0.821, 0.318],
    [0.993, 0.906, 0.144],
];

#[cfg_attr(not(feature = "images"), allow(dead_code))]
fn scale_colour(points: &[[f32; 3]], level: f32) -> [f32; 3] {
    let at = level.clamp(0.0, 1.0) * (points.len() - 1) as f32;
    let i = (at.floor() as usize).min(points.len() - 2);
    let t = at - i as f32;
    let (a, b) = (points[i], points[i + 1]);
    [0, 1, 2].map(|c| a[c] + (b[c] - a[c]) * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(arg: &str) -> Spectrogram {
        match parse(arg).unwrap() {
            Output::Spectrogram { options, .. } => options,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn paths_and_options() {
        assert_eq!(
            parse("song.wav").unwrap(),
            Output::Wav {
                path: "song.wav".into(),
                format: WavFormat::Float32
            }
        );
        assert_eq!(
            parse("out/song.wav:i16").unwrap(),
            Output::Wav {
                path: "out/song.wav".into(),
                format: WavFormat::Pcm16
            }
        );
        let s = spec("song.png:1000x500,range=80dB,low=40Hz,high=12kHz,colors=magma");
        assert_eq!((s.width, s.height), (Some(1000), 500));
        assert_eq!((s.range, s.low, s.high), (80.0, 40.0, 12_000.0));
        assert_eq!(s.colors, Colors::Magma);
        assert_eq!(spec("a.jpg:x300").height, 300);
        assert_eq!(spec("a.jpg:300").width, Some(300));
        // A `:` in a file name, and a Windows drive.
        assert_eq!(
            parse("notes:draft.png").unwrap().path(),
            &PathBuf::from("notes:draft.png")
        );
        assert_eq!(
            parse("C:\\out\\song.png").unwrap().path(),
            &PathBuf::from("C:\\out\\song.png")
        );
    }

    #[test]
    fn mistakes() {
        let e = |arg: &str| parse(arg).unwrap_err();
        assert!(e("song.mp3").starts_with("`song.mp3` is not a file `render` writes"));
        assert!(e("song.wav:1000x500").starts_with("a WAV file takes"));
        assert!(e("song.png:depth=3").starts_with("a spectrogram takes"));
        assert!(e("song.png:low=20kHz").starts_with("`low` (20000Hz) must be below"));
        assert!(e("song.png:axb").starts_with("`axb` is not a size"));
        assert!(e("song.png:colors=pink").starts_with("`colors` is"));
        assert!(e("song.png:low=3s").starts_with("`low`:"));
        assert!(e("song.png:range=-3dB").starts_with("`range` is how far"));
    }

    #[cfg(feature = "images")]
    #[test]
    fn a_sine_lands_on_its_row_and_its_columns() {
        let rate = 48_000;
        // 61 rows from 20kHz down to 20Hz: a row every twentieth of a
        // decade, so row 20 is 2kHz exactly.
        let s = Spectrogram {
            width: Some(10),
            height: 61,
            ..Default::default()
        };
        // Silence for half a second, then a full-level 2kHz sine on the left.
        let samples: Vec<f32> = (0..rate)
            .flat_map(|i| {
                let on = i >= rate / 2;
                let x = (std::f32::consts::TAU * 2000.0 * i as f32 / rate as f32).sin();
                [if on { x } else { 0.0 }, 0.0]
            })
            .collect();
        let (w, _, px) = spectrogram(&samples, 2, rate, &s);
        let at = |row: usize, col: usize| px[row * w as usize + col];
        // Its row is white in the columns where it plays, and red: left only.
        for col in 5..10 {
            let p = at(20, col);
            assert!(
                p[0] > 0.99 && p[1] == 0.0 && p[2] == 0.0,
                "column {col}: {p:?}"
            );
        }
        // Not before it starts, and not on the rows next to it.
        for col in 0..5 {
            assert_eq!(at(20, col)[0], 0.0, "column {col}");
        }
        for col in 5..10 {
            assert!(at(19, col)[0] < 0.2 && at(21, col)[0] < 0.2, "column {col}");
        }
    }

    #[cfg(feature = "images")]
    #[test]
    fn a_short_note_stays_in_its_own_column() {
        let rate = 48_000;
        // 20 columns over a second: 50ms each. Notes of one column each, at
        // 2kHz (row 20 of 61) and at 200Hz (row 40), in columns 7 and 12.
        let s = Spectrogram {
            width: Some(20),
            height: 61,
            ..Default::default()
        };
        let column = rate / 20;
        let samples: Vec<f32> = (0..rate)
            .flat_map(|i| {
                let t = i as f32 / rate as f32;
                let mut x = 0.0;
                if (7 * column..8 * column).contains(&i) {
                    x += (std::f32::consts::TAU * 2000.0 * t).sin() * 0.5;
                }
                if (12 * column..13 * column).contains(&i) {
                    x += (std::f32::consts::TAU * 200.0 * t).sin() * 0.5;
                }
                [x, x]
            })
            .collect();
        let (w, _, px) = spectrogram(&samples, 2, rate, &s);
        let at = |row: usize, col: usize| px[row * w as usize + col][0];
        for (row, col) in [(20, 7), (40, 12)] {
            assert!(
                at(row, col) > 0.85,
                "row {row} column {col}: {}",
                at(row, col)
            );
            for other in (0..20).filter(|c| *c != col) {
                assert_eq!(at(row, other), 0.0, "row {row} spilled into column {other}");
            }
        }
    }
}
