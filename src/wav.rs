//! Minimal WAV writer.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::format::{OutSample, to_i24};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WavFormat {
    Pcm16,
    Pcm24,
    Float32,
}

impl WavFormat {
    fn bits(self) -> u16 {
        match self {
            WavFormat::Pcm16 => 16,
            WavFormat::Pcm24 => 24,
            WavFormat::Float32 => 32,
        }
    }

    fn tag(self) -> u16 {
        match self {
            WavFormat::Pcm16 | WavFormat::Pcm24 => 1, // WAVE_FORMAT_PCM
            WavFormat::Float32 => 3,                  // WAVE_FORMAT_IEEE_FLOAT
        }
    }
}

/// Write interleaved `samples` as a WAV stream.
pub fn write(
    mut w: impl Write,
    sample_rate: u32,
    channels: u16,
    format: WavFormat,
    samples: &[f32],
) -> io::Result<()> {
    let bytes_per_sample = u32::from(format.bits() / 8);
    let data_len = u32::try_from(samples.len())
        .ok()
        .and_then(|n| n.checked_mul(bytes_per_sample))
        .filter(|&n| n <= u32::MAX - 36)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "too long for WAV"))?;
    let block_align = u32::from(channels) * bytes_per_sample;

    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVE")?;

    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&format.tag().to_le_bytes())?;
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * block_align).to_le_bytes())?;
    w.write_all(&(block_align as u16).to_le_bytes())?;
    w.write_all(&format.bits().to_le_bytes())?;

    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for &x in samples {
        match format {
            WavFormat::Pcm16 => w.write_all(&i16::from_f32(x).to_le_bytes())?,
            WavFormat::Pcm24 => w.write_all(&to_i24(x).to_le_bytes()[..3])?,
            WavFormat::Float32 => w.write_all(&x.to_le_bytes())?,
        }
    }
    w.flush()
}

/// [`write`] to a file at `path`.
pub fn write_file(
    path: impl AsRef<Path>,
    sample_rate: u32,
    channels: u16,
    format: WavFormat,
    samples: &[f32],
) -> io::Result<()> {
    let file = BufWriter::new(File::create(path)?);
    write(file, sample_rate, channels, format, samples)
}
