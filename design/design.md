# Rill — Language Design Doc (v0.1)

Oct 6, 2026 · @Brage Rønne Ingebrigtsen

## Overview and goals

Rill is a statically typed, embeddable language for real-time audio where streams of samples are the core abstraction. It targets effects, virtual instruments and live audio apps on desktops, and the same scripts should run on microcontrollers with a primitive speaker.

Design principles:

- **Streams are first-class.** Every value inside a rill is the current value of a stream; time is implicit.
- **Block size is invisible.** User code never sees callback boundaries, so output is identical at 32 or 1024 frames per callback.
- **Expressive at build time, boring at run time.** High-level features resolve before audio starts; the callback runs flat loops with no allocation, locks or syscalls.
- **Hardware-agnostic numbers.** Programmers think in `Sample`; the compiler picks the storage format per target.
- **Embeddable like Lua.** A small C/Rust API; the host owns the clock and the audio device.

## Core concepts

**Stream.** A sequence of values with one value per tick at a known rate. A constant is a stream that never changes, so any parameter accepts either.

**Rill.** A stream processor. Its body describes what happens on *one* tick; the compiler runs it across each block. By default a rill is 1:1: exactly one output frame per input frame, checked at compile time.

**`Sample`.** A native numeric type the programmer treats as a real number nominally in \[-1.0, 1.0\]. Its storage (f32, i16, i24, i32 fixed-point, u8 on tiny targets) is chosen per target, and conversions happen at I/O. Arithmetic on `Sample` behaves the same everywhere, within the precision of the target.

**Frame.** One `Sample` per channel, written `[Sample; N]`. Mono is `[Sample; 1]`, 7.1 is `[Sample; 8]`. N is part of the type and known at build time.

**State.** Variables declared with `state` inside a rill persist across ticks and callbacks. Only rills have state, so all memory is visible and preallocated.

**Rate.** Every stream carries a sample rate in its type. The default is the host rate; rate-changing rills declare their ratio.

## Syntax sketch

Two kinds of definitions: `fn` for pure functions on values, and `rill` for stream processors with optional state. A file contains only definitions; running it instantiates its entry rill, `main` unless another is named, and plays what that rill returns. All syntax here is provisional.

```rill
// Pure function on one value. Takes exactly what it declares.
fn abs(x: Sample) Sample {
    if x < 0 { -x } else { x }
}

// Stateful 1:1 rill. Body runs once per tick.
rill peak(x: Sample, release: Time = 300ms) Sample {
    state level: Sample = 0
    let a = abs(x)
    level = if a > level { a } else { level * decay(release) }
    return level
}

// Composition with pipes.
rill meter(x: Sample) Sample {
    return x |> abs |> peak
}

// Generic over channel count. Only needed when a rill works ACROSS channels.
rill mix_down<N>(x: [Sample; N]) [Sample; 1] {
    return [sum(x) / N]
}

// Rate-changing rill: the ratio is part of the signature.
rill decimate(x: Sample) Sample @ rate / 2 { ... }

// Generator: no audio input.
rill sine(freq: Freq) Sample {
    state phase: Float = 0
    phase = wrap(phase + freq / RATE)
    return sin(phase * TAU)
}

// Entry point: its return value goes to the device. Parameters need defaults;
// they are the program's controls.
rill main(depth: Freq = 20Hz) Sample {
    let lfo   = sine(0.5Hz) * depth + 440Hz
    let voice = sine(lfo) * 0.3
    return voice
}
```

Literals carry units: `440Hz`, `120bpm`, `300ms`, `2s`, `+7st`, `+50cents`, `-6dB`, `3/2`. A tempo is a frequency, so `120bpm` is `2Hz`. Units convert to samples using the host rate at build time.

## Type system

Types describe one tick's value; stream-ness comes from being inside a rill. Every value in a body is the current tick's. The core types for v0.1:

| Type | Meaning | Notes |
| --- | --- | --- |
| `Sample` | One audio value, nominally \[-1.0, 1.0\] | Storage chosen per target |
| `[Sample; N]` | A frame of N channels | N is a compile-time constant: a whole number, a size parameter, or a constant expression such as `riff.step_count * 2` |
| `[[Sample; 2]; N]` | Frames nest: N stereo voices or buses | See Polyphony |
| `Float`, `Int`, `Bool` | Plain values for control logic | Not converted at I/O |
| `Freq`, `Time` | Unit-carrying numbers | Convert to samples via the host rate |
| `Pitch`, `Interval` | Abstract musical values | Turned into `Freq` by a tuning (see Pitch) |
| `Gain` | A level change, written in `dB` | Any plain number is an amplitude factor (see Levels) |
| `fn(A, B) R` | A function value | See Functions as values |

Plain numbers convert with `as`: `x as Float`, `x as Sample`, `x as Int` (which truncates toward zero). `as` binds tighter than arithmetic, so `a + b as Int` casts only `b`. Units and levels never disappear by a cast: `freq / 1Hz` gives a plain number, and `amp(g)` the factor of a level.

Lifting rules:

1. A `fn` takes exactly what it declares and never lifts. It is called from bodies, where values are already per tick; to accept channels it says so with a size parameter (`fn f<N>(x: [Sample; N])`). Built-in functions follow the same rule.
2. A rill applied to an argument with more frame layers than its parameter runs once per element of the extra layers, each copy with its own state, and wraps its result in those layers. A rill taking `Sample` applied to `[Sample; N]` runs N copies; one taking `[Sample; 2]` applied to `[[Sample; 2]; 4]` runs four; `pan(x: Sample) [Sample; 2]` applied to `[Sample; 3]` gives `[[Sample; 2]; 3]`. The copies follow the argument with the most extra layers; another lifting argument with fewer lines up with its outer or inner end (rule 6), and the rest are shared by every copy. Every rill call is its own instance; `let` binds a value, so a bound result used twice is one instance.
3. A constant can be passed wherever a stream of the same type is expected.
4. Operators on frames are element-wise. A single value applies to every channel, and a shorter frame lines up with the end of the other side it matches (rule 6): `buses * [0.5, 1, 1, 0.2]` gives each of four stereo buses its own gain, and `buses * [1, 0.5]` turns the right channel of every bus down.
5. `sum`, `min` and `max` reduce the outer layer: `sum` of `[[Sample; 2]; 3]` is a stereo `[Sample; 2]`.
6. A shorter shape lines up with the **outer** layers of a longer one if it matches them, or with the **inner** layers if it matches those, and is reused across the rest. With 5 notes of 8 voices each (`[[Freq; 8]; 5]`), a `[Float; 5]` gives one value per note and a `[Float; 8]` one per voice, the same for every note. A shape matching both ends (`[Float; 8]` with `[[Sample; 8]; 8]`) is an error rather than a guess: `[x; 8]` says "the same for every outer element". A shape matching neither is an error.
7. `each` before an argument (`f(each expr)`, `f(name: each expr)`) evaluates it once per copy of a lifted rill, across every lifted layer, instead of once for all of them. It matters for anything whose evaluations differ: `saw(offset: each random())` gives every copy its own offset, and `each lfo(0.3Hz)` its own LFO with its own state. The value has the parameter's own type and is not lifted itself; `each` on a call that does not lift is an error. `each` is a keyword only there.

Randomness: `random()` is a `Float` in [0, 1), and `random(lo: S, hi: S)` any number type in [`lo`, `hi`) (levels evenly in dB). Every evaluation picks its number when the program is built, so it is a constant while playing: `random()` written once in a rill body is one number for the whole run, not a new one every sample. The numbers come from a seed; the same program with the same seed picks the same numbers. Without `--seed`, every run picks a new seed and prints it, so a run worth keeping can be repeated. Noise that changes every sample is a separate, later feature.

Rate rules:

1. Streams with different rates cannot be combined without an explicit conversion.
2. Every path through a 1:1 rill must `return` exactly once per tick; the compiler rejects anything else.
3. Any cycle in the graph must pass through a delay of at least one sample.

### Polyphony

Lifting makes polyphony the default. A chord is a frame of pitches, so it becomes one voice per note, and a stereo voice becomes a frame of stereo frames:

