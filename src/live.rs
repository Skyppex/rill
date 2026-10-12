//! Replacing a running program with a new build while it plays.
//!
//! A [`Live`] handle (on any thread but the audio one) builds programs and
//! hands them over; the [`Player`] (on the audio thread) takes each at the
//! start of a block, carries the running program's state into it, and
//! crossfades from the old program to the new. Nothing on the audio thread
//! allocates, frees, locks or waits: programs travel through two slots, one
//! each way, and old programs go back to be dropped by the [`Live`] side.
//!
//! Two things ask for a new build: an edited source ([`Live::reload`]) and a
//! value going past its `max` while playing ([`Live::grow`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::engine::{Config, Engine, EngineKeys, TakeoverReport};
use crate::event::Dispatch;
use crate::lang::{self, Diagnostic, SourceMap, Sources};

/// Where programs pass between the two sides.
struct Exchange {
    /// A new engine for the audio thread to take, or null.
    incoming: AtomicPtr<Engine>,
    /// An engine the audio thread is done with, to drop elsewhere, or null.
    outgoing: AtomicPtr<Engine>,
    /// Crossfade length in frames.
    fade: AtomicUsize,
}

impl Exchange {
    fn put(slot: &AtomicPtr<Engine>, engine: Box<Engine>) -> Option<Box<Engine>> {
        let old = slot.swap(Box::into_raw(engine), Ordering::AcqRel);
        // SAFETY: only `Box::into_raw` pointers are put in the slots, and a
        // pointer is taken out by exactly one swap.
        (!old.is_null()).then(|| unsafe { Box::from_raw(old) })
    }

    fn take(slot: &AtomicPtr<Engine>) -> Option<Box<Engine>> {
        let p = slot.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: as in `put`.
        (!p.is_null()).then(|| unsafe { Box::from_raw(p) })
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        drop(Exchange::take(&self.incoming));
        drop(Exchange::take(&self.outgoing));
    }
}

/// The audio side: renders the running engine, swapping in new builds at
/// block boundaries with a crossfade.
pub struct Player {
    exchange: Arc<Exchange>,
    current: Box<Engine>,
    /// The engine being faded out, and how far the fade has got.
    fading: Option<(Box<Engine>, usize)>,
    /// An engine done with, waiting for the outgoing slot to be free.
    retired: Option<Box<Engine>>,
    /// Scratch for one block of each engine, planar.
    new_out: Vec<Box<[f32]>>,
    old_out: Vec<Box<[f32]>>,
}

impl Player {
    /// Play `engine` with no [`Live`] side: it never changes.
    pub fn new(engine: Engine) -> Player {
        let exchange = Arc::new(Exchange {
            incoming: AtomicPtr::new(ptr::null_mut()),
            outgoing: AtomicPtr::new(ptr::null_mut()),
            fade: AtomicUsize::new(0),
        });
        Player::with(exchange, engine)
    }

    fn with(exchange: Arc<Exchange>, engine: Engine) -> Player {
        let config = engine.config();
        let scratch = || {
            (0..config.out_channels)
                .map(|_| vec![0.0; config.max_frames].into_boxed_slice())
                .collect()
        };
        Player {
            exchange,
            current: Box::new(engine),
            fading: None,
            retired: None,
            new_out: scratch(),
            old_out: scratch(),
        }
    }

    pub fn config(&self) -> Config {
        self.current.config()
    }

    /// Samples played since the start, across swaps.
    pub fn position(&self) -> u64 {
        self.current.position()
    }

    /// The running engine.
    pub fn engine(&self) -> &Engine {
        &self.current
    }

    /// Set a live control in the running program (and the one fading out).
    pub fn set_param(&mut self, name: &str, value: f32) -> bool {
        if let Some((old, _)) = &mut self.fading {
            old.set_param(name, value);
        }
        self.current.set_param(name, value)
    }

