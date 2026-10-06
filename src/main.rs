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
struct SourceArgs {
    /// Rill source file to run.
    #[arg(required_unless_present = "patch", conflicts_with = "patch")]
    source: Option<PathBuf>,
    /// Run a built-in patch instead of a source file.
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(patches::NAMES))]
    patch: Option<String>,
    /// Oscillator frequency in Hz, for `--patch`.
    #[arg(long, default_value_t = 440.0, requires = "patch")]
    freq: f32,
    /// Output gain, for `--patch`.
    #[arg(long, default_value_t = 0.3, requires = "patch")]
    gain: f32,
}

impl SourceArgs {
    fn name(&self) -> String {
        match (&self.source, &self.patch) {
            (Some(path), _) => path.display().to_string(),
            (None, Some(patch)) => format!("patch `{patch}`"),
            (None, None) => unreachable!("required by clap"),
        }
    }

    /// Build the graph for an engine with `config`, printing any
    /// diagnostics.
    fn graph(&self, config: &Config) -> Result<Graph, String> {
        let Some(path) = &self.source else {
            let patch = self.patch.as_deref().expect("required by clap");
            return Ok(patches::by_name(patch, self.freq, self.gain).expect("validated by clap"));
        };
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let name = path.display().to_string();
        match rill::lang::load(&src, config) {
            Ok((graph, warnings)) => {
                for w in &warnings {
                    eprint!("{}", w.render(&name, &src));
                }
                Ok(graph)
            }
            Err(diags) => {
                for d in &diags {
                    eprint!("{}", d.render(&name, &src));
                }
                let errors = diags.iter().filter(|d| d.is_error()).count();
                Err(format!("{name}: {errors} error(s)"))
            }
        }
    }
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct CheckArgs {
    #[command(subcommand)]
    view: Option<CheckView>,
    /// Source file.
    #[arg(required = true)]
    file: Option<PathBuf>,
    /// Print the signature of every fn and rill.
    #[arg(long)]
    signatures: bool,
}

#[derive(Subcommand)]
enum CheckView {
    /// Check, then print the syntax tree with every expression's type.
    Ast {
        /// Source file.
        file: PathBuf,
        /// Colour the tree. `auto` colours only when writing to a terminal
        /// and `NO_COLOR` is unset.
        #[arg(long, value_enum, default_value_t = Color::Auto)]
        color: Color,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Color {
    Auto,
    Always,
    Never,
}

impl Color {
    fn enabled(self) -> bool {
        use std::io::IsTerminal;
        match self {
            Color::Always => true,
            Color::Never => false,
            Color::Auto => {
                std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
            }
        }
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
    /// Play a Rill file (or a built-in patch) on the default audio device.
    #[cfg(feature = "device")]
    Play {
        #[command(flatten)]
        source: SourceArgs,
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
    /// Parse and type-check a Rill source file.
    Check(CheckArgs),
    /// Render a Rill file (or a built-in patch) to a WAV file through a
    /// simulated audio callback.
    Render {
        #[command(flatten)]
        source: SourceArgs,
        /// Output file.
        #[arg(short, long)]
        out: PathBuf,
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
            source,
            seconds,
            host,
            buffer,
        } => {
            let options = rill::device::Options {
                host,
                buffer_frames: buffer,
                max_frames: None,
            };
            let playback = rill::device::play(
                |config| source.graph(config).map_err(anyhow::Error::msg),
                &options,
            )?;
            let buffer = match playback.buffer_frames {
                Some(frames) => format!("{frames}-frame buffer"),
                None => "default buffer".to_owned(),
            };
            println!(
                "playing {} on {} ({} Hz, {} ch, {}, {buffer})",
                source.name(),
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
        Command::Check(args) => match args.view {
            Some(CheckView::Ast { file, color }) => {
                let (src, program, checked) = check_file(&file)?;
                let tree = rill::lang::pretty::tree(&src, &program, &checked, color.enabled());
                print!("{tree}");
            }
            None => {
                let file = args.file.expect("required by clap");
                let (_, _, checked) = check_file(&file)?;
                if args.signatures {
                    for sig in &checked.signatures {
                        println!("{sig}");
                    }
                }
                let count = |k| checked.signatures.iter().filter(|s| s.kind == k).count();
                println!(
                    "{}: ok ({} fn, {} rill)",
                    file.display(),
                    count(rill::lang::types::DefKind::Fn),
                    count(rill::lang::types::DefKind::Rill)
                );
            }
        },
        Command::Render {
            source,
            out,
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
            let mut engine = Engine::new(source.graph(&config)?, config)?;
            let frames = (seconds * rate as f32).round() as usize;
            let samples = offline::render(&mut engine, frames, &Blocks::Fixed(block));
            let format = match format {
                Format::F32 => WavFormat::Float32,
                Format::I16 => WavFormat::Pcm16,
                Format::I24 => WavFormat::Pcm24,
            };
            wav::write_file(&out, rate, channels, format, &samples)?;
            println!(
                "rendered {} to {} ({frames} frames)",
                source.name(),
                out.display()
            );
        }
    }
    Ok(())
}

/// Parse and check `file`, printing diagnostics to stderr. Fails if there
/// were any errors.
fn check_file(
    file: &std::path::Path,
) -> Result<(String, rill::lang::ast::Program, rill::lang::Checked), Box<dyn std::error::Error>> {
    let src = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let name = file.display().to_string();
    match rill::lang::compile(&src) {
        Ok((program, checked)) => {
            for w in &checked.warnings {
                eprint!("{}", w.render(&name, &src));
            }
            Ok((src, program, checked))
        }
        Err(diags) => {
            for d in &diags {
                eprint!("{}", d.render(&name, &src));
            }
            let errors = diags.iter().filter(|d| d.is_error()).count();
            Err(format!("{name}: {errors} error(s)").into())
        }
    }
}
