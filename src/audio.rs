//! Audio files named in a program, as in `const KICK = ./kick.wav`.
//!
//! A file is decoded when the program is built, never while it plays, and
//! kept by path: building again (a hot reload, an editor checking as you
//! type) does not decode it again unless the file changed. For playing, it is
//! resampled to the host's rate once per rate, so a second of the file is a
//! second while playing.
//!
//! Decoding is pure Rust ([symphonia]): WAV, AIFF, CAF, FLAC, ALAC, MP1/2/3,
//! Vorbis and AAC, in RIFF, AIFF, CAF, OGG, MP4 and Matroska containers.
//! What a sampler keeps beside the sound, the note a file plays unchanged
//! and its loop, is read from WAV `smpl` and AIFF `INST`/`MARK` chunks.
//!
//! [symphonia]: https://crates.io/crates/symphonia

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// The extensions of the formats that can be read, in lower case.
pub const EXTENSIONS: &[&str] = &[
    "wav", "wave", "aif", "aiff", "aifc", "caf", "flac", "mp1", "mp2", "mp3", "ogg", "oga", "m4a",
    "mp4", "mka", "mkv", "webm",
];

/// Whether `path` names an audio file, by its extension.
pub fn is_audio(path: &str) -> bool {
    extension(path).is_some_and(|e| EXTENSIONS.contains(&e.as_str()))
}

/// The extension of the last part of `path`, in lower case.
pub fn extension(path: &str) -> Option<String> {
    let last = path.rsplit('/').next()?;
    let (stem, ext) = last.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// A decoded audio file, at its own rate.
#[derive(Debug)]
pub struct Audio {
    /// Its own sample rate.
    pub rate: u32,
    /// One buffer per channel, all of the same length.
    pub samples: Vec<Vec<f32>>,
    /// The note it plays unchanged, as a pitch (60 is C4), if it says.
    pub root: Option<f32>,
    /// Its loop, start to end in frames (the end not included), if it has
    /// one.
    pub looped: Option<(usize, usize)>,
}

impl Audio {
    pub fn channels(&self) -> usize {
        self.samples.len()
    }

    /// How many frames it has, at its own rate.
    pub fn frames(&self) -> usize {
        self.samples.first().map_or(0, Vec::len)
    }

    /// How long it lasts, in seconds.
    pub fn length(&self) -> f64 {
        self.frames() as f64 / f64::from(self.rate)
    }

    /// Its loop in seconds: the one it has, or the whole file.
    pub fn loop_seconds(&self) -> (f64, f64) {
        let rate = f64::from(self.rate);
        match self.looped {
            Some((start, end)) => (start as f64 / rate, end as f64 / rate),
            None => (0.0, self.length()),
        }
    }
}

/// When a file was last changed, and how long it is: a different stamp
/// means it is read again.
type Stamp = (Option<SystemTime>, u64);

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path).map_or((None, 0), |m| (m.modified().ok(), m.len()))
}

type Decoded = HashMap<PathBuf, (Stamp, Arc<Audio>)>;
type Resampled = HashMap<(PathBuf, u32), (Stamp, Arc<[Box<[f32]>]>)>;

fn decoded() -> &'static Mutex<Decoded> {
    static CACHE: OnceLock<Mutex<Decoded>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn resampled() -> &'static Mutex<Resampled> {
    static CACHE: OnceLock<Mutex<Resampled>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The audio file at `path`, decoded, or why it cannot be.
pub fn load(path: &Path) -> Result<Arc<Audio>, String> {
    let now = stamp(path);
    if let Some((s, audio)) = decoded().lock().ok().and_then(|c| c.get(path).cloned())
        && s == now
    {
        return Ok(audio);
    }
    let audio = Arc::new(decode(path)?);
    if let Ok(mut cache) = decoded().lock() {
        cache.insert(path.to_owned(), (now, Arc::clone(&audio)));
    }
    Ok(audio)
}

/// The audio file at `path` at `rate`, one buffer per channel, or why it
/// cannot be read.
pub fn at_rate(path: &Path, rate: u32) -> Result<Arc<[Box<[f32]>]>, String> {
    let now = stamp(path);
    let key = (path.to_owned(), rate);
    if let Some((s, samples)) = resampled().lock().ok().and_then(|c| c.get(&key).cloned())
        && s == now
    {
        return Ok(samples);
    }
    let audio = load(path)?;
    let samples: Arc<[Box<[f32]>]> = resample(&audio.samples, audio.rate, rate)
        .into_iter()
        .map(Vec::into_boxed_slice)
        .collect();
    if let Ok(mut cache) = resampled().lock() {
        cache.insert(key, (now, Arc::clone(&samples)));
    }
    Ok(samples)
}