```rill
rill pan(x: Sample, pos: Float) [Sample; 2] {
    return [x * (1 - pos), x * pos]
}

rill main() [Sample; 2] {
    return [C4, E4, G4] |> equal |> sine |> pan(0.3) |> sum
    //     [Pitch; 3]     [Freq; 3]  [Sample; 3]  [[Sample; 2]; 3]  [Sample; 2]
}
```

Per-voice settings are frames of the outer size: `pan([0.2, 0.5, 0.8])` in place of `pan(0.3)` places each voice, and `+ [0dB, -3dB, -6dB]` sets each voice's level. Nesting costs nothing while playing: sizes are known at build time, so `[[Sample; 2]; 4]` is eight values. The entry rill's output and parameters stay flat.

Optional channel layouts name positions without changing the type: `type Surround71 = [Sample; 8] layout(L, R, C, LFE, Ls, Rs, Lb, Rb)` lets code write `x.lfe`.

## Pitch and tuning

A note name is an abstract `Pitch`, not a frequency; it becomes `Freq` only when it goes through a tuning. A tuning is an ordinary function from `Pitch` to `Freq`. Raw `Hz` literals bypass tuning entirely.

```rill
E4 |> equal(24, a4: 432Hz) |> sine      // quarter tones, A4 at 432Hz
E4 |> just(C) |> sine                   // just intonation on C
(E4 + 4st) |> equal |> sine             // Pitch + Interval -> Pitch
sum([E4, F#4, B4] |> equal |> sine)     // a chord: three voices, mixed

let tuning = fn(p: Pitch) Freq { pythagorean(p, D) }   // pick one, pass it around
```

The built-in tunings take the pitch first, so pipes read naturally:

```rill
equal(pitch: Pitch, steps: Int = 12, a4: Freq = 440Hz) Freq
just(pitch: Pitch, root: Pitch, a4: Freq = 440Hz) Freq
pythagorean(pitch: Pitch, root: Pitch, a4: Freq = 440Hz) Freq
meantone(pitch: Pitch, root: Pitch, a4: Freq = 440Hz) Freq
```

Rules:

- A `Pitch` is a position, not an amount: `Pitch ± Interval` gives a `Pitch` (the interval always comes after) and `Pitch - Pitch` gives the `Interval` between them. Pitches can be compared, but not scaled or added together.
- `equal` places note names on the nearest of `steps` equal divisions of the octave. The scale tunings build twelve notes from ratios above their root and put A4 exactly on `a4`. In every tuning, fractions of a semitone (cents, bends) stay continuous.
- A tuning's settings are ordinary arguments, so they can change while playing (`a4` from a live control, say). Your own tuning is any `fn(Pitch) Freq`.
- Chords are frames of pitches, written with `[ ]` and commas, not `+`, because `+` already means addition on `Freq` and mixing on streams. The built-in tunings accept a chord and tune each pitch; passing the result to a rill that takes `Freq` lifts it to one voice per pitch.
- When every input is known at build time, tuning constant-folds to a number. Otherwise it is evaluated per tick.
- Note events carry `pitch` as a `Pitch` and `velocity` (or `release`) in 0–1 (see Events). Velocity is linear; a curve such as `velocity * velocity` sounds more even, since a straight line is too loud at soft velocities.

## Levels

A level change is a `Gain`, written in decibels. A signal moves up or down in level with `+` and `-`, the way a pitch moves by an interval; the level always comes after the signal.

```rill
voice - 6dB                     // half the amplitude
voice + volume                  // volume: Gain, e.g. a live control
[l, r] - [0dB, 6dB]             // a level per channel
ramp(-60, 0, 4s) * 1dB          // a fade that moves evenly in level
level(peak(voice)) < -20dB      // level of an amplitude, for meters and dynamics
```

Rules:

- `x ± Gain` scales `x` by `10^(±dB/20)`, per channel on frames. `Gain ± x` is an error (the level comes after), and so is multiplying or dividing a signal (`Sample`) by a `Gain`: levels are added or subtracted.
- Levels combine with `+`/`-`, scale with `*`/`/` by plain numbers (half of `-6dB` is `-3dB`), and compare with each other. `Gain / Gain` is a plain ratio.
- A `Gain` is stored as an amplitude factor (`-6dB` is about 0.5), so any plain number can be passed where a `Gain` is expected: `0.5` means about -6dB. Hosts send `Gain` controls the same way. Inside an expression nothing converts: `voice - 0.5` is ordinary subtraction.
- `level(x)` is the level of an amplitude (`level(1)` is 0dB; silence is held at -120dB instead of -inf). `amp(g)` is the amplitude factor of a level.