    /// Send an event to the running program (and the one fading out).
    pub fn dispatch(&mut self, dispatch: &Dispatch) -> bool {
        if let Some((old, _)) = &mut self.fading {
            // Ids are per build; an event by id only goes to the old build
            // if it has the same event there.
            let same = match dispatch {
                Dispatch::Incoming(_) => true,
                Dispatch::To(id, _) => {
                    let i = usize::from(id.0);
                    let a = old.events().get(i).map(|d| d.name.as_str());
                    let b = self.current.events().get(i).map(|d| d.name.as_str());
                    a == b
                }
            };
            if same {
                old.dispatch(dispatch);
            }
        }
        self.current.dispatch(dispatch)
    }

    /// At a block boundary: hand back an engine done with, and take a new
    /// one if one is waiting and no fade is under way.
    fn swap(&mut self) {
        if let Some(old) = self.retired.take() {
            self.retire(old);
        }
        if self.fading.is_some() || self.retired.is_some() {
            return;
        }
        let Some(mut new) = Exchange::take(&self.exchange.incoming) else {
            return;
        };
        new.take_over(&self.current);
        let old = std::mem::replace(&mut self.current, new);
        if self.exchange.fade.load(Ordering::Relaxed) == 0 {
            self.retire(old);
        } else {
            self.fading = Some((old, 0));
        }
    }

    fn retire(&mut self, old: Box<Engine>) {
        let p = Box::into_raw(old);
        match self.exchange.outgoing.compare_exchange(
            ptr::null_mut(),
            p,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            // The other side has not collected the last one yet: keep this
            // one until it has.
            // SAFETY: `p` came from `Box::into_raw` just above.
            Err(_) => self.retired = Some(unsafe { Box::from_raw(p) }),
        }
    }

    /// Render into planar output, one slice per channel.
    pub fn render_planar(&mut self, out: &mut [&mut [f32]]) {
        let frames = out.first().map_or(0, |c| c.len());
        let block = self.current.config().max_frames;
        let mut done = 0;
        while done < frames {
            self.swap();
            let n = (frames - done).min(block);
            self.render_block(n);
            for (c, channel) in out.iter_mut().enumerate() {
                channel[done..done + n].copy_from_slice(&self.new_out[c][..n]);
            }
            done += n;
        }
    }

    /// Render `n` frames (at most a block) into `new_out`, crossfading from
    /// the engine fading out if there is one.
    fn render_block(&mut self, n: usize) {
        render_into(&mut self.current, &mut self.new_out, n);
        let Some((old, at)) = &mut self.fading else {
            return;
        };
        render_into(old, &mut self.old_out, n);
        let total = self.exchange.fade.load(Ordering::Relaxed).max(1);
        for i in 0..n {
            let t = ((*at + i) as f32 / total as f32).min(1.0);
            // Half a cosine: the gains always add up to 1.
            let g = 0.5 - 0.5 * (t * std::f32::consts::PI).cos();
            for (new, old) in self.new_out.iter_mut().zip(&self.old_out) {
                new[i] = old[i] + (new[i] - old[i]) * g;
            }
        }
        *at += n;
        if *at >= total {
            let (old, _) = self.fading.take().expect("fading");
            self.retire(old);
        }
    }

    /// Render into an interleaved buffer, converting to `T`. Trailing samples
    /// that do not make up a whole frame are zeroed.
    pub fn render_interleaved_with<T: Copy>(&mut self, out: &mut [T], convert: impl Fn(f32) -> T) {
        let channels = self.current.config().out_channels;
        let frames = out.len() / channels;
        out[frames * channels..].fill(convert(0.0));
        let block = self.current.config().max_frames;
        let mut done = 0;
        while done < frames {
            self.swap();
            let n = (frames - done).min(block);
            self.render_block(n);
            for i in 0..n {
                for c in 0..channels {
                    out[(done + i) * channels + c] = convert(self.new_out[c][i]);
                }
            }
            done += n;
        }
    }

    /// [`Player::render_interleaved_with`] for `f32`.
    pub fn render_interleaved(&mut self, out: &mut [f32]) {
        self.render_interleaved_with(out, |x| x);
    }
}

/// The most output channels a [`Player`] renders: its scratch is on the
/// stack, so the audio thread does not allocate.
pub const MAX_CHANNELS: usize = 32;

