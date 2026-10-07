use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use rill::offline::{self, Blocks};
use rill::wav::{self, WavFormat};
use rill::{Config, Engine, EventValue, Graph, ParamEvent, RillEvent, patches};

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
    /// Rill to start the program at.
    #[arg(long, default_value = rill::lang::build::DEFAULT_ENTRY)]
    entry: String,
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
        match rill::lang::load(&src, config, &self.entry) {
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
    /// Rill the program starts at.
    #[arg(long, default_value = rill::lang::build::DEFAULT_ENTRY)]
    entry: String,
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
        /// Rill the program starts at.
        #[arg(long, default_value = rill::lang::build::DEFAULT_ENTRY)]
        entry: String,
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

#[derive(Clone, Debug, PartialEq)]
struct ScheduledParamEvent {
    name: String,
    value: f32,
    time_seconds: f32,
}

#[derive(Clone, Debug, PartialEq)]
struct ScheduledRillEvent {
    name: String,
    time_seconds: f32,
    values: Vec<EventValue>,
}

fn parse_param_event(input: &str) -> Result<ScheduledParamEvent, String> {
    let (name, rest) = input
        .split_once('=')
        .ok_or_else(|| "expected NAME=VALUE@TIME".to_owned())?;
    let (value, time) = rest
        .split_once('@')
        .ok_or_else(|| "expected NAME=VALUE@TIME".to_owned())?;
    if name.is_empty() {
        return Err("parameter name cannot be empty".to_owned());
    }
    let value = value
        .parse::<f32>()
        .map_err(|_| format!("`{value}` is not a number"))?;
    let time_seconds = parse_time_seconds(time)?;
    Ok(ScheduledParamEvent {
        name: name.to_owned(),
        value,
        time_seconds,
    })
}

fn parse_rill_event(input: &str) -> Result<ScheduledRillEvent, String> {
    let (head, fields) = input
        .split_once(':')
        .map_or((input, ""), |(head, fields)| (head, fields));
    let (name, time) = head
        .split_once('@')
        .ok_or_else(|| "expected NAME@TIME[:FIELD=VALUE,...]".to_owned())?;
    if name.is_empty() {
        return Err("event name cannot be empty".to_owned());
    }
    let time_seconds = parse_time_seconds(time)?;
    let mut values = Vec::new();
    if !fields.is_empty() {
        for field in fields.split(',') {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| "expected event fields as FIELD=VALUE".to_owned())?;
            if name.is_empty() {
                return Err("event field name cannot be empty".to_owned());
            }
            values.push(EventValue {
                name: name.to_owned(),
                value: parse_event_value(value)?,
            });
        }
    }
    Ok(ScheduledRillEvent {
        name: name.to_owned(),
        time_seconds,
        values,
    })
}

fn parse_event_value(input: &str) -> Result<f32, String> {
    input.parse::<f32>().or_else(|_| {
        rill::lang::check::pitch_literal(input)
            .ok_or_else(|| format!("`{input}` is not a number or pitch"))
    })
}

fn parse_time_seconds(input: &str) -> Result<f32, String> {
    let (number, scale) = if let Some(ms) = input.strip_suffix("ms") {
        (ms, 0.001)
    } else if let Some(s) = input.strip_suffix('s') {
        (s, 1.0)
    } else {
        (input, 1.0)
    };
    let seconds = number
        .parse::<f32>()
        .map_err(|_| format!("`{input}` is not a time"))?
        * scale;
    if seconds.is_finite() && seconds >= 0.0 {
        Ok(seconds)
    } else {
        Err("time must be a non-negative finite number".to_owned())
    }
}

#[cfg(feature = "device")]
impl From<&ScheduledParamEvent> for rill::device::ScheduledParamEvent {
    fn from(event: &ScheduledParamEvent) -> Self {
        rill::device::ScheduledParamEvent {
            name: event.name.clone(),
            value: event.value,
            time_seconds: event.time_seconds,
        }
    }
}

#[cfg(feature = "device")]
impl From<&ScheduledRillEvent> for rill::device::ScheduledRillEvent {
    fn from(event: &ScheduledRillEvent) -> Self {
        rill::device::ScheduledRillEvent {
            name: event.name.clone(),
            time_seconds: event.time_seconds,
            values: event.values.clone(),
        }
    }
}

fn validate_param_event_duration(
    events: &[ScheduledParamEvent],
    seconds: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    if !(seconds.is_finite() && seconds >= 0.0) {
        return Err("--seconds must be a non-negative number".into());
    }
    for event in events {
        if event.time_seconds > seconds {
            return Err(format!(
                "parameter event `{}` is after the playback duration",
                event.name
            )
            .into());
        }
    }
    Ok(())
}

