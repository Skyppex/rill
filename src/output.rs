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
/// plays: rows evenly spaced in pitch from `high` at the top to `low` at
/// the bottom, white at full level and black `range` below.
#[derive(Clone, Debug, PartialEq)]
pub struct Spectrogram {
    /// Columns; `None` is one per 10ms of audio.
    pub width: Option<u32>,
    pub height: u32,
    /// How far below full level is black, in dB.
    pub range: f32,
    pub low: f32,
    pub high: f32,
    /// Samples per FFT.
    pub fft: usize,
    pub colors: Colors,
}

impl Default for Spectrogram {
    fn default() -> Self {
        Spectrogram {
            width: None,
            height: 512,
            range: 60.0,
            low: 50.0,
            high: 15_000.0,
            fft: 4096,
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
                    return Err(
                        "`range` is how far below full level black is: above 0dB".into()
                    );
                }
                s.range = db;
            }
            "low" => s.low = freq(value)?,
            "high" => s.high = freq(value)?,
            "fft" => {
                s.fft = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| n.is_power_of_two() && (64..=65_536).contains(n))
                    .ok_or_else(|| {
                        format!("`fft` is a power of two from 64 to 65536, not `{value}`")
                    })?;
            }
            "colors" | "colours" => {
                s.colors = Colors::NAMES
                    .iter()
                    .find(|(n, _)| *n == value)
                    .map(|(_, c)| *c)
                    .ok_or_else(|| {
                        format!(
                            "`colors` is `stereo`, `grey`, `magma` or `viridis`, not `{value}`"
                        )
                    })?;
            }
            other => {
                return Err(format!(
                    "a spectrogram takes a size (`1000x500`), `range`, `low`, `high`, `fft` and `colors`, not `{other}`"
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
    use rustfft::FftPlanner;
    use rustfft::num_complex::Complex;

    let channels = channels.max(1);
    let frames = samples.len() / channels;
    let seconds = frames as f64 / f64::from(rate);
    let width = s
        .width
        .unwrap_or_else(|| ((seconds * 100.0).round() as u32).max(1));
    let height = s.height;
    let n = s.fft;
    let window: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos())
        .collect();
    // A full-level sine reads as 1: twice its bin over the window's sum.
    let scale = 2.0 / window.iter().sum::<f32>();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
    // Where each image row is among the bins.
    let rows: Vec<f32> = (0..height)
        .map(|y| {
            let along = if height == 1 {
                0.5
            } else {
                y as f32 / (height - 1) as f32
            };
            s.high * (s.low / s.high).powf(along) * n as f32 / rate as f32
        })
        .collect();
    // The levels of one signal, per column, per row, from 0 (black) to 1
    // (white).
    let levels = |signal: &dyn Fn(usize) -> f32| -> Vec<Vec<f32>> {
        let mut buf = vec![Complex::new(0.0f32, 0.0); n];
        (0..width)
            .map(|col| {
                let centre = ((col as f64 + 0.5) / f64::from(width) * frames as f64) as isize;
                for (i, b) in buf.iter_mut().enumerate() {
                    let at = centre - (n / 2) as isize + i as isize;
                    let x = if at >= 0 && (at as usize) < frames {
                        signal(at as usize)
                    } else {
                        0.0
                    };
                    *b = Complex::new(x * window[i], 0.0);
                }
                fft.process(&mut buf);
                rows.iter()
                    .map(|&bin| {
                        let i = bin.floor() as usize;
                        let t = bin - bin.floor();
                        let m = |k: usize| buf.get(k).map_or(0.0, |c| c.norm()) * scale;
                        let amp = m(i) + (m(i + 1) - m(i)) * t;
                        let db = 20.0 * amp.max(1e-12).log10();
                        (1.0 + db / s.range).clamp(0.0, 1.0)
                    })
                    .collect()
            })
            .collect()
    };
    let channel = |c: usize| move |i: usize| samples[i * channels + c];
    let mixed =
        |i: usize| (0..channels).map(|c| samples[i * channels + c]).sum::<f32>() / channels as f32;
    let stereo = s.colors == Colors::Stereo && channels >= 2;
    let (left, right) = if stereo {
        (levels(&channel(0)), Some(levels(&channel(1))))
    } else {
        (levels(&mixed), None)
    };
    let mut pixels = vec![[0.0f32; 3]; (width * height) as usize];
    for (col, l) in left.iter().enumerate() {
        for (y, &a) in l.iter().enumerate() {
            pixels[y * width as usize + col] = match (&right, s.colors) {
                (Some(r), _) => {
                    let b = r[col][y];
                    [a, a.min(b), b]
                }
                (None, Colors::Magma) => scale_colour(&MAGMA, a),
                (None, Colors::Viridis) => scale_colour(&VIRIDIS, a),
                (None, _) => [a; 3],
            };
        }
    }
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
        let s = spec("song.png:1000x500,range=80dB,low=40Hz,high=12kHz,fft=8192,colors=magma");
        assert_eq!((s.width, s.height), (Some(1000), 500));
        assert_eq!((s.range, s.low, s.high, s.fft), (80.0, 40.0, 12_000.0, 8192));
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
        assert!(e("song.png:fft=1000").starts_with("`fft` is a power of two"));
        assert!(e("song.png:low=20kHz").starts_with("`low` (20000Hz) must be below"));
        assert!(e("song.png:axb").starts_with("`axb` is not a size"));
        assert!(e("song.png:colors=pink").starts_with("`colors` is"));
        assert!(e("song.png:low=3s").starts_with("`low`:"));
        assert!(e("song.png:range=-3dB").starts_with("`range` is how far"));
    }

    #[cfg(feature = "images")]
    #[test]
    fn a_sine_is_a_bright_row_at_its_pitch() {
        let rate = 48_000;
        let s = Spectrogram {
            width: Some(8),
            height: 64,
            ..Default::default()
        };
        let row_of = |freq: f32| {
            let along = (freq / s.high).ln() / (s.low / s.high).ln();
            (along * (s.height - 1) as f32).round() as usize
        };
        // Left only, at 1kHz, full level.
        let samples: Vec<f32> = (0..rate)
            .flat_map(|i| {
                let x = (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin();
                [x, 0.0]
            })
            .collect();
        let (w, _, px) = spectrogram(&samples, 2, rate, &s);
        let (w, col) = (w as usize, 4);
        let brightest = (0..64)
            .max_by(|&a, &b| px[a * w + col][0].total_cmp(&px[b * w + col][0]))
            .unwrap();
        assert!(brightest.abs_diff(row_of(1000.0)) <= 1);
        let p = px[brightest * w + col];
        // Full level is white; left only is red.
        assert!(p[0] > 0.95 && p[1] == 0.0 && p[2] == 0.0, "{p:?}");
    }
}
