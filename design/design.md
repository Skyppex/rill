# Rill — Language Design Doc (v0.1)

Oct 6, 2026 · @Brage Rønne Ingebrigtsen

## Overview and goals

Rill is a statically typed, embeddable language for real-time audio where streams of samples are the core abstraction. It targets effects, virtual instruments and live audio apps on desktops, and the same scripts should run on microcontrollers with a primitive speaker.

Design principles:

- **Streams are first-class.** Every value inside a rill is the current value of a stream; time is implicit.
- **Block size is invisible.** User code never sees callback boundaries, so output is identical at 32 or 1024 frames per callback.
- **Expressive at build time, boring at run time.** High-level features resolve before audio starts; the callback runs flat loops with no allocation, locks or syscalls.
- **Hardware-agnostic numbers.** Programmers think in `sample`; the compiler picks the storage format per target.
- **Embeddable like Lua.** A small C/Rust API; the host owns the clock and the audio device.

## Core concepts

**Stream.** A sequence of values with one value per tick at a known rate. A constant is a stream that never changes, so any parameter accepts either.

**Rill.** A stream processor. Its body describes what happens on *one* tick; the compiler runs it across each block. By default a rill is 1:1: exactly one output frame per input frame, checked at compile time.

**`sample`.** A native numeric type the programmer treats as a real number nominally in \[-1.0, 1.0\]. Its storage (f32, i16, i24, i32 fixed-point, u8 on tiny targets) is chosen per target, and conversions happen at I/O. Arithmetic on `sample` behaves the same everywhere, within the precision of the target.

**Frame.** One `sample` per channel, written `[sample; N]`. Mono is `[sample; 1]`, 7.1 is `[sample; 8]`. N is part of the type and known at build time.

**State.** Variables declared with `state` inside a rill persist across ticks and callbacks. Only rills have state, so all memory is visible and preallocated.

**Rate.** Every stream carries a sample rate in its type. The default is the host rate; rate-changing rills declare their ratio.

## Syntax sketch

Two kinds of definitions: `fn` for pure functions on values, and `rill` for stream processors with optional state. A file contains only definitions; running it instantiates its entry rill, `main` unless another is named, and plays what that rill returns. All syntax here is provisional.

```
// Pure function on one value. Takes exactly what it declares.
fn abs(x: sample) -> sample {
    if x < 0 { -x } else { x }
}

// Stateful 1:1 rill. Body runs once per tick.
rill peak(x: sample, release: Time = 300ms) -> sample {
    state level: sample = 0
    let a = abs(x)
    level = if a > level { a } else { level * decay(release) }
    return level
}

// Composition with pipes.
rill meter(x: sample) -> sample {
    return x |> abs |> peak
}

// Generic over channel count. Only needed when a rill works ACROSS channels.
rill mix_down<N>(x: [sample; N]) -> [sample; 1] {
    return [sum(x) / N]
}

// Rate-changing rill: the ratio is part of the signature.
rill decimate(x: sample) -> sample @ rate / 2 { ... }

// Generator: no audio input.
rill sine(freq: Hz) -> sample {
    state phase: f32 = 0
    phase = wrap(phase + freq / RATE)
    return sin(phase * TAU)
}

// Entry point: its return value goes to the device. Parameters need defaults;
// they are the program's controls.
rill main(depth: Hz = 20Hz) -> sample {
    let lfo   = sine(0.5Hz) * depth + 440Hz
    let voice = sine(lfo) * 0.3
    return voice
}
```

Literals carry units: `440Hz`, `300ms`, `2s`, `+7st`, `+50cents`, `-6dB`, `3/2`. Units convert to samples using the host rate at build time.

## Type system

Types describe one tick's value; stream-ness comes from being inside a rill. Every value in a body is the current tick's. The core types for v0.1:

| Type | Meaning | Notes |
| --- | --- | --- |
| `sample` | One audio value, nominally \[-1.0, 1.0\] | Storage chosen per target |
| `[sample; N]` | A frame of N channels | N is a compile-time constant |
| `f32`, `i32`, `bool` | Plain values for control logic | Not converted at I/O |
| `Hz`, `Time` | Unit-carrying numbers | Convert to samples via the host rate |
| `Pitch`, `Interval` | Abstract musical values | Turned into `Hz` by a tuning (see Pitch) |
| `Gain` | A level change, written in `dB` | Any plain number is an amplitude factor (see Levels) |
| `fn(A, B) -> R` | A function value | See Functions as values |

Lifting rules:

1. A `fn` takes exactly what it declares and never lifts. It is called from bodies, where values are already per tick; to accept channels it says so with a size parameter (`fn f<N>(x: [sample; N])`). Built-in functions follow the same rule.
2. A rill taking `sample` can be applied to `[sample; N]`; it runs N independent copies, each with its own state. Every rill call is its own instance; `let` binds a value, so a bound result used twice is one instance.
3. A constant can be passed wherever a stream of the same type is expected.
4. Operators on frames are element-wise; `sum`, `max` and similar reduce across channels.

Rate rules:

1. Streams with different rates cannot be combined without an explicit conversion.
2. Every path through a 1:1 rill must `return` exactly once per tick; the compiler rejects anything else.
3. Any cycle in the graph must pass through a delay of at least one sample.

Optional channel layouts name positions without changing the type: `type Surround71 = [sample; 8] layout(L, R, C, LFE, Ls, Rs, Lb, Rb)` lets code write `x.lfe`.

## Pitch and tuning

A note name is an abstract `Pitch`, not a frequency; it becomes `Hz` only when it goes through a tuning. A tuning is an ordinary function from `Pitch` to `Hz`. Raw `Hz` literals bypass tuning entirely.

```
E4 |> equal(24, a4: 432Hz) |> sine      // quarter tones, A4 at 432Hz
E4 |> just(C) |> sine                   // just intonation on C
(E4 + 4st) |> equal |> sine             // Pitch + Interval -> Pitch
sum([E4, F#4, B4] |> equal |> sine)     // a chord: three voices, mixed

let tuning = fn(p: Pitch) -> Hz { pythagorean(p, D) }   // pick one, pass it around
```

The built-in tunings take the pitch first, so pipes read naturally:

```
equal(pitch: Pitch, steps: i32 = 12, a4: Hz = 440Hz) -> Hz
just(pitch: Pitch, root: Pitch, a4: Hz = 440Hz) -> Hz
pythagorean(pitch: Pitch, root: Pitch, a4: Hz = 440Hz) -> Hz
meantone(pitch: Pitch, root: Pitch, a4: Hz = 440Hz) -> Hz
```

Rules:

- A `Pitch` is a position, not an amount: `Pitch ± Interval` gives a `Pitch` (the interval always comes after) and `Pitch - Pitch` gives the `Interval` between them. Pitches can be compared, but not scaled or added together.
- `equal` places note names on the nearest of `steps` equal divisions of the octave. The scale tunings build twelve notes from ratios above their root and put A4 exactly on `a4`. In every tuning, fractions of a semitone (cents, bends) stay continuous.
- A tuning's settings are ordinary arguments, so they can change while playing (`a4` from a live control, say). Your own tuning is any `fn(Pitch) -> Hz`.
- Chords are frames of pitches, written with `[ ]` and commas, not `+`, because `+` already means addition on `Hz` and mixing on streams. The built-in tunings accept a chord and tune each pitch; passing the result to a rill that takes `Hz` lifts it to one voice per pitch.
- When every input is known at build time, tuning constant-folds to a number. Otherwise it is evaluated per tick.
- Note events carry `pitch` as a `Pitch` and `velocity` in 0–1. Velocity is linear; a curve such as `velocity * velocity` sounds more even, since a straight line is too loud at soft velocities.

## Levels

A level change is a `Gain`, written in decibels. A signal moves up or down in level with `+` and `-`, the way a pitch moves by an interval; the level always comes after the signal.

```
voice - 6dB                     // half the amplitude
voice + volume                  // volume: Gain, e.g. a live control
[l, r] - [0dB, 6dB]             // a level per channel
ramp(-60, 0, 4s) * 1dB          // a fade that moves evenly in level
level(peak(voice)) < -20dB      // level of an amplitude, for meters and dynamics
```

Rules:

- `x ± Gain` scales `x` by `10^(±dB/20)`, per channel on frames. `Gain ± x` is an error (the level comes after), and so is multiplying or dividing a signal (`sample`) by a `Gain`: levels are added or subtracted.
- Levels combine with `+`/`-`, scale with `*`/`/` by plain numbers (half of `-6dB` is `-3dB`), and compare with each other. `Gain / Gain` is a plain ratio.
- A `Gain` is stored as an amplitude factor (`-6dB` is about 0.5), so any plain number can be passed where a `Gain` is expected: `0.5` means about -6dB. Hosts send `Gain` controls the same way. Inside an expression nothing converts: `voice - 0.5` is ordinary subtraction.
- `level(x)` is the level of an amplitude (`level(1)` is 0dB; silence is held at -120dB instead of -inf). `amp(g)` is the amplitude factor of a level.

## Functions as values

Fns are values: they can be bound with `let`, passed to fns and rills, returned from fns, and chosen while playing. Anonymous fns use the named syntax without the name.

```
fn a432(p: Pitch) -> Hz { 432Hz * pow(2, (p - A4) / 12st) }

rill voice(pitch: Pitch, tune: fn(Pitch) -> Hz) -> sample {
    return pitch |> tune |> sine
}

voice(E4, a432)                         // a named fn
voice(E4, fn(p) { equal(p, 24) })       // anonymous; types come from `tune`
voice(E4, equal)                        // extra parameters with defaults are fine
voice(E4, if minor { just_c } else { equal })   // chosen while playing
```

Rules:

- Function types are written `fn(A, B) -> R`. They cannot be `state`, frame channels, event fields or the entry rill's parameters.
- An anonymous fn's parameter and return types can be left out when the surroundings say what they are (a parameter of function type, an annotated `let`, a `return`, or a fn's declared return type). Otherwise they are required.
- Anonymous fns capture what they use, by value, at the point they are made. They are fns, so they are pure: they read what they capture but cannot change it, and cannot call rills.
- A named fn can be used where a function type with fewer parameters is expected, if its remaining parameters have defaults. A built-in that works on several types (`sin`) needs an expected type to pick one.
- Rills are not values: they carry state, and whether an unchosen rill keeps running is not decided yet.
- Recursion is still not allowed, including through function values.
- At build time every call of a function value is replaced by the function's body, so function values cost nothing while playing. A function chosen while playing becomes a branch between the candidates; only the chosen one runs.

## Execution model

A Rill program runs in two stages: a build stage that may do anything, and a run stage that only executes a frozen graph inside the audio callback.

1. **Parse and type-check** the source.
2. **Build stage** (any thread): instantiate the entry rill with its parameter defaults, resolve pitches and units, inline every rill call (each call site gets its own state), expand chords and channel lifting, and produce a graph of nodes.
3. **Optimize**: constant folding, specialization for constant parameters, control-rate inference, dead-node removal, buffer reuse via liveness analysis.
4. **Freeze**: allocate every buffer and every `state` variable in one arena; topologically sort nodes.
5. **Run stage** (audio thread): on each callback, pop due events, split the block at their sample offsets, run each node over its slice in order, write the final node into the host's output buffer.

Run-stage rules: no allocation, no locks, no syscalls, no unbounded loops; denormals flushed to zero. Control input arrives through a lock-free single-producer/single-consumer queue, and parameter changes are smoothed over a few milliseconds.

Per-callback inputs the runtime receives from the host: input buffer, output buffer, frame count (up to a declared maximum), channel counts, sample rate, frame position, and timestamped events. Buffers are planar internally; interleaved host buffers are converted at the boundary.

Hot reload: build the new graph off the audio thread, swap it in with an atomic pointer, crossfade for a few milliseconds, and free the old graph off the audio thread.

Backends, in order of priority: per-block bytecode interpreter (first), compile-to-C for embedded (later), JIT via Cranelift (later).

## Embedding API

The host owns the audio device and the clock; Rill only fills buffers. A first sketch of the C API:

```c
typedef struct {
    uint32_t sample_rate;
    uint32_t max_frames;      // largest block the host will ever request
    uint16_t in_channels;
    uint16_t out_channels;
    rill_format format;       // F32, I16, I24, I32, U8 ...
} rill_config;

rill_engine* rill_create(const rill_config* cfg);
rill_status  rill_load(rill_engine*, const char* src);     // build stage
void         rill_render(rill_engine*,                      // run stage, real-time safe
                         const void* const* in, void* const* out,
                         uint32_t frames);
void         rill_set_param(rill_engine*, const char* name, float value);  // a parameter of the entry rill
void         rill_send_event(rill_engine*, uint32_t frame_offset, rill_event ev);
void         rill_destroy(rill_engine*);
```

On microcontrollers the host calls `rill_render` from the DMA half-complete and complete interrupts. Memory use is fixed after `rill_load` and can be reported so the host can size its arena.

## Non-goals and open questions

Not in v0.1: plugin formats (CLAP comes later), FFT and other block-level rills, variable-ratio resampling, polyphonic voice management, a JIT, and a fixed-point backend.

Open questions:

- **Headroom on integer targets.** `f32` can exceed 1.0 mid-graph and come back down; fixed-point clips. Options: guard bits in the internal format, or saturating arithmetic with a compiler warning.
- **Feedback in block processing.** Process cycles sample by sample, or fuse each cycle into one node that loops internally?
- **Sequencing.** How notes and events over time fit beside streams: event streams, a scheduler API, or ChucK-style `=> now`.
- **Block-level escape hatch.** Syntax for rills that need a whole buffer, such as FFT effects, and how they report latency.
- ~~**Sharing semantics.**~~ Settled: `let` binds a value, so a bound stream used twice is one instance; every call is a new instance.
- **Time stretching.** Whether variable-rate rills are allowed on live input or only on stored buffers.

## Milestones

The goal of the first milestone is hearing a sine wave from Rill source in real time; everything else waits until that works.

- [x] **M1 — Hello, sine.** Hard-coded graph runtime in Rust with `cpal`: a sine node, a gain node, output to the device. Plus a fake-callback loop that renders to WAV for tests.
- [x] **M2 — Parser and checker.** Parse `fn`, `rill`, `state`, `let`, `|>`, unit literals; type-check `sample` and `[sample; N]`.
- [x] **M3 — Graph builder.** Top-level code builds a graph; per-block interpreter runs it. Lifting over channels works.
- [x] **M4 — Live control.** `rill_set_param`, parameter smoothing, sample-accurate events with block splitting.
- [x] **M5 — Pitch layer.** `Pitch`, `Interval`, chord literals, 12-TET and just tuning, constant folding.
- [ ] **M6 — Sequencing.** Time signatures, programmable sequences.
- [ ] **M7 — Hot reload.** Swap graphs with a crossfade while audio plays.
- [ ] **M100 — Embedded spike.** Run a fixed patch on a microcontroller through the C API.