fn validate_rill_event_duration(
    events: &[ScheduledRillEvent],
    seconds: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    for event in events {
        if event.time_seconds > seconds {
            return Err(format!("event `{}` is after the playback duration", event.name).into());
        }
    }
    Ok(())
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
        /// Simulate a live control change, e.g. `gain=0.8@500ms`.
        #[arg(long = "param-event", value_name = "NAME=VALUE@TIME", value_parser = parse_param_event)]
        param_events: Vec<ScheduledParamEvent>,
        /// Simulate a rill event, e.g. `note_on@0ms:pitch=660,velocity=1`.
        #[arg(long = "event", value_name = "NAME@TIME[:FIELD=VALUE,...]", value_parser = parse_rill_event)]
        events: Vec<ScheduledRillEvent>,
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
        /// Simulate a live control change, e.g. `gain=0.8@500ms`.
        #[arg(long = "param-event", value_name = "NAME=VALUE@TIME", value_parser = parse_param_event)]
        param_events: Vec<ScheduledParamEvent>,
        /// Simulate a rill event, e.g. `note_on@0ms:pitch=660,velocity=1`.
        #[arg(long = "event", value_name = "NAME@TIME[:FIELD=VALUE,...]", value_parser = parse_rill_event)]
        events: Vec<ScheduledRillEvent>,
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
            param_events,
            events,
        } => {
            if let Some(seconds) = seconds {
                validate_param_event_duration(&param_events, seconds)?;
                validate_rill_event_duration(&events, seconds)?;
            }
            let options = rill::device::Options {
                host,
                buffer_frames: buffer,
                max_frames: None,
                param_events: param_events.iter().map(Into::into).collect(),
                events: events.iter().map(Into::into).collect(),
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
            Some(CheckView::Ast { file, entry, color }) => {
                let (src, program, checked) = check_file(&file, &entry)?;
                let tree = rill::lang::pretty::tree(&src, &program, &checked, color.enabled());
                print!("{tree}");
            }
            None => {
                let file = args.file.expect("required by clap");
                let (_, _, checked) = check_file(&file, &args.entry)?;
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
            param_events,
            events,
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
            let samples = if param_events.is_empty() && events.is_empty() {
                offline::render(&mut engine, frames, &Blocks::Fixed(block))
            } else {
                render_with_events(&mut engine, frames, block, &param_events, &events)?
            };
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

fn render_with_events(
    engine: &mut Engine,
    frames: usize,
    block: usize,
    param_events: &[ScheduledParamEvent],
    rill_events: &[ScheduledRillEvent],
) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let channels = engine.config().out_channels;
    let rate = engine.config().sample_rate as f32;
    let known = engine.params().collect::<Vec<_>>();
    for event in param_events {
        if !known.contains(&event.name.as_str()) {
            let available = if known.is_empty() {
                "this program exposes no live parameters".to_owned()
            } else {
                format!("available: {}", known.join(", "))
            };
            return Err(format!("unknown live parameter `{}` ({available})", event.name).into());
        }
        if (event.time_seconds * rate).round() as usize > frames {
            return Err(format!(
                "parameter event `{}` is after the render duration",
                event.name
            )
            .into());
        }
    }
    for event in rill_events {
        if (event.time_seconds * rate).round() as usize > frames {
            return Err(format!("event `{}` is after the render duration", event.name).into());
        }
    }

    let mut scheduled_params = param_events
        .iter()
        .map(|e| ((e.time_seconds * rate).round() as usize, e))
        .collect::<Vec<_>>();
    scheduled_params.sort_by_key(|(frame, _)| *frame);
    let mut scheduled_rill = rill_events
        .iter()
        .map(|e| ((e.time_seconds * rate).round() as usize, e))
        .collect::<Vec<_>>();
    scheduled_rill.sort_by_key(|(frame, _)| *frame);

    let mut out = vec![0.0; frames * channels];
    let mut done = 0;
    let mut next_param = 0;
    let mut next_rill = 0;
    while done < frames {
        let n = block.min(frames - done);
        let block_end = done + n;
        let start_param = next_param;
        while next_param < scheduled_params.len() && scheduled_params[next_param].0 <= block_end {
            next_param += 1;
        }
        let block_param_events = scheduled_params[start_param..next_param]
            .iter()
            .map(|(frame, event)| ParamEvent {
                frame_offset: frame - done,
                name: event.name.as_str(),
                value: event.value,
            })
            .collect::<Vec<_>>();
        let start_rill = next_rill;
        while next_rill < scheduled_rill.len() && scheduled_rill[next_rill].0 <= block_end {
            next_rill += 1;
        }
        let block_rill_events = scheduled_rill[start_rill..next_rill]
            .iter()
            .map(|(frame, event)| RillEvent {
                frame_offset: frame - done,
                name: event.name.as_str(),
                values: &event.values,
            })
            .collect::<Vec<_>>();
        engine.render_interleaved_with_param_and_rill_events(
            &mut out[done * channels..block_end * channels],
            |x| x,
            &block_param_events,
            &block_rill_events,
        );
        done = block_end;
    }
    Ok(out)
}

/// Parse and check `file`, with `entry` as the rill it starts at, printing
/// diagnostics to stderr. Fails if there were any errors.
fn check_file(
    file: &std::path::Path,
    entry: &str,
) -> Result<(String, rill::lang::ast::Program, rill::lang::Checked), Box<dyn std::error::Error>> {
    let src = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let name = file.display().to_string();
    match rill::lang::compile_entry(&src, entry) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_param_events() {
        assert_eq!(
            parse_param_event("gain=0.75@500ms").unwrap(),
            ScheduledParamEvent {
                name: "gain".to_owned(),
                value: 0.75,
                time_seconds: 0.5,
            }
        );
        assert_eq!(parse_param_event("depth=2@1s").unwrap().time_seconds, 1.0);
        assert!(parse_param_event("gain@1s").is_err());
        assert!(parse_param_event("gain=1@-1s").is_err());

        let event = parse_rill_event("note_on@250ms:pitch=660,velocity=1").unwrap();
        assert_eq!(event.name, "note_on");
        assert_eq!(event.time_seconds, 0.25);
        assert_eq!(event.values[0].name, "pitch");
        assert_eq!(event.values[0].value, 660.0);
    }
}
