//! Real-time output through the system audio device, via cpal.
//!
//! The device dictates sample rate, channel count and sample format; the
//! graph is frozen against those and the resulting [`Engine`] is moved into
//! the audio callback, which owns it from then on.
//!
//! On WSLg, ALSA has no real device; build with `--features pulseaudio` and
//! pass `--host pulseaudio`. The pure-Rust PulseAudio client cpal uses needs
//! a cookie file even though WSLg's server ignores it; see
//! [`ensure_pulse_cookie`].

use anyhow::{Context as _, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::engine::{Config, Engine};
use crate::graph::Graph;

/// If no PulseAudio cookie can be found, write an all-zero one to the temp
/// directory and point `$PULSE_COOKIE` at it for this process.
///
/// Without a cookie the pulseaudio client sends an empty one and the server
/// hangs up, which surfaces as "failed to fill whole buffer". Servers with
/// anonymous auth (WSLg, most desktop sessions) accept zeros.
///
/// # Safety
///
/// Sets an environment variable, so no other thread may be reading or
/// writing the environment. Call it at startup, before spawning threads.
pub unsafe fn ensure_pulse_cookie() -> std::io::Result<()> {
    let found = std::env::var_os("PULSE_COOKIE")
        .map(std::path::PathBuf::from)
        .into_iter()
        .chain(std::env::home_dir().into_iter().flat_map(|home| {
            [
                home.join(".config/pulse/cookie"),
                home.join(".pulse-cookie"),
            ]
        }))
        .any(|path| path.exists());
    if found {
        return Ok(());
    }
    let path = std::env::temp_dir().join("rill-pulse-cookie");
    std::fs::write(&path, [0u8; 256])?;
    // SAFETY: the caller guarantees no concurrent environment access.
    unsafe { std::env::set_var("PULSE_COOKIE", path) };
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// cpal host name, e.g. "alsa" or "pulseaudio". `None` for the default.
    pub host: Option<String>,
    /// Ask the device for this many frames per callback. Defaults to about
    /// [`DEFAULT_BUFFER_MS`] when the device reports a supported range.
    pub buffer_frames: Option<u32>,
    /// Upper bound on frames per engine pass. Defaults to 1024.
    pub max_frames: Option<usize>,
}

/// Callback period requested when the caller does not pick one.
///
/// Leaving it to the backend is not an option: cpal's PulseAudio host then
/// leaves every buffer attribute to the server, which defaults to two
/// seconds of buffering, so sound starts and stops two seconds late.
pub const DEFAULT_BUFFER_MS: u32 = 20;

/// A running output stream. Audio stops when this is dropped.
pub struct Playback {
    _stream: cpal::Stream,
    pub device: String,
    pub config: Config,
    pub format: SampleFormat,
    /// Frames per callback requested from the device, if any.
    pub buffer_frames: Option<u32>,
}

fn host(name: Option<&str>) -> anyhow::Result<cpal::Host> {
    let Some(name) = name else {
        return Ok(cpal::default_host());
    };
    let id = cpal::available_hosts()
        .into_iter()
        .find(|id| id.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let known: Vec<_> = cpal::available_hosts().iter().map(|h| h.name()).collect();
            anyhow!(
                "unknown audio host {name:?}; available: {}",
                known.join(", ")
            )
        })?;
    Ok(cpal::host_from_id(id)?)
}

/// Build a graph for the default output device and start playing it.
///
/// `make_graph` receives the device's configuration, since a graph depends on
/// the sample rate and channel count it will run at.
pub fn play(
    make_graph: impl FnOnce(&Config) -> anyhow::Result<Graph>,
    options: &Options,
) -> anyhow::Result<Playback> {
    let host = host(options.host.as_deref())?;
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device on host {}", host.id().name()))?;
    let device_name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "<unnamed>".to_owned());

    let supported = device
        .default_output_config()
        .context("querying default output config")?;
    let format = supported.sample_format();
    let buffer_frames = options.buffer_frames.or(match *supported.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            Some((supported.sample_rate() * DEFAULT_BUFFER_MS / 1000).clamp(min, max))
        }
        cpal::SupportedBufferSize::Unknown => None,
    });
    let mut stream_config: StreamConfig = supported.into();
    if let Some(frames) = buffer_frames {
        stream_config.buffer_size = cpal::BufferSize::Fixed(frames);
    }

    let config = Config {
        sample_rate: stream_config.sample_rate,
        max_frames: options.max_frames.unwrap_or(1024),
        out_channels: usize::from(stream_config.channels),
    };
    let engine = Engine::new(make_graph(&config)?, config)?;

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, stream_config, engine),
        SampleFormat::F64 => build::<f64>(&device, stream_config, engine),
        SampleFormat::I8 => build::<i8>(&device, stream_config, engine),
        SampleFormat::I16 => build::<i16>(&device, stream_config, engine),
        SampleFormat::I24 => build::<cpal::I24>(&device, stream_config, engine),
        SampleFormat::I32 => build::<i32>(&device, stream_config, engine),
        SampleFormat::U8 => build::<u8>(&device, stream_config, engine),
        SampleFormat::U16 => build::<u16>(&device, stream_config, engine),
        SampleFormat::U32 => build::<u32>(&device, stream_config, engine),
        other => bail!("unsupported device sample format {other}"),
    }?;
    stream.play().context("starting output stream")?;

    Ok(Playback {
        _stream: stream,
        device: device_name,
        config,
        format,
        buffer_frames,
    })
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut engine: Engine,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            engine.render_interleaved_with(data, T::from_sample);
        },
        // Runs off the audio thread, so printing is fine here.
        |err| eprintln!("rill: stream error: {err}"),
        None,
    )?;
    Ok(stream)
}

/// Names of the hosts and output devices cpal can see, for diagnostics.
pub fn list() -> anyhow::Result<Vec<(String, Vec<String>)>> {
    let mut hosts = Vec::new();
    for id in cpal::available_hosts() {
        let host = cpal::host_from_id(id)?;
        let default = host.default_output_device().and_then(|d| d.id().ok());
        let mut names = Vec::new();
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                let mut name = device
                    .description()
                    .map(|d| d.name().to_owned())
                    .unwrap_or_else(|_| "<unnamed>".to_owned());
                if default.is_some() && device.id().ok() == default {
                    name.push_str(" (default)");
                }
                names.push(name);
            }
        }
        hosts.push((id.name().to_owned(), names));
    }
    Ok(hosts)
}