## Events

Events come from outside the program (a keyboard, a plugin host, a test) or from its own sequences. Every incoming event has a **kind**, a `sender`, a `channel` and a payload. What senders and channels mean is up to whoever sends the event; Rill only compares the numbers. Nothing is tied to MIDI: a MIDI host would use a number per device as the sender, and put control change 11 on channel 11.

A program declares the events it handles, once, at the top level: a name, a kind, and optional filters. Handlers name the declaration, so the routing lives in one place and the code that reacts never sees sender or channel numbers.

```rill
event keys_press note_on(sender: 5, channel: 1)
event keys_release note_off(sender: 5, channel: 1)
event expression control_change(sender: 5, channel: 11)
event melody note_on                    // no filters: every note_on

rill synth() Sample {
    state pitch: Pitch = A4
    state level: Float = 0
    state bright: Float = 0

    on keys_press(note) { pitch = note.pitch; level = note.velocity }
    on keys_release(note) { level = level * (1 - note.release) }
    on expression(value) { bright = value }
    ...
}
```

| Kind | Handler receives | Fields |
| --- | --- | --- |
| `note_on` | `NoteOn` | `pitch: Pitch`, `velocity: Float` (0–1), `instance: Int` |
| `note_off` | `NoteOff` | `pitch: Pitch`, `release: Float` (0–1), `instance: Int` |
| `control_change` | `Float` | the value, as sent |

Rules:

- The filters are `sender` and `channel`, whole-number constants; a sender can also be a sequence's name (see Sequences). A filter left out matches anything.
- `instance` is the id of the sequence instance that played a note, and 0 for notes from the host.
- An incoming event runs the handlers of every declaration it matches, in declaration order. A host can also send straight to a declaration by name, skipping its filters; tests and the sequencer do that.
- `on NAME(param)` takes at most one parameter, typed by the event's kind whatever it is called; `on NAME { ... }` ignores the payload. Each payload has only its own kind's fields: `note.release` in a `note_on` handler is an error.
- Handlers live in rills. Every instance handles the event, so a lifted rill reacts once per copy.
- Pitch bend, aftertouch and any other controller are control changes on their own channel.
- A declaration nothing handles is a warning.
- `on start { ... }` is built in: it runs once, before the first sample, in every rill instance that has it.

## Sequences

A sequence is a pattern of notes that plays only when the program says so, at its own tempo and meter. Several can play at once. It makes no sound itself: while it plays, it makes events named after it, which handlers react to.

```rill
seq riff(meter: 4/4, step: 1/8, tempo: 120bpm, gate: 0.9, velocity: 0.8) {
    C4, _, E4@0.5, _, [G4, B4]@1, E4, _, C5,
}

rill voice() Sample {
    state pitch: Pitch = C4
    state level: Float = 0

    on riff_note_on(note) claim {
        pitch = note.pitch
        level = note.velocity
    }

    on riff_note_off release {
        level = 0
    }

    return sine(pitch |> equal) * level
}

rill main(speed: Freq = 120bpm) Sample {
    state accent: Float = 1
    let accents: [Float; riff.step_count] = [1.2, 1, 0.8, 1, 1.2, 1, 0.8, 1]

    on start {
        invoke riff(loop: true)
    }

    on riff_step(s) {
        accent = accents[s.step - 1]
    }

    return sum([voice(); 8]) * accent
}
```

- **Steps** are separated by commas: a pitch, a chord (`[G4, B4]`) or a rest (`_`). `@0.5` sets a step's velocity, from 0 to 1. Each note starts with its step and ends `gate` × `step` later.
- **Settings** are all optional:

  | Setting | Default | When |
  | --- | --- | --- |
  | `meter` | `4/4` | fixed when declared; the beat is its denominator (a quarter in 4/4, an eighth in 7/8) |
  | `step` | `1/8` | fixed; a note value (`3/16` is a dotted eighth) |
  | `tempo` | `120bpm` | per invocation; can follow a stream while playing |
  | `gate`, `velocity` | `0.9`, `0.8` | per invocation |
  | `repeat`, `loop` | `1`, `false` | per invocation |
  | `instances` | `64` | fixed; how many copies can play at once |

