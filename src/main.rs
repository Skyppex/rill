use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use rill::lang::value::Value;
use rill::offline::{self, Blocks};
use rill::wav::{self, WavFormat};
use rill::{Config, Engine, Graph, ParamEvent, RillEvent, patches};

#[derive(Parser)]
#[command(
    version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("RILL_COMMIT"),
        "; ",
        env!("RILL_RUSTC"),
        ")"
    ),
    about = "Rill: a language for real-time audio"
)]
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
    /// Seed for the numbers `random()` picks; the same seed picks the same
    /// numbers. Without one, every run picks a new seed and prints it.
    #[arg(long)]
    seed: Option<u64>,
}

/// The seed this run picked, when none was given.
static PICKED_SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

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
        let seed = self
            .seed
            .unwrap_or_else(|| *PICKED_SEED.get_or_init(rill::lang::build::new_seed));
        let options = rill::lang::build::Options {
            seed: Some(seed),
            ..options.clone()
        };
        let (sources, built) =
            rill::lang::load_file(path, &rill::lang::Disk, config, &self.entry, &options);
        match built {
            Ok((graph, checked)) => {
                for w in &checked.warnings {
                    eprint!("{}", sources.render(w));
                }
                if self.seed.is_none() && checked.uses_prelude("random") {
                    static SHOWN: std::sync::Once = std::sync::Once::new();
                    SHOWN.call_once(|| {
                        eprintln!("rill: random seed {seed} (repeat with --seed {seed})")
                    });
                }
                Ok(graph)
            }
            Err(diags) => {
                for d in &diags {
                    eprint!("{}", sources.render(d));
                }
                let errors = diags.iter().filter(|d| d.is_error()).count();
                Err(format!("{}: {errors} error(s)", path.display()))
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
    /// As written; checked against the parameter's type once the program
    /// is built.
    value: Value,
    time_seconds: f32,
}

#[derive(Clone, Debug, PartialEq)]
struct ScheduledRillEvent {
    /// An event kind (`note_on`) or the name of a declared event.
    target: String,
    time_seconds: f32,
    fields: Vec<(String, Value)>,
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
    let value = Value::parse(value)?;
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
    let mut values: Vec<(String, Value)> = Vec::new();
    if !fields.is_empty() {
        for field in fields.split(',') {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| "expected event fields as FIELD=VALUE".to_owned())?;
            if name.is_empty() {
                return Err("event field name cannot be empty".to_owned());
            }
            values.push((name.to_owned(), Value::parse(value)?));
        }
    }
    Ok(ScheduledRillEvent {
        target: name.to_owned(),
        time_seconds,
        fields: values,
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

#[cfg(feature = "device")]
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

#[cfg(feature = "device")]
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
    /// List the core library's modules and what each exports, or print one
    /// module's source, as in `rill core @gen/osc`.
    Core {
        /// A core module, as imported: `@gen/osc`.
        module: Option<String>,
    },
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
        /// Simulate a live control change, e.g. `volume=-6dB@500ms` or
        /// `cutoff=2kHz@1s`. Units are as in Rill and must fit the
        /// parameter; a plain number is the value as the program holds it.
        #[arg(long = "param-event", value_name = "NAME=VALUE@TIME", value_parser = parse_param_event)]
        param_events: Vec<ScheduledParamEvent>,
        /// Send an event. `note_on@1s:sender=5,channel=1,pitch=A4,velocity=0.8`
        /// is matched against the declared events; a declared name, as in
        /// `keys@1s:pitch=A4,velocity=0.8`, goes straight to that event.
        #[arg(long = "event", value_name = "EVENT@TIME[:FIELD=VALUE,...]", value_parser = parse_rill_event)]
        events: Vec<ScheduledRillEvent>,
        /// Do not reload the program when its files change. (Without the
        /// `watch` feature, files are never watched.)
        #[arg(long)]
        no_watch: bool,
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
        /// Run the program one sample at a time instead of over stretches
        /// of samples, to compare.
        #[arg(long)]
        per_sample: bool,
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
        /// Simulate a live control change, e.g. `volume=-6dB@500ms` or
        /// `cutoff=2kHz@1s`. Units are as in Rill and must fit the
        /// parameter; a plain number is the value as the program holds it.
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
            no_watch,
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
            let mut live = None;
            let playback = rill::device::play(
                |config| match &source.source {
                    Some(path) => {
                        let (l, player) = start_live(&source, path, config)?;
                        live = Some(l);
                        Ok(player)
                    }
                    None => {
                        let graph = source.graph(config).map_err(anyhow::Error::msg)?;
                        Ok(rill::live::Player::new(Engine::new(graph, *config)?))
                    }
                },
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
            if seconds.is_none() {
                println!("press Enter to stop");
            }
            control(live.as_mut(), seconds, !no_watch)?;
            drop(playback);
        }
        Command::Core { module: None } => {
            for (name, src) in rill::lang::core::FILES {
                println!("@{name}");
                for line in core_exports(src) {
                    println!("  {line}");
                }
            }
        }
        Command::Core { module: Some(m) } => {
            let name = m.strip_prefix('@').unwrap_or(&m);
            let name = name.strip_suffix(".rill").unwrap_or(name);
            match rill::lang::core::module(name) {
                Some(src) => print!("{src}"),
                None => {
                    let names: Vec<String> = rill::lang::core::names().collect();
                    return Err(format!(
                        "there is no core module `@{name}`; there are {}",
                        names.join(", ")
                    )
                    .into());
                }
            }
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
                let (sources, program, checked) = check_file(&file, &entry)?;
                let tree = rill::lang::pretty::tree(&sources, &program, &checked, color.enabled());
                print!("{tree}");
            }
            None => {
                let file = args.file.expect("required by clap");
                let (sources, _, checked) = check_file(&file, &args.entry)?;
                if args.signatures {
                    for sig in &checked.signatures {
                        println!("{sig}");
                    }
                }
                let count = |k| checked.signatures.iter().filter(|s| s.kind == k).count();
                let files = match sources.files.len() {
                    1 => String::new(),
                    n => format!("{n} files, "),
                };
                println!(
                    "{}: ok ({files}{} fn, {} rill)",
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
            per_sample,
        } => profile(&source, seconds, rate, channels, block, scalar, per_sample)?,
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
    let mut values = Vec::with_capacity(param_events.len());
    for event in param_events {
        values.push(engine.param_value(&event.name, &event.value)?);
        if let Some(hint) = engine.param_hint(&event.name, &event.value) {
            eprintln!("rill: note: {hint}");
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
        .zip(values)
        .map(|(e, v)| ((e.time_seconds * rate).round() as usize, e, v))
        .collect::<Vec<_>>();
    scheduled_params.sort_by_key(|(frame, _, _)| *frame);
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
            .map(|&(frame, event, value)| ParamEvent {
                frame_offset: frame - done,
                name: event.name.as_str(),
                value,
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
    per_sample: bool,
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
    let options = rill::lang::build::Options {
        vectorize: !scalar,
        stream: !per_sample,
        ..Default::default()
    };
    let mut engine = Engine::new(source.graph_with(&config, &options)?, config)?;
    let build_time = built.elapsed();

    let frames = (seconds * rate as f32).round() as usize;
    let mut out = vec![0.0f32; block * usize::from(channels)];
    // Warm up caches and branch predictors, then start from the beginning.
    for _ in 0..(rate as usize / 10).div_ceil(block) {
        engine.render_interleaved(&mut out);
    }
    engine.reset();

    let mut late = 0usize;
    let budget = Duration::from_secs_f64(block as f64 / f64::from(rate));
    // Every block's time, set aside first so measuring allocates nothing.
    let mut times: Vec<Duration> = Vec::with_capacity(frames.div_ceil(block));
    let cpu_before = cpu_usage();
    let started = Instant::now();
    let mut done = 0;
    while done < frames {
        let n = block.min(frames - done);
        let t = Instant::now();
        engine.render_interleaved(&mut out[..n * usize::from(channels)]);
        let took = t.elapsed();
        times.push(took);
        late += usize::from(took > budget);
        done += n;
    }
    let total = started.elapsed();
    let cpu = cpu_usage()
        .zip(cpu_before)
        .map(|(after, before)| after.since(&before));
    times.sort_unstable();
    let worst = times.last().copied().unwrap_or_default();
    // The block time that `p` of the blocks were faster than.
    let at = |p: f64| {
        let i = ((times.len() as f64 * p).ceil() as usize).clamp(1, times.len().max(1)) - 1;
        times.get(i).copied().unwrap_or_default()
    };
    std::hint::black_box(&out);

    let audio = frames as f64 / f64::from(rate);
    let load = total.as_secs_f64() / audio;
    let rill::lang::vm::ProgramSize {
        instructions,
        operations,
        registers,
    } = engine.program_size();
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let how = match (scalar, per_sample) {
        (false, false) => "",
        (true, false) => ", scalar",
        (false, true) => ", per sample",
        (true, true) => ", scalar, per sample",
    };
    println!("{} ({build} build{how})", source.name());
    println!(
        "  program:  {instructions} instructions per sample ({operations} operations), {registers} registers, {} node(s); built in {:.1} ms",
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
    println!(
        "  spread:   median {:.3} ms, p99 {:.3} ms, p99.9 {:.3} ms, worst {:.3} ms",
        ms(at(0.5)),
        ms(at(0.99)),
        ms(at(0.999)),
        ms(worst)
    );
    if let Some(cpu) = cpu {
        let busy = cpu.user + cpu.system;
        let share = |d: Duration| {
            if busy.is_zero() {
                0.0
            } else {
                d.as_secs_f64() / busy.as_secs_f64() * 100.0
            }
        };
        println!(
            "  cpu:      user {:.1} ms ({:.1}%), kernel {:.1} ms ({:.1}%); {} switches ({} forced); {} page faults ({} from disk)",
            ms(cpu.user),
            share(cpu.user),
            ms(cpu.system),
            share(cpu.system),
            cpu.voluntary + cpu.involuntary,
            cpu.involuntary,
            cpu.minor_faults + cpu.major_faults,
            cpu.major_faults
        );
    }
    // How much of the tick streams over samples.
    if let Some(stages) = engine.stages() {
        match &stages.fallback {
            Some(why) => println!("  stages:   sample by sample: {why}"),
            None => {
                let (streams, loops) = stages.counts();
                let total = (streams + loops).max(1);
                let streaming = stages.stages.iter().filter(|s| s.streams).count();
                let buffers = match engine.stream_buffers() {
                    Some((n, len)) => format!(
                        "; {n} buffers of {len} samples ({:.0} KB)",
                        (n * len * 4) as f64 / 1024.0
                    ),
                    None => "; runs sample by sample, as nothing streams".into(),
                };
                println!(
                    "  stages:   {streams} of {} instructions stream ({:.0}%); {} stages, {streaming} streaming{buffers}",
                    streams + loops,
                    streams as f64 * 100.0 / total as f64,
                    stages.stages.len()
                );
                if !stages.looping.is_empty() {
                    let why: Vec<String> = stages
                        .looping
                        .iter()
                        .map(|(why, count)| format!("{count} {why}"))
                        .collect();
                    println!("    looping: {}", why.join(", "));
                }
            }
        }
    }
    // Which copies run together, and why the others do not.
    let grouping = engine.grouping();
    if !grouping.is_empty() {
        let together = grouping.iter().filter(|g| g.why.is_none()).count();
        println!(
            "  grouped:  {together} of {} pools and loops run their copies together",
            grouping.len()
        );
        // The same thing in every copy of an outer pool once, with a count:
        // names that differ only by copy index, as `[3]`.
        let mut left: Vec<(String, usize, &str, usize)> = Vec::new();
        for g in grouping.iter().filter(|g| g.why.is_some()) {
            let name = any_copy(&g.name);
            let why = g.why.as_deref().unwrap_or_default();
            match left
                .iter_mut()
                .find(|(n, c, w, _)| *n == name && *c == g.copies && *w == why)
            {
                Some(same) => same.3 += 1,
                None => left.push((name, g.copies, why, 1)),
            }
        }
        for (name, copies, why, times) in left {
            let times = if times > 1 {
                format!(", {times} times")
            } else {
                String::new()
            };
            println!("    {name} ({copies} copies{times}): {why}");
        }
    }
    Ok(())
}

/// CPU time and interruptions of the thread doing the rendering (of the
/// whole process where the OS cannot tell threads apart), from `getrusage`.
struct CpuUsage {
    user: std::time::Duration,
    system: std::time::Duration,
    /// Times the thread gave up the CPU (to wait), and was made to.
    voluntary: i64,
    involuntary: i64,
    /// Pages touched for the first time, and pages read back from disk.
    minor_faults: i64,
    major_faults: i64,
}

impl CpuUsage {
    fn since(&self, before: &CpuUsage) -> CpuUsage {
        CpuUsage {
            user: self.user.saturating_sub(before.user),
            system: self.system.saturating_sub(before.system),
            voluntary: self.voluntary - before.voluntary,
            involuntary: self.involuntary - before.involuntary,
            minor_faults: self.minor_faults - before.minor_faults,
            major_faults: self.major_faults - before.major_faults,
        }
    }
}

#[cfg(unix)]
fn cpu_usage() -> Option<CpuUsage> {
    #[cfg(target_os = "linux")]
    let who = libc::RUSAGE_THREAD;
    #[cfg(not(target_os = "linux"))]
    let who = libc::RUSAGE_SELF;
    // SAFETY: `getrusage` fills in the struct it is given, which starts
    // zeroed (a valid `rusage`).
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(who, &mut usage) != 0 {
            return None;
        }
        usage
    };
    let time = |t: libc::timeval| {
        std::time::Duration::from_secs(t.tv_sec as u64)
            + std::time::Duration::from_micros(t.tv_usec as u64)
    };
    Some(CpuUsage {
        user: time(usage.ru_utime),
        system: time(usage.ru_stime),
        voluntary: usage.ru_nvcsw as i64,
        involuntary: usage.ru_nivcsw as i64,
        minor_faults: usage.ru_minflt as i64,
        major_faults: usage.ru_majflt as i64,
    })
}

#[cfg(not(unix))]
fn cpu_usage() -> Option<CpuUsage> {
    None
}

/// `name` with every copy index (`[3]`) as `[*]`.
fn any_copy(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find(']') {
            Some(close) if close > 0 && after[..close].bytes().all(|b| b.is_ascii_digit()) => {
                out.push_str("[*]");
                rest = &after[close + 1..];
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// What a core module exports, one line each: `rill saw(freq: Freq) Sample`.
fn core_exports(src: &str) -> Vec<String> {
    let Ok(program) = rill::lang::parse(src) else {
        return Vec::new();
    };
    let text = |span: rill::lang::Span| &src[span.start as usize..span.end as usize];
    let mut out: Vec<(u32, String)> = Vec::new();
    for item in &program.items {
        let d = item.def();
        if d.export.is_some() {
            // The signature: everything before the body, without `export`.
            let head = text(rill::lang::Span {
                start: d.span.start,
                end: d.body.span.start,
            });
            let head = head.trim_start_matches("export").trim();
            out.push((
                d.span.start,
                head.split_whitespace().collect::<Vec<_>>().join(" "),
            ));
        }
    }
    for c in program.consts.iter().filter(|c| c.export.is_some()) {
        out.push((
            c.span.start,
            text(c.span).trim_start_matches("export").trim().to_owned(),
        ));
    }
    for s in program.seqs.iter().filter(|s| s.export.is_some()) {
        out.push((s.span.start, format!("seq {}", s.name.name)));
    }
    for e in program.events.iter().filter(|e| e.export.is_some()) {
        out.push((
            e.span.start,
            text(e.span).trim_start_matches("export").trim().to_owned(),
        ));
    }
    for i in program.imports.iter().filter(|i| i.export.is_some()) {
        out.push((i.span.start, format!("everything from `{}`", i.path)));
    }
    out.sort();
    out.into_iter().map(|(_, s)| s).collect()
}

/// Build the program at `path` to play live: rebuilt when its files
/// change, and when a value outgrows its `max`. Prints its diagnostics.
#[cfg(feature = "device")]
fn start_live(
    source: &SourceArgs,
    path: &std::path::Path,
    config: &Config,
) -> anyhow::Result<(rill::live::Live, rill::live::Player)> {
    let seed = source
        .seed
        .unwrap_or_else(|| *PICKED_SEED.get_or_init(rill::lang::build::new_seed));
    let options = rill::lang::build::Options {
        seed: Some(seed),
        ..Default::default()
    };
    match rill::live::Live::from_file(path, *config, &source.entry, options) {
        Ok((live, player, built)) => {
            for w in &built.warnings {
                eprint!("{}", built.sources.render(w));
            }
            if source.seed.is_none() && built.random {
                eprintln!("rill: random seed {seed} (repeat with --seed {seed})");
            }
            Ok((live, player))
        }
        Err(failed) => {
            for d in &failed.errors {
                eprint!("{}", failed.sources.render(d));
            }
            let errors = failed.errors.iter().filter(|d| d.is_error()).count();
            anyhow::bail!("{}: {errors} error(s)", path.display())
        }
    }
}

/// While playing: reload when the program's files change (if `watch`),
/// grow ceilings when values outgrow them, and stop after `seconds` or when
/// Enter is pressed.
#[cfg(feature = "device")]
fn control(
    mut live: Option<&mut rill::live::Live>,
    seconds: Option<f32>,
    watch: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    enum Message {
        Stop,
        #[cfg_attr(not(feature = "watch"), allow(dead_code))]
        Changed(PathBuf),
    }
    impl From<PathBuf> for Message {
        fn from(path: PathBuf) -> Message {
            Message::Changed(path)
        }
    }
    let (tx, rx) = mpsc::channel();
    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs_f32(s));
    if deadline.is_none() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = std::io::stdin().read_line(&mut String::new());
            let _ = tx.send(Message::Stop);
        });
    }
    #[cfg(feature = "watch")]
    let mut watcher = match (&live, watch) {
        (Some(live), true) => Some(Watcher::new(live.files(), tx.clone())?),
        _ => None,
    };
    #[cfg(not(feature = "watch"))]
    let _ = watch;

    loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Message::Stop) => return Ok(()),
            Ok(Message::Changed(path)) => {
                let Some(live) = live.as_deref_mut() else {
                    continue;
                };
                if !live.uses(&path) {
                    continue;
                }
                // Editors often write a file in several steps; take changes
                // close together as one.
                std::thread::sleep(Duration::from_millis(50));
                while let Ok(m) = rx.try_recv() {
                    if let Message::Stop = m {
                        return Ok(());
                    }
                }
                match live.reload() {
                    Ok(built) => {
                        for w in &built.warnings {
                            eprint!("{}", built.sources.render(w));
                        }
                        let name = built
                            .sources
                            .files
                            .first()
                            .map_or("", |f| f.display.as_str());
                        println!(
                            "reloaded {name} ({} states carried, {} new)",
                            built.report.carried, built.report.fresh
                        );
                        #[cfg(feature = "watch")]
                        if let Some(w) = &mut watcher {
                            w.follow(live.files());
                        }
                    }
                    Err(failed) => {
                        for d in &failed.errors {
                            eprint!("{}", failed.sources.render(d));
                        }
                        eprintln!("rill: still playing the last version that built");
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(());
        }
        if let Some(live) = live.as_deref_mut()
            && let Some(grown) = live.grow()
        {
            match grown {
                Ok(_) => println!("grew to fit: {}", describe_raised(live.raised())),
                Err(failed) => {
                    for d in &failed.errors {
                        eprint!("{}", failed.sources.render(d));
                    }
                }
            }
        }
    }
}

/// The ceilings raised so far, briefly.
#[cfg(feature = "device")]
fn describe_raised(raised: &std::collections::HashMap<String, f32>) -> String {
    let mut all: Vec<String> = raised.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    all.sort();
    all.join(", ")
}

/// Watches the folders of a program's files, sending the paths that change.
#[cfg(all(feature = "device", feature = "watch"))]
struct Watcher {
    inner: notify::RecommendedWatcher,
    folders: Vec<PathBuf>,
}

#[cfg(all(feature = "device", feature = "watch"))]
impl Watcher {
    fn new<M: From<PathBuf> + Send + 'static>(
        files: &[PathBuf],
        tx: std::sync::mpsc::Sender<M>,
    ) -> notify::Result<Watcher> {
        let inner = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if matches!(
                event.kind,
                notify::EventKind::Modify(_) | notify::EventKind::Create(_)
            ) {
                for path in event.paths {
                    let _ = tx.send(M::from(path));
                }
            }
        })?;
        let mut w = Watcher {
            inner,
            folders: Vec::new(),
        };
        w.follow(files);
        Ok(w)
    }

    /// Watch the folders of `files` (editors often replace a file rather than
    /// write to it, so the folder is what sees it).
    fn follow(&mut self, files: &[PathBuf]) {
        use notify::Watcher as _;
        for file in files {
            let folder = file
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."))
                .to_owned();
            if !self.folders.contains(&folder)
                && self
                    .inner
                    .watch(&folder, notify::RecursiveMode::NonRecursive)
                    .is_ok()
            {
                self.folders.push(folder);
            }
        }
    }
}

/// Whether the root file of `program` exports anything.
fn is_library(program: &rill::lang::ast::Program) -> bool {
    let root = |m: usize| m == 0;
    let m = &program.modules;
    (0..program.items.len()).any(|i| root(m.item(i)) && program.items[i].def().export.is_some())
        || (0..program.seqs.len()).any(|i| root(m.seq(i)) && program.seqs[i].export.is_some())
        || (0..program.events.len()).any(|i| root(m.event(i)) && program.events[i].export.is_some())
        || (0..program.consts.len()).any(|i| root(m.konst(i)) && program.consts[i].export.is_some())
        || (0..program.imports.len())
            .any(|i| root(m.import(i)) && program.imports[i].export.is_some())
}

/// Parse and check `file`, with `entry` as the rill it starts at, printing
/// diagnostics to stderr. Fails if there were any errors.
fn check_file(
    file: &std::path::Path,
    entry: &str,
) -> Result<
    (
        rill::lang::SourceMap,
        rill::lang::ast::Program,
        rill::lang::Checked,
    ),
    Box<dyn std::error::Error>,
> {
    // A file that exports something is a library: it need not run on its
    // own, unless asked to with `--entry`.
    let (sources, result) = match rill::lang::compile_file(file, &rill::lang::Disk) {
        (sources, Ok((program, checked)))
            if entry == rill::lang::build::DEFAULT_ENTRY
                && is_library(&program)
                && checked.scopes.first().and_then(|s| s.def(entry)).is_none() =>
        {
            (sources, Ok((program, checked)))
        }
        _ => rill::lang::compile_file_entry(file, entry, &rill::lang::Disk),
    };
    match result {
        Ok((program, checked)) => {
            for w in &checked.warnings {
                eprint!("{}", sources.render(w));
            }
            Ok((sources, program, checked))
        }
        Err(diags) => {
            for d in &diags {
                eprint!("{}", sources.render(d));
            }
            let errors = diags.iter().filter(|d| d.is_error()).count();
            Err(format!("{}: {errors} error(s)", file.display()).into())
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
                value: Value::number(0.75),
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
            [
                ("pitch".to_owned(), Value::Pitch(69.0)),
                ("velocity".to_owned(), Value::number(1.0))
            ]
        );
    }

    #[test]
    fn values_can_be_written_with_units() {
        let src = "rill main(volume: Gain = 0dB, cutoff: Freq = 1kHz) [Sample; 2] {
            let one: Sample = 1.0
            return [one + volume, cutoff / 1Hz]
        }";
        let config = Config {
            sample_rate: 48_000,
            max_frames: 64,
            out_channels: 2,
        };
        let engine = || {
            let (graph, _) = rill::lang::load(src, &config, "main").unwrap();
            Engine::new(graph, config).unwrap()
        };
        let render = |events: &[&str]| {
            let events: Vec<ScheduledParamEvent> = events
                .iter()
                .map(|e| parse_param_event(e).unwrap())
                .collect();
            render_with_events(&mut engine(), 4, 64, &events, &[])
        };
        // Controls glide to a new value; by the last frame of a long
        // render they are there.
        let last = |events: &[&str]| {
            let events: Vec<ScheduledParamEvent> = events
                .iter()
                .map(|e| parse_param_event(e).unwrap())
                .collect();
            let out = render_with_events(&mut engine(), 24_000, 64, &events, &[]).unwrap();
            [out[out.len() - 2], out[out.len() - 1]]
        };
        let [volume, cutoff] = last(&["volume=-6dB@0s", "cutoff=2.5kHz@0s"]);
        assert!((volume - 0.501).abs() < 1e-3, "{volume}");
        assert!((cutoff - 2500.0).abs() < 1e-2, "{cutoff}");
        // A plain number is still the value as the program holds it.
        assert!((last(&["volume=0.25@0s"])[0] - 0.25).abs() < 1e-4);
        // A unit that does not fit says what would.
        let wrong = render(&["volume=-6Hz@0s"]).unwrap_err().to_string();
        assert!(
            wrong.contains("parameter `volume`") && wrong.contains("`dB`"),
            "{wrong}"
        );
        assert!(render(&["cutoff=-6dB@0s"]).is_err());
    }
}
