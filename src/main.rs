use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use rill::offline::{self, Blocks};
use rill::wav::{self, WavFormat};
use rill::{Config, Engine, Graph, patches};

#[derive(Parser)]
#[command(version, about = "Rill: a language for real-time audio")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct PatchArgs {
    /// Which built-in patch to run.
    #[arg(long, default_value = "sine", value_parser = clap::builder::PossibleValuesParser::new(patches::NAMES))]
    patch: String,
    /// Oscillator frequency in Hz.
    #[arg(long, default_value_t = 440.0)]
    freq: f32,
    /// Output gain.
    #[arg(long, default_value_t = 0.3)]
    gain: f32,
}

impl PatchArgs {
    fn graph(&self) -> Graph {
        patches::by_name(&self.patch, self.freq, self.gain).expect("validated by clap")
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    F32,
    I16,
    I24,
}

#[derive(Subcommand)]
enum Command {
    /// Play a patch on the default audio device.
    #[cfg(feature = "device")]
    Play {
        #[command(flatten)]
        patch: PatchArgs,
        /// Stop after this many seconds instead of waiting for Enter.
        #[arg(long)]
        seconds: Option<f32>,
        /// Audio host to use (see `rill devices`).
        #[arg(long)]
        host: Option<String>,
        /// Request a fixed callback size from the device.
        #[arg(long)]
        buffer: Option<u32>,
    },
    /// List audio hosts and output devices.
    #[cfg(feature = "device")]
    Devices,
    /// Render a patch to a WAV file through a simulated audio callback.
    Render {
        /// Output file.
        out: PathBuf,
        #[command(flatten)]
        patch: PatchArgs,
        #[arg(long, default_value_t = 2.0)]
        seconds: f32,
        #[arg(long, default_value_t = 48_000)]
        rate: u32,
        #[arg(long, default_value_t = 2)]
        channels: u16,
        /// Frames per simulated callback.
        #[arg(long, default_value_t = 256)]
        block: usize,
        #[arg(long, value_enum, default_value_t = Format::F32)]
        format: Format,
    },
}

fn main() -> ExitCode {
    #[cfg(feature = "pulseaudio")]
    // SAFETY: nothing else has started yet, so no thread can race on the
    // environment.
    if let Err(err) = unsafe { rill::device::ensure_pulse_cookie() } {
        eprintln!("rill: warning: could not write a PulseAudio cookie: {err}");
    }

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rill: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        #[cfg(feature = "device")]
        Command::Play {
            patch,
            seconds,
            host,
            buffer,
        } => {
            let options = rill::device::Options {
                host,
                buffer_frames: buffer,
                max_frames: None,
            };
            let playback = rill::device::play(patch.graph(), &options)?;
            let buffer = match playback.buffer_frames {
                Some(frames) => format!("{frames}-frame buffer"),
                None => "default buffer".to_owned(),
            };
            println!(
                "playing `{}` on {} ({} Hz, {} ch, {}, {buffer})",
                patch.patch,
                playback.device,
                playback.config.sample_rate,
                playback.config.out_channels,
                playback.format
            );
            match seconds {
                Some(s) => std::thread::sleep(std::time::Duration::from_secs_f32(s)),
                None => {
                    println!("press Enter to stop");
                    std::io::stdin().read_line(&mut String::new())?;
                }
            }
            drop(playback);
        }
        #[cfg(feature = "device")]
        Command::Devices => {
            for (host, devices) in rill::device::list()? {
                println!("{host}");
                for device in devices {
                    println!("  {device}");
                }
            }
        }
        Command::Render {
            out,
            patch,
            seconds,
            rate,
            channels,
            block,
            format,
        } => {
            if block == 0 {
                return Err("--block must be > 0".into());
            }
            if !(seconds.is_finite() && seconds >= 0.0) {
                return Err("--seconds must be a non-negative number".into());
            }
            let config = Config {
                sample_rate: rate,
                max_frames: block,
                out_channels: usize::from(channels),
            };
            let mut engine = Engine::new(patch.graph(), config)?;
            let frames = (seconds * rate as f32).round() as usize;
            let samples = offline::render(&mut engine, frames, &Blocks::Fixed(block));
            let format = match format {
                Format::F32 => WavFormat::Float32,
                Format::I16 => WavFormat::Pcm16,
                Format::I24 => WavFormat::Pcm24,
            };
            wav::write_file(&out, rate, channels, format, &samples)?;
            println!("wrote {frames} frames to {}", out.display());
        }
    }
    Ok(())
}