#[cfg(feature = "audio-files")]
fn decode(path: &Path) -> Result<Audio, String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|e| format!("cannot read it: {e}"))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| match e {
            Error::Unsupported(_) => "its format is not one Rill can read".to_owned(),
            e => format!("it cannot be read as audio: {e}"),
        })?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("it has no audio in it")?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.ok_or("it does not say its sample rate")?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| match e {
            Error::Unsupported(_) => "it is encoded in a way Rill cannot decode (Opus needs libopus, which Rill does not use)".to_owned(),
            e => format!("it cannot be decoded: {e}"),
        })?;
    let mut samples: Vec<Vec<f32>> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(e) => return Err(format!("it cannot be read to the end: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A damaged packet is skipped, as players do.
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("it cannot be decoded: {e}")),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count();
        if samples.is_empty() {
            samples = vec![Vec::new(); channels];
        }
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_planar_ref(decoded);
        let n = buf.samples().len() / channels.max(1);
        for (c, out) in samples.iter_mut().enumerate().take(channels) {
            out.extend_from_slice(&buf.samples()[c * n..(c + 1) * n]);
        }
    }
    if samples.is_empty() {
        return Err("it has no audio in it".into());
    }
    let (root, looped) = std::fs::read(path).map_or((None, None), |bytes| sampler_chunks(&bytes));
    let frames = samples[0].len();
    Ok(Audio {
        rate,
        samples,
        root,
        looped: looped.filter(|&(s, e)| s < e && e <= frames),
    })
}

#[cfg(not(feature = "audio-files"))]
fn decode(_: &Path) -> Result<Audio, String> {
    Err("this build of Rill reads no audio files: it was built without the `audio-files` feature".into())
}

/// `input` (one buffer per channel) from rate `from` to rate `to`, with a
/// windowed sinc, keeping its length in time.
#[cfg(feature = "audio-files")]
fn resample(input: &[Vec<f32>], from: u32, to: u32) -> Vec<Vec<f32>> {
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
        WindowFunction,
    };
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let channels = input.len();
    let frames = input[0].len();
    let ratio = f64::from(to) / f64::from(from);
    let want = (frames as f64 * ratio).round() as usize;
    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Cubic,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    const CHUNK: usize = 1024;
    let Ok(mut r) = SincFixedIn::<f32>::new(ratio, 1.0, params, CHUNK, channels) else {
        return input.to_vec();
    };
    // `process` already gives output lined up with the input in time (an
    // impulse lands where it should), so nothing is dropped from the
    // front; the end is flushed out of the filter.
    let delay = r.output_delay();
    let mut out: Vec<Vec<f32>> = vec![Vec::with_capacity(want + delay + 2 * CHUNK); channels];
    let mut append = |chunk: Vec<Vec<f32>>| {
        for (o, c) in out.iter_mut().zip(chunk) {
            o.extend(c);
        }
    };
    let mut pos = 0;
    while pos + CHUNK <= frames {
        let part: Vec<&[f32]> = input.iter().map(|c| &c[pos..pos + CHUNK]).collect();
        if let Ok(chunk) = r.process(&part, None) {
            append(chunk);
        }
        pos += CHUNK;
    }
    if pos < frames {
        let part: Vec<&[f32]> = input.iter().map(|c| &c[pos..]).collect();
        if let Ok(chunk) = r.process_partial(Some(&part), None) {
            append(chunk);
        }
    }
    // What is still inside the filter.
    for _ in 0..(delay / CHUNK + 3) {
        match r.process_partial::<&[f32]>(None, None) {
            Ok(chunk) if !chunk[0].is_empty() => append(chunk),
            _ => break,
        }
    }
    for o in &mut out {
        o.resize(want, 0.0);
    }
    out
}

#[cfg(not(feature = "audio-files"))]
fn resample(input: &[Vec<f32>], _: u32, _: u32) -> Vec<Vec<f32>> {
    input.to_vec()
}

/// The root note and loop a sampler keeps in a WAV `smpl` chunk, or in
/// AIFF `INST` and `MARK` chunks.
#[cfg_attr(not(feature = "audio-files"), allow(dead_code))]
fn sampler_chunks(bytes: &[u8]) -> (Option<f32>, Option<(usize, usize)>) {
    let tag = |at: usize| bytes.get(at..at + 4);
    match (tag(0), tag(8)) {
        (Some(b"RIFF"), Some(b"WAVE")) => wav_smpl(bytes),
        (Some(b"FORM"), Some(b"AIFF" | b"AIFC")) => aiff_inst(bytes),
        _ => (None, None),
    }
}