- **Invoking** happens in `on` handlers, which are moments; a rill body runs every sample:
  - `invoke riff` starts a new instance and returns its id, an `Int`. Invoking twice plays two overlapping copies.
  - `invoke id riff` uses instance `id` of `riff`, and does nothing while it is playing. Ids you pick are whole numbers ≥ 1; fresh ids are negative.
  - `trigger 5 id riff` starts at step 5 (steps count from 1), restarting the instance if it is playing. A step computed while playing wraps around.
  - `halt id riff` or `halt riff` stops instances; the notes they hold end.
  - Settings can be given at invocation: `invoke riff(tempo: 90bpm, repeat: 4)`. A tempo that reads the rill's streams follows them while playing; values from the handler, like `hit.velocity`, are kept from the moment of invoking.
  - `invoke lead_on(pitch: C4, velocity: 1)` sends a declared event directly. Invoked events arrive after the current handler, in the same sample.
- **Tempo:** a tempo known when the program is built must be above 0bpm. While playing, an invocation whose tempo is 0bpm or below does nothing; a running sequence whose tempo stream drops to 0 holds still.
- At most `instances` copies of a sequence play at once; a new one beyond that replaces the oldest (never one started in the same sample; then the new one does not start).

### What a sequence makes

For `seq riff`, these events exist without being declared. The name says what happened; the payload carries the details, all `Int` except the notes'.

| Event | When | Payload |
| --- | --- | --- |
| `riff_start` | an instance starts: `invoke`, or `trigger` on a stopped one | `instance`, `step` (where it starts) |
| `riff_finished` | its last pass is over | `instance`, `step` (the last) |
| `riff_halted` | stopped early: `halt`, or `trigger` restarting it | `instance`, `step` (the one it was on) |
| `riff_replaced` | stopped to make room for a new instance | `instance`, `step` |
| `riff_end` | right after any of the three stops | `instance`, `step` |
| `riff_repeated` | back to step 1 for another pass, from `repeat` or `loop` | `instance`, `pass` (the one beginning) |
| `riff_bar` | the first beat of every bar | `instance`, `bar`, `pass` |
| `riff_beat` | every beat of the meter | `instance`, `beat` (in the bar), `bar`, `pass` |
| `riff_step` | every step, rests included | `instance`, `step`, `pass` |
| `riff_rest` | a step without notes | `instance`, `step`, `pass` |
| `riff_note_on` | a note starts | `pitch`, `velocity`, `instance` (a `NoteOn`) |
| `riff_note_off` | a note ends | `pitch`, `release`, `instance` (a `NoteOff`) |

- Everything counts from 1. Every start is followed, eventually, by exactly one of the three stops and then `end`; reaching the last step is not stopping, and a looping sequence only stops when halted or replaced.
- **Bars and beats** follow the meter and count from the start of each pass: step 1 is always bar 1, beat 1. A beat can fall between steps (`step: 3/16` in 4/4). A pattern that does not fill its last bar ends on a short bar. After `trigger N`, the first beat is the next one in the pattern, or at once if step N starts one.
- **Order in one sample:** note-offs; a stop and `end`; `start` or `repeated`; `bar`; `beat`; `step`; `rest`; note-ons. So a step's handler sees the notes before it already released.
- They are ordinary events: `claim` and `release` work on the note events, plain handlers run in every voice. They cannot be invoked, and a written event cannot take one of their names. Declarations can still filter a sequence's notes, as in `event lead note_on(sender: riff, channel: 2)`; one with no `sender` hears every sequence and the host.
- A handler that could start again what runs it, in the same sample, is an error: `on riff_start { invoke riff }` would never end. Starting, the first step and its notes happen at once; so do the note-offs, `halted` and `end` of a `halt` or a restarting `trigger`. Later events (`finished`, `repeated`, later steps) are fine: `on riff_finished { invoke riff }` plays it again and again.

### What is fixed about a sequence

`riff.field` reads what is fixed when the program is built and the same for every instance. They are constants: usable as defaults, state initialisers and sizes (`[Float; riff.step_count]`).