/// Render `n` frames of `engine` into the planar `out`.
fn render_into(engine: &mut Engine, out: &mut [Box<[f32]>], n: usize) {
    let mut slices: [&mut [f32]; MAX_CHANNELS] = Default::default();
    let channels = out.len().min(MAX_CHANNELS);
    for (s, o) in slices.iter_mut().zip(out.iter_mut()) {
        *s = &mut o[..n];
    }
    engine.render_planar(&mut slices[..channels]);
}

/// Where a program comes from, so it can be built again.
enum Origin {
    File(PathBuf),
    Text(String),
}

/// What a successful rebuild did.
#[derive(Debug)]
pub struct Reloaded {
    /// Shows the program's diagnostics, whichever file they are in.
    pub sources: SourceMap,
    pub warnings: Vec<Diagnostic>,
    /// State carried over, and state new to this build.
    pub report: TakeoverReport,
    /// Whether the program uses `random()`, so its seed is worth showing.
    pub random: bool,
}

/// A rebuild that failed: the running program keeps playing.
#[derive(Debug)]
pub struct Failed {
    pub sources: SourceMap,
    pub errors: Vec<Diagnostic>,
}

/// The control side: builds programs and hands them to the [`Player`].
pub struct Live {
    exchange: Arc<Exchange>,
    origin: Origin,
    config: Config,
    entry: String,
    options: lang::build::Options,
    /// The running program's keys, to plan the next takeover.
    keys: EngineKeys,
    /// The files the running program was built from.
    files: Vec<PathBuf>,
    /// Ceilings raised while playing, by state key (see [`Live::grow`]).
    raised: HashMap<String, f32>,
    /// The running program's ceilings, and what it has asked of them.
    ceilings: Option<lang::vm::Ceilings>,
    /// When a ceiling last grew, to grow at most once per [`GROWTH_GAP`].
    grown: Option<Instant>,
}

/// The shortest time between two rebuilds for growing ceilings, so a value
/// sweeping upwards does not keep rebuilding.
pub const GROWTH_GAP: Duration = Duration::from_millis(50);

/// How much room a ceiling gets when it grows: this many times what was
/// asked of it.
pub const GROWTH: f32 = 1.5;

impl Live {
    /// Build the program starting at the file `path` and get it ready to
    /// play: the [`Live`] side to rebuild it, and the [`Player`] for the
    /// audio thread. `options` should have a seed, so rebuilds pick the
    /// same numbers; one is picked if not.
    pub fn from_file(
        path: &Path,
        config: Config,
        entry: &str,
        options: lang::build::Options,
    ) -> Result<(Live, Player, Reloaded), Failed> {
        Live::start(Origin::File(path.to_owned()), config, entry, options)
    }

    /// [`Live::from_file`] for a program in a string.
    pub fn from_text(
        src: &str,
        config: Config,
        entry: &str,
        options: lang::build::Options,
    ) -> Result<(Live, Player, Reloaded), Failed> {
        Live::start(Origin::Text(src.to_owned()), config, entry, options)
    }

    fn start(
        origin: Origin,
        config: Config,
        entry: &str,
        mut options: lang::build::Options,
    ) -> Result<(Live, Player, Reloaded), Failed> {
        options.seed.get_or_insert_with(lang::build::new_seed);
        let mut live = Live {
            exchange: Arc::new(Exchange {
                incoming: AtomicPtr::new(ptr::null_mut()),
                outgoing: AtomicPtr::new(ptr::null_mut()),
                fade: AtomicUsize::new((config.sample_rate as usize) / 50),
            }),
            origin,
            config,
            entry: entry.to_owned(),
            options,
            keys: EngineKeys::default(),
            files: Vec::new(),
            raised: HashMap::new(),
            ceilings: None,
            grown: None,
        };
        let (engine, reloaded) = live.build()?;
        live.keys = engine.keys();
        live.ceilings = engine.ceilings();
        let player = Player::with(live.exchange.clone(), engine);
        Ok((live, player, reloaded))
    }