#[cfg_attr(not(feature = "audio-files"), allow(dead_code))]
fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

#[cfg_attr(not(feature = "audio-files"), allow(dead_code))]
fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

#[cfg_attr(not(feature = "audio-files"), allow(dead_code))]
fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The chunks after a 12-byte RIFF or FORM header: id and body.
#[cfg_attr(not(feature = "audio-files"), allow(dead_code))]
fn chunks(bytes: &[u8], big_endian: bool) -> Vec<(&[u8], &[u8])> {
    let mut out = Vec::new();
    let mut at = 12;
    while let Some(id) = bytes.get(at..at + 4) {
        let size = if big_endian {
            be32(bytes, at + 4)
        } else {
            le32(bytes, at + 4)
        };
        let Some(size) = size else { break };
        let start = at + 8;
        let end = start.saturating_add(size as usize).min(bytes.len());
        out.push((id, &bytes[start..end]));
        // Chunks are padded to an even length.
        at = end + (size as usize & 1);
    }
    out
}

#[cfg_attr(not(feature = "audio-files"), allow(dead_code))]
fn wav_smpl(bytes: &[u8]) -> (Option<f32>, Option<(usize, usize)>) {
    let Some((_, smpl)) = chunks(bytes, false).into_iter().find(|(id, _)| *id == b"smpl") else {
        return (None, None);
    };
    let root = le32(smpl, 12).filter(|&n| n <= 127).map(|n| {
        // The pitch fraction is a part of a semitone, out of 2^32.
        let fraction = le32(smpl, 16).map_or(0.0, |f| f64::from(f) / 4_294_967_296.0);
        (f64::from(n) + fraction) as f32
    });
    let looped = (le32(smpl, 28).unwrap_or(0) > 0)
        .then(|| {
            // The first loop: its start, and its end, which is played.
            let (start, end) = (le32(smpl, 36 + 8)?, le32(smpl, 36 + 12)?);
            Some((start as usize, end as usize + 1))
        })
        .flatten();
    (root, looped)
}

#[cfg_attr(not(feature = "audio-files"), allow(dead_code))]
fn aiff_inst(bytes: &[u8]) -> (Option<f32>, Option<(usize, usize)>) {
    let all = chunks(bytes, true);
    let Some((_, inst)) = all.iter().find(|(id, _)| *id == b"INST") else {
        return (None, None);
    };
    let root = inst.first().filter(|&&n| n <= 127).map(|&n| {
        let detune = inst.get(1).map_or(0, |&d| d as i8);
        f32::from(n) + f32::from(detune) / 100.0
    });
    // The sustain loop: a play mode, then the ids of two markers.
    let looped = (|| {
        if be16(inst, 8)? == 0 {
            return None;
        }
        let (begin, end) = (be16(inst, 10)?, be16(inst, 12)?);
        let (_, mark) = all.iter().find(|(id, _)| *id == b"MARK")?;
        let mut markers = HashMap::new();
        let mut at = 2;
        for _ in 0..be16(mark, 0)? {
            let id = be16(mark, at)?;
            let position = be32(mark, at + 2)?;
            // A Pascal string, padded to an even length with its count.
            let len = usize::from(*mark.get(at + 6)?);
            markers.insert(id, position as usize);
            at += 6 + (len + 1).div_ceil(2) * 2;
        }
        Some((*markers.get(&begin)?, *markers.get(&end)?))
    })();
    (root, looped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert!(is_audio("./kick.wav"));
        assert!(is_audio("../a/B.FLAC"));
        assert!(!is_audio("./osc"));
        assert!(!is_audio("./notes.txt"));
        assert!(!is_audio("./.wav"));
        assert_eq!(extension("./x.tar.gz").as_deref(), Some("gz"));
    }

    #[test]
    #[cfg(feature = "audio-files")]
    fn resampling_keeps_time() {
        // An impulse at 1000 of 44.1kHz is at 1088.4 of 48kHz.
        let mut x = vec![0.0f32; 4410];
        x[1000] = 1.0;
        let y = resample(&[x], 44_100, 48_000);
        assert_eq!(y[0].len(), 4800);
        let peak = (0..y[0].len())
            .max_by(|&a, &b| y[0][a].total_cmp(&y[0][b]))
            .unwrap();
        assert!((1087..=1090).contains(&peak), "peak at {peak}");
    }
}