| Field | Type | `seq riff(meter: 3/4, step: 1/8) { 6 steps }` |
| --- | --- | --- |
| `step_count` | `Int` | 6 |
| `beats_per_bar` | `Int` | 3 |
| `beat_unit` | `Int` | 4 |
| `step_size` | `Float` | 0.125 (of a whole note) |
| `steps_per_beat` | `Float` | 2 |
| `beat_count` | `Float` | 3 |
| `bar_count` | `Float` | 1 |
| `instances` | `Int` | 64 |

Settings an invocation can change (`tempo`, `gate`, `velocity`, `repeat`, `loop`) are not fields.

## Voices

A pool of voices is a frame of separate rill instances, written like the frame type: `[lead(); 8]` is eight `lead`s, each with its own state. Lifting a rill over a frame makes a pool too.

```rill
rill lead() Sample {
    state pitch: Pitch = C4
    state level: Float = 0

    on lead_on(note) claim {
        pitch = note.pitch
        level = note.velocity
    }

    on lead_off release {
        level = 0
    }

    return (pitch |> equal |> sine) * level
}
```

- `on EVENT claim { ... }` (on a `note_on` event) runs in **one** copy, which then holds the note. `on EVENT release { ... }` (on a `note_off` event) runs only in the copy holding a note with the same pitch, sender and instance.
- A released copy stays busy while it still makes sound: it is free once its output has stayed below -90dB for its tail, 100ms unless `claim(tail: 2s)` says otherwise. Effects inside the voice (a reverb tail) keep it busy; effects after the pool do not.
- A new note goes to a free copy; else the copy released longest ago; else the one that has held its note longest.
- Plain `on` handlers run in every copy, as before. Outside a pool, `claim` and `release` behave like plain handlers.

## Functions as values

Fns are values: they can be bound with `let`, passed to fns and rills, returned from fns, and chosen while playing. Anonymous fns use the named syntax without the name.

```rill
fn a432(p: Pitch) Freq { 432Hz * pow(2, (p - A4) / 12st) }

rill voice(pitch: Pitch, tune: fn(Pitch) Freq) Sample {
    return pitch |> tune |> sine
}

voice(E4, a432)                         // a named fn
voice(E4, fn(p) { equal(p, 24) })       // anonymous; types come from `tune`
voice(E4, equal)                        // extra parameters with defaults are fine
voice(E4, if minor { just_c } else { equal })   // chosen while playing
```

Rules:

- Function types are written `fn(A, B) R`. They cannot be `state`, frame elements, event fields or the entry rill's parameters.
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
void         rill_send_event(rill_engine*, uint32_t frame_offset, rill_event ev);       // matched against the declarations
int32_t      rill_event_id(rill_engine*, const char* name);                            // a declared event, or -1
void         rill_send_event_to(rill_engine*, uint32_t frame_offset, int32_t id, rill_payload p);
void         rill_destroy(rill_engine*);
```

A `rill_event` is a sender, a channel and a `rill_payload`: a tagged union of note on (pitch, velocity), note off (pitch, release) and control change (value).

On microcontrollers the host calls `rill_render` from the DMA half-complete and complete interrupts. Memory use is fixed after `rill_load` and can be reported so the host can size its arena.

## Non-goals and open questions

Not in v0.1: plugin formats (CLAP comes later), FFT and other block-level rills, variable-ratio resampling, a JIT, and a fixed-point backend.

Open questions:

- **Headroom on integer targets.** `f32` can exceed 1.0 mid-graph and come back down; fixed-point clips. Options: guard bits in the internal format, or saturating arithmetic with a compiler warning.
- **Feedback in block processing.** Process cycles sample by sample, or fuse each cycle into one node that loops internally?
- ~~**Sequencing.**~~ Settled: sequences are senders of note events, started and stopped from handlers (see Sequences).
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
- [x] **M6 — Sequencing.** Time signatures, programmable sequences.
- [ ] **M7 — Module system.** Export/import, standard library.
- [ ] **M8 — Hot reload.** Swap graphs with a crossfade while audio plays.
- [ ] **M100 — Embedded spike.** Run a fixed patch on a microcontroller through the C API.