    /// Build the program as its sources are now, planned to take over from
    /// the running one.
    fn build(&mut self) -> Result<(Engine, Reloaded), Failed> {
        let (sources, result) = match &self.origin {
            Origin::File(path) => {
                let (sources, result) =
                    lang::load_file(path, &lang::Disk, &self.config, &self.entry, &self.options);
                let result = result.map(|(graph, checked)| {
                    let random = checked.uses_prelude("random");
                    (graph, checked.warnings, random)
                });
                (sources, result)
            }
            Origin::Text(src) => (
                SourceMap::single("", src.as_str()),
                lang::load_with(src, &self.config, &self.entry, &self.options)
                    .map(|(graph, warnings)| (graph, warnings, false)),
            ),
        };
        let (graph, warnings, random) = match result {
            Ok(built) => built,
            Err(errors) => return Err(Failed { sources, errors }),
        };
        let mut engine = Engine::new(graph, self.config).map_err(|e| Failed {
            sources: sources.clone(),
            errors: vec![Diagnostic::error(lang::Span::default(), e.to_string())],
        })?;
        let report = engine.plan_takeover(&self.keys);
        self.files = sources
            .files
            .iter()
            .filter_map(|f| f.path.clone())
            .collect();
        Ok((
            engine,
            Reloaded {
                sources,
                warnings,
                report,
                random,
            },
        ))
    }

    /// Build the program again from its sources and hand it to the
    /// [`Player`], which crossfades to it. A build with errors changes
    /// nothing: the running program keeps playing.
    pub fn reload(&mut self) -> Result<Reloaded, Failed> {
        self.collect();
        let (engine, reloaded) = self.build()?;
        self.keys = engine.keys();
        self.ceilings = engine.ceilings();
        if let Some(waiting) = Exchange::put(&self.exchange.incoming, Box::new(engine)) {
            // An earlier build the player had not taken yet: this replaces it.
            drop(waiting);
        }
        Ok(reloaded)
    }

    /// If a value went past its ceiling while playing (a filter's slope past
    /// its `max`, a delay's time past its line), rebuild with room for
    /// [`GROWTH`] times as much there, and hand it over. Ceilings never
    /// shrink, and this rebuilds at most once per [`GROWTH_GAP`]. Returns
    /// `None` when nothing needed to grow (or it is too soon). Call it every
    /// few milliseconds.
    pub fn grow(&mut self) -> Option<Result<Reloaded, Failed>> {
        self.collect();
        if self.grown.is_some_and(|t| t.elapsed() < GROWTH_GAP) {
            return None;
        }
        let (list, wants) = self.ceilings.as_ref()?;
        let mut grew = false;
        for ((key, value), want) in list.iter().zip(wants.iter()) {
            let asked = f32::from_bits(want.load(Ordering::Relaxed));
            if asked > *value {
                let room = self.raised.entry(key.clone()).or_insert(0.0);
                if asked * GROWTH > *room {
                    *room = asked * GROWTH;
                    grew = true;
                }
            }
        }
        if !grew {
            return None;
        }
        self.grown = Some(Instant::now());
        self.options.ceilings = self.raised.clone();
        Some(self.reload())
    }

    /// Ceilings raised so far, by state key, with how big they are now.
    pub fn raised(&self) -> &HashMap<String, f32> {
        &self.raised
    }

    /// Replace the program's text (for a program from a string) and reload.
    pub fn reload_text(&mut self, src: &str) -> Result<Reloaded, Failed> {
        self.origin = Origin::Text(src.to_owned());
        self.reload()
    }

    /// Drop engines the player is done with. Cheap; call it now and then.
    pub fn collect(&self) {
        drop(Exchange::take(&self.exchange.outgoing));
    }

    /// The files the running program was built from, to watch for changes.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// How long the crossfade to a new build lasts, in frames. 0 switches at
    /// the next block boundary.
    pub fn set_fade(&self, frames: usize) {
        self.exchange.fade.store(frames, Ordering::Relaxed);
    }

    /// Whether a source of the program is the file `path`.
    pub fn uses(&self, path: &Path) -> bool {
        let canonical = |p: &Path| lang::Disk.canonical(p);
        let wanted = canonical(path);
        self.files.iter().any(|f| canonical(f) == wanted)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.collect();
    }
}
