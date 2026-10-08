use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use rill::offline::{self, Blocks};
use rill::wav::{self, WavFormat};
use rill::{Config, Engine, Graph, ParamEvent, RillEvent, patches};

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
        self.graph_with(config, &rill::lang::build::Options::default())
    }

    fn graph_with(
        &self,
        config: &Config,
        options: &rill::lang::build::Options,
    ) -> Result<Graph, String> {
        let Some(path) = &self.source else {
            let patch = self.patch.as_deref().expect("required by clap");
            return Ok(patches::by_name(patch, self.freq, self.gain).expect("validated by clap"));
        };
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let name = path.display().to_string();
        match rill::lang::load_with(&src, config, &self.entry, options) {
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
    /// An event kind (`note_on`) or the name of a declared event.
    target: String,
    time_seconds: f32,
    fields: Vec<(String, f32)>,
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
        .ok_or_else(|| "expected EVENT@TIME[:FIELD=VALUE,...]".to_owned())?;
    if name.is_empty() {
        return Err("event name cannot be empty".to_owned());
    }
    let time_seconds = parse_time_seconds(time)?;
    let mut values: Vec<(String, f32)> = Vec::new();
    if !fields.is_empty() {
        for field in fields.split(',') {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| "expected event fields as FIELD=VALUE".to_owned())?;
            if name.is_empty() {
                return Err("event field name cannot be empty".to_owned());
            }
            values.push((name.to_owned(), parse_event_value(value)?));
        }
    }
    Ok(ScheduledRillEvent {
        target: name.to_owned(),
        time_seconds,
        fields: values,
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
            target: event.target.clone(),
            time_seconds: event.time_seconds,
            fields: event.fields.clone(),
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
            return Err(format!("event `{}` is after the playback duration", event.target).into());
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
        /// Send an event. `note_on@1s:sender=5,channel=1,pitch=A4,velocity=0.8`
        /// is matched against the declared events; a declared name, as in
        /// `keys@1s:pitch=A4,velocity=0.8`, goes straight to that event.
        #[arg(long = "event", value_name = "EVENT@TIME[:FIELD=VALUE,...]", value_parser = parse_rill_event)]
        events: Vec<ScheduledRillEvent>,
    },
    /// List audio hosts and output devices.
    #[cfg(feature = "device")]
    Devices,
    /// Parse and type-check a Rill source file.
    Check(CheckArgs),
    /// Measure how fast a program runs: render it as fast as possible,
    /// with no audio device, and report the load it would put on the audio
    /// thread. Works the same in debug and release builds, though debug
    /// builds run much slower.
    Profile {
        #[command(flatten)]
        source: SourceArgs,
        /// Seconds of audio to render.
        #[arg(long, default_value_t = 10.0)]
        seconds: f32,
        #[arg(long, default_value_t = 48_000)]
        rate: u32,
        #[arg(long, default_value_t = 2)]
        channels: u16,
        /// Frames per simulated callback.
        #[arg(long, default_value_t = 256)]
        block: usize,
        /// Run every copy of repeated code on its own instead of together,
        /// to compare.
        #[arg(long)]
        scalar: bool,
    },
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
        /// Send an event. `note_on@1s:sender=5,channel=1,pitch=A4,velocity=0.8`
        /// is matched against the declared events; a declared name, as in
        /// `keys@1s:pitch=A4,velocity=0.8`, goes straight to that event.
        #[arg(long = "event", value_name = "EVENT@TIME[:FIELD=VALUE,...]", value_parser = parse_rill_event)]
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
        Command::Profile {
            source,
            seconds,
            rate,
            channels,
            block,
            scalar,
        } => profile(&source, seconds, rate, channels, block, scalar)?,
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
    let mut dispatches = Vec::with_capacity(rill_events.len());
    for event in rill_events {
        if (event.time_seconds * rate).round() as usize > frames {
            return Err(format!("event `{}` is after the render duration", event.target).into());
        }
        dispatches.push(rill::event::parse_dispatch(
            engine.events(),
            &event.target,
            &event.fields,
        )?);
    }

    let mut scheduled_params = param_events
        .iter()
        .map(|e| ((e.time_seconds * rate).round() as usize, e))
        .collect::<Vec<_>>();
    scheduled_params.sort_by_key(|(frame, _)| *frame);
    let mut scheduled_rill = rill_events
        .iter()
        .zip(dispatches)
        .map(|(e, d)| ((e.time_seconds * rate).round() as usize, d))
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
            .map(|&(frame, dispatch)| RillEvent {
                frame_offset: frame - done,
                dispatch,
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

/// `rill profile`: render as fast as possible and report the load.
fn profile(
    source: &SourceArgs,
    seconds: f32,
    rate: u32,
    channels: u16,
    block: usize,
    scalar: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::{Duration, Instant};
    if block == 0 {
        return Err("--block must be > 0".into());
    }
    if !(seconds.is_finite() && seconds > 0.0) {
        return Err("--seconds must be a positive number".into());
    }
    let config = Config {
        sample_rate: rate,
        max_frames: block,
        out_channels: usize::from(channels),
    };
    let built = Instant::now();
    let options = rill::lang::build::Options { vectorize: !scalar };
    let mut engine = Engine::new(source.graph_with(&config, &options)?, config)?;
    let build_time = built.elapsed();

    let frames = (seconds * rate as f32).round() as usize;
    let mut out = vec![0.0f32; block * usize::from(channels)];
    // Warm up caches and branch predictors, then start from the beginning.
    for _ in 0..(rate as usize / 10).div_ceil(block) {
        engine.render_interleaved(&mut out);
    }
    engine.reset();

    let mut worst = Duration::ZERO;
    let mut late = 0usize;
    let budget = Duration::from_secs_f64(block as f64 / f64::from(rate));
    let started = Instant::now();
    let mut done = 0;
    while done < frames {
        let n = block.min(frames - done);
        let t = Instant::now();
        engine.render_interleaved(&mut out[..n * usize::from(channels)]);
        let took = t.elapsed();
        worst = worst.max(took);
        late += usize::from(took > budget);
        done += n;
    }
    let total = started.elapsed();
    std::hint::black_box(&out);

    let audio = frames as f64 / f64::from(rate);
    let load = total.as_secs_f64() / audio;
    let (instructions, registers) = engine.program_size();
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let how = if scalar { ", scalar" } else { "" };
    println!("{} ({build} build{how})", source.name());
    println!(
        "  program:  {instructions} instructions per sample, {registers} registers, {} node(s); built in {:.1} ms",
        engine.node_count(),
        ms(build_time)
    );
    println!(
        "  rendered: {audio:.2} s of audio in {:.3} s, {:.0} ns per sample",
        total.as_secs_f64(),
        total.as_secs_f64() * 1e9 / frames as f64
    );
    println!(
        "  load:     {:.1}% of one core ({:.1}x faster than real time)",
        load * 100.0,
        1.0 / load
    );
    println!(
        "  blocks:   worst {:.3} ms of a {:.3} ms budget ({block} frames); {late} of {} over budget",
        ms(worst),
        ms(budget),
        frames.div_ceil(block)
    );
    Ok(())
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

        let event = parse_rill_event("note_on@250ms:pitch=A4,velocity=1").unwrap();
        assert_eq!(event.target, "note_on");
        assert_eq!(event.time_seconds, 0.25);
        assert_eq!(
            event.fields,
            [("pitch".to_owned(), 69.0), ("velocity".to_owned(), 1.0)]
        );
    }
}
