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

**Frame.** One `Sample` per channel, written `[Sample; N]`. Mono is `[Sample; 1]`, 7.1 is `[Sample; 8]`. N is part of the type and known at build time. A frame of one is the same as its element, both ways: a `[Sample; 1]` goes wherever a `Sample` does and the other way round, and a lone value fills a `[Sample; N]` parameter with N = 1.

**State.** Variables declared with `state` inside a rill persist across ticks and callbacks. Only rills have state, so all memory is visible and preallocated.

**Rate.** Every stream carries a sample rate in its type. The default is the host rate; rate-changing rills declare their ratio.

## Syntax sketch

Two kinds of definitions: `fn` for pure functions on values, and `rill` for stream processors with optional state. Beside them, a file holds `const`s (see Constants), event declarations and sequences, and nothing else; running it instantiates its entry rill, `main` unless another is named, and plays what that rill returns. All syntax here is provisional.

```rill
// Pure function on one value. Takes exactly what it declares.
fn abs(x: Sample) Sample {
    if x < 0 { -x } else { x }
}

// Stateful 1:1 rill. Body runs once per tick.
rill peak(x: Sample, release: Time = 300ms) Sample {
    state level: Sample = 0
    let a = abs(x)
    // Falls by 60 dB (to 0.001) over `release`.
    level = if a > level { a } else { level * exp(log(0.001) / (release * RATE)) }
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

Literals carry units: `440Hz`, `120bpm`, `300ms`, `2s`, `+7st`, `+50cents`, `1oct` (`12st`), `-6dB`, `3/2`. A tempo is a frequency, so `120bpm` is `2Hz`. Units convert to samples using the host rate at build time.

## Type system

Types describe one tick's value; stream-ness comes from being inside a rill. Every value in a body is the current tick's. The core types for v0.1:

| Type | Meaning | Notes |
| --- | --- | --- |
| `Sample` | One audio value, nominally \[-1.0, 1.0\] | Storage chosen per target |
| `[Sample; N]` | A frame of N channels | N is a compile-time constant: a whole number, a size parameter, a `const`, or a constant expression such as `riff.step_count * 2` |
| `[[Sample; 2]; N]` | Frames nest: N stereo voices or buses | See Polyphony |
| `Float`, `Int`, `Bool` | Plain values for control logic | Not converted at I/O |
| `Freq`, `Time` | Unit-carrying numbers | Convert to samples via the host rate |
| `Pitch`, `Interval` | Abstract musical values | Turned into `Freq` by a tuning (see Pitch) |
| `Gain` | A level change, written in `dB` | Any plain number is an amplitude factor (see Levels) |
| `Slope` | A level over an interval: `24dB / 1oct` | `Gain / Interval`; stored in dB per octave, so `6dB / 6st` is `12dB / 1oct`. Times an `Interval`, a `Gain` again |
| `fn(A, B) R` | A function value | Pure, takes exactly what it declares |
| `rill(A, B) R` | A rill value | Stateful, lifts like a named rill |
| `Audio<N>` | An audio file with N channels | Read when the program is built; `Audio` alone takes any N (see Audio files) |
| `Image` | An image | Read when the program is built (see Images) |

Numbers are written as `440`, `0.5` or `.5`, `48_000`, `1e-3`, with a unit right after where they have one (`440Hz`, `.5s`, `-6dB`).

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

### Constants

`const NAME: Type = value` names a value worked out when the program is built. The type may be left out, as with `let`.

```rill
const ROOT: Pitch = C3
const CHORD = [ROOT, ROOT + 7st, ROOT + 15st]
const VOICES = 8
const DETUNE = random(lo: 5cents, hi: 15cents)

rill pad() [Sample; 2] {
    const SPREAD = DETUNE * 2
    let levels: [Float; VOICES] = [1 / VOICES as Float; VOICES]
    ...
}
```

- **Where.** At the top level, a `const` belongs to its file and can be used in any order: `CHORD` above uses `ROOT` before it is written. A cycle (`const A = B`, `const B = A`) is an error. In a block, a `const` is scoped to the block, like `let`.
- **The value** must be constant: numbers with units, pitches, chords and frames, arithmetic, prelude functions of constants (`random()` included), other `const`s, sequence fields and `RATE`. A `fn` or `rill` cannot be used, and a `const` cannot hold a function or rill.
- **One value.** A top-level `const` is worked out once, so `DETUNE` is the same number everywhere it is used. A `const` in a rill is worked out once per instance: each of `[pad(); 4]` gets its own `random()`. It can use the rill's size parameters.
- **Sizes.** A `const` holding a whole number of at least 1 is a size: `[Float; VOICES]`, `spread<VOICES>(...)`. One worked out from `RATE` or from a size parameter is not (yet).
- A `const` cannot be assigned. A name in a block can shadow a top-level `const`; a top-level `const` cannot take the name of a definition, sequence, event, note (`A`, `C4`), prelude function or prelude constant.

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

A note name is a letter from `C` to `B`, a sharp (`#` or `s`) or a flat (`b`) if it has one, and an octave, 4 if left out: `C4`, `F#3`, `Fs3`, `Eb4`, `Bb`. Any letter takes either, so `B#3` is `C4` and `Cb4` is `B3`.

A note name is an abstract `Pitch`, not a frequency; it becomes `Freq` only when it goes through a tuning. A tuning is an ordinary function from `Pitch` to `Freq`. Raw `Hz` literals bypass tuning entirely.

```rill
E4 |> equal(24, a4: 432Hz) |> sine      // quarter tones, A4 at 432Hz
E4 |> just(C) |> sine                   // just intonation on C
(E4 + 4st) |> equal |> sine             // Pitch + Interval -> Pitch
sum([E4, F#4, B4] |> equal |> sine)     // a chord: three voices, mixed

let tuning = fn(p: Pitch) Freq { pythagorean(p, D) }   // pick one, pass it around
```

The prelude's tunings take the pitch first, so pipes read naturally:

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
- Chords are frames of pitches, written with `[ ]` and commas, not `+`, because `+` already means addition on `Freq` and mixing on streams. The prelude's tunings accept a chord and tune each pitch; passing the result to a rill that takes `Freq` lifts it to one voice per pitch.
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
| `note_change` | `NoteChange` | `pitch: Pitch`, `pressure: Float` (0–1), `timbre: Float` (0–1), `slide: Bool`, `instance: Int`, `from: Pitch` |
| `control_change` | `Float` | the value, as sent |

A note is split in two, the way a voice uses it:

- **`note_on` and `note_off` are the gate:** a key going down and how hard, a key coming up and how fast. `note_on` still says which pitch, for telling notes apart.
- **`note_change` is the note's state:** its pitch, pressure (aftertouch) and timbre (MPE's third dimension). It comes right after every `note_on`, with the note's starting values and `slide` false, and again whenever one of them changes while the note is held (`slide` true). `from` is the pitch the note was held at, which says which note changes; a host sending one without `from` changes the note at its own pitch. A host's notes start with pressure 0 and timbre 0.5.

So a voice keeps its pitch in one handler, and can tell a fresh note (jump) from a slide (glide):

```rill
on keys_on(n) claim { gate = 1; level = n.velocity }
on keys_change(n) { pitch = n.pitch; glide = if n.slide { 40ms } else { 0ms } }
on keys_off release { gate = 0 }
```

Rules:

- The filters are `sender` and `channel`, whole-number constants; a sender can also be a sequence's name (see Sequences). A filter left out matches anything.
- `instance` is the id of the sequence instance that played a note, and 0 for notes from the host.
- An incoming event runs the handlers of every declaration it matches, in declaration order. A host can also send straight to a declaration by name, skipping its filters; tests and the sequencer do that.
- `on NAME(param)` takes at most one parameter, typed by the event's kind whatever it is called; `on NAME { ... }` ignores the payload. Each payload has only its own kind's fields: `note.release` in a `note_on` handler is an error.
- Handlers live in rills. Every instance handles the event, so a lifted rill reacts once per copy.
- Per-note pitch, pressure (aftertouch) and timbre are `note_change`s. Channel-wide pitch bend and any other controller are control changes on their own channel.
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

- **Steps** are separated by commas: a pitch, a chord (`[G4, B4]`) or a rest (`_`). Each note starts with its step and ends `gate` × `step` later.
- **Holding:** `~` after a step (`C2~`) holds its notes into the next step, which says what happens to them: the same pitch ties them (one note, no new `note_on`), another pitch slides them (a `note_change` with `slide` true, and no new `note_on`), a rest ends them. A `~` on its own as a step goes on with the notes before it, so `C2, ~, ~` is one note three steps long. Chords pair up by place: `[C3, E3]~, [D3, F3]` slides C3 to D3 and E3 to F3; extra notes start, notes without a partner end. A held note ends at the gate of the last step that holds it. The last step holds into the first when the pattern goes round again, and ends with the pattern otherwise.

  ```rill
  seq acid(step: 1/16, gate: .5) { C2~, C2, _, D#2~, F2~, G2, _, C3 }
  seq pad(step: 1/4) { C3@p.2~, ~@p.6~, ~@p1, _ }   // a note swelling in pressure
  ```
- **A step's values** follow `@` as flags, each a letter and a value from 0 to 1: `v` velocity, `p` pressure, `t` timbre, `r` release. Letters written together share the value after them, so `G3@v.5p1rt.3` is velocity 0.5, pressure 1, and release and timbre 0.3. A bare value is the velocity: `E4@0.5`. Velocity goes with the `note_on`, pressure and timbre with its `note_change`, and release with the `note_off`. On a chord, the values go to every note. On a step that holds notes (a tie, a slide or `~`), `p` and `t` change them with a `note_change`, `r` sets the release of their note-off to come, and `v` is an error: they have started already.
- **Settings** are all optional:

  | Setting | Default | When |
  | --- | --- | --- |
  | `meter` | `4/4` | fixed when declared; the beat is its denominator (a quarter in 4/4, an eighth in 7/8) |
  | `step` | `1/8` | fixed; a note value (`3/16` is a dotted eighth) |
  | `tempo` | `120bpm` | per invocation; can follow a stream while playing |
  | `gate`, `velocity` | `0.9`, `0.8` | per invocation |
  | `pressure`, `timbre`, `release` | `0`, `0.5`, `0.5` | per invocation; for notes that do not give their own |
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
| `riff_note_change` | right after each `note_on`, with its starting values; a slide; a held step that changes pressure or timbre | a `NoteChange` |

- Everything counts from 1. Every start is followed, eventually, by exactly one of the three stops and then `end`; reaching the last step is not stopping, and a looping sequence only stops when halted or replaced.
- **Bars and beats** follow the meter and count from the start of each pass: step 1 is always bar 1, beat 1. A beat can fall between steps (`step: 3/16` in 4/4). A pattern that does not fill its last bar ends on a short bar. After `trigger N`, the first beat is the next one in the pattern, or at once if step N starts one.
- **Order in one sample:** note-offs; a stop and `end`; `start` or `repeated`; `bar`; `beat`; `step`; `rest`; slides and changes of held notes; note-ons, each followed by its `note_change`. So a step's handler sees the notes before it already released.
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

Settings an invocation can change (`tempo`, `gate`, `velocity`, `pressure`, `timbre`, `release`, `repeat`, `loop`) are not fields.

### What an instance is doing

`riff[id].field` reads instance `id` of a sequence as it plays: every instance has its own tempo, which can follow a stream, so these are read every sample, not fixed. The id is what `invoke` returned, or a note's `instance`, kept in `state` by the voice that claimed it:

```rill
rill pad() Sample {
    state id: Int = 0
    state press: Float = 0
    on pads_note_on(n) claim { id = n.instance }
    on pads_note_change(n) { press = n.pressure }
    let p = slew(press, pads[id].step_time, pads[id].step_time)   // glide over one step
    ...
}
```

| Field | Type | |
| --- | --- | --- |
| `tempo` | `Freq` | its tempo now |
| `step_time` | `Time` | one step at that tempo |
| `beat_time` | `Time` | one beat at that tempo |
| `step`, `pass` | `Int` | where it is, from 1 |
| `progress` | `Float` | how far through its step, from 0 up to just under 1 |
| `playing` | `Bool` | |

- An instance that is not playing (never started, stopped, or id 0, which host notes carry) has the sequence's own tempo, so a voice's release does not jump; `step`, `pass` and `progress` are 0. A sequence holding still (tempo 0 or below) gives an hour for `step_time` and `beat_time`.
- `riff[id]` alone is not a value, and fns cannot read it: what it gives changes while playing.
- A segment ends at every step, so `progress` and a followed tempo are read sample by sample in a looping stage, exactly as running the tick sample by sample would.

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
- A `note_change` handler runs only in the copy holding the note it changes (the one at `from`), without a keyword; that copy then holds the note at its new pitch, so the `note_off` that comes later finds it.
- A released copy stays busy while it still makes sound: it is free once its output has stayed below -90dB for its tail, 100ms unless `claim(tail: 2s)` says otherwise. Effects inside the voice (a reverb tail) keep it busy; effects after the pool do not.
- A new note goes to a free copy; else the copy released longest ago; else the one that has held its note longest.
- Plain `on` handlers run in every copy, as before. Outside a pool, `claim` and `release` behave like plain handlers.

## Functions and rills as values

Fns and rills are values: they can be bound with `let`, passed to fns and rills, returned from fns, and chosen while playing. Anonymous fns use the named syntax without the name; there are no anonymous rills.

```rill
fn a432(p: Pitch) Freq { 432Hz * pow(2, (p - A4) / 12st) }

rill voice(pitch: Pitch, tune: fn(Pitch) Freq) Sample {
    return pitch |> tune |> sine
}

rill layer(osc: rill(Freq) Sample, root: Freq) Sample {
    return sum(osc([root, root * 2]))
}

voice(E4, a432)                         // a named fn
voice(E4, fn(p) { equal(p, 24) })       // anonymous; types come from `tune`
voice(E4, equal)                        // extra parameters with defaults are fine
voice(E4, if minor { just_c } else { equal })   // chosen while playing
layer(sine, 220Hz)                      // a named rill
```

Rules:

- Function types are written `fn(A, B) R`; rill types are written `rill(A, B) R`. They cannot be `state`, frame elements, event fields or the entry rill's parameters.
- An anonymous fn's parameter and return types can be left out when the surroundings say what they are (a parameter of function type, an annotated `let`, a `return`, or a fn's declared return type). Otherwise they are required.
- Anonymous fns capture what they use, by value, at the point they are made. They are fns, so they are pure: they read what they capture but cannot change it, and cannot call rills.
- A named fn or rill can be used where a callable type with fewer parameters is expected, if its remaining parameters have defaults. A prelude function that works on several types (`sin`) needs an expected type to pick one.
- A rill value still behaves like a rill call: it can only be called from a rill, it lifts over extra frame layers, and every call site has its own state.
- Recursion is still not allowed, including through callable values.
- At build time every call of a callable value is replaced by the callable's body, so callable values cost nothing while playing. A callable chosen while playing becomes a branch between the candidates; only the chosen one runs.

## Audio files

A path is a value, written as it is: `./kick.wav`, `../pads/warm.flac`, `/samples/hit.aiff`. A path to an audio file is an `Audio`, read when the program is built, never while it plays:

```rill
import @gen/sampler

const KICK = ./sounds/kick.wav

rill main() Sample {
    return KICK |> samples()
}
```

- **Paths** start with `./`, `../` or `/`, are relative to the file they are written in (or for a program without a file, to where Rill runs), and hold letters, digits and `_ . - + /`, so no spaces. A path goes on through `.`, so a field needs parentheses: `(./kick.wav).length`. Imports use the same paths (see Modules).
- **Formats:** WAV, AIFF, CAF, FLAC, ALAC, MP1/2/3, Vorbis and AAC, in RIFF, AIFF, CAF, OGG, MP4/M4A and Matroska/WebM; the extension says which. Opus is not one of them. Decoding is pure Rust, behind the `audio-files` cargo feature (on by default).
- **The type** has the channels in it: `./kick.wav` is an `Audio<1>`, a stereo file an `Audio<2>`. A rill takes any file with a size parameter, `rill twice<N>(a: Audio<N>) [Sample; N]`, and `Audio` alone takes any file whose channels it does not need. An audio file is fixed when the program is built, so it cannot be `state`, a frame element, or a parameter of the entry rill.
- **Rate:** every file is resampled to the host rate when the program is built (a windowed sinc), so a second of the file is a second while playing and its pitch is its own. The rate syntax of rate-changing rills is a different thing: it is relative to the host rate, and does not apply.
- **Where files come from:** every file a program reads, modules and audio, comes through the `Sources` handler of whatever builds it, in the build stage only (`rill play`, `render`, `profile`, `check`, every hot reload, an editor checking as you type), never while it plays. The disk is the default; an embedder can serve bytes from memory or a bundle, or hand over samples it made itself (`Sources::audio`), which needs no decoder at all, and says what hot reload should watch (`Sources::watch`).
- **Built again only when changed:** a file is kept by its path while its source gives it the same stamp (on disk, when it last changed), so a hot reload of the program does not read it again; replacing the file reloads the program, which plays the new sound.

**Fields**, all constants:

| Field | Type | |
| --- | --- | --- |
| `channels` | `Int` | a size: `[Sample; KICK.channels]` |
| `frames` | `Int` | at its own rate |
| `length` | `Time` | |
| `rate` | `Freq` | its own rate, before resampling |
| `root` | `Pitch` | the note it plays unchanged, from a WAV `smpl` or AIFF `INST` chunk; an error for a file that does not say |
| `loop_start`, `loop_end` | `Time` | its loop from the same chunks, or `0s` and its `length` |

**Reading** is the prelude's `read(audio, at)`: one value per channel at time `at`, read between samples on a cubic, and silence before the start and after the end. A time far into a long file is not exact as one `f32` number (a minute in, a step of one sample rounds by several percent), so `read(audio, frame, part)` reads at whole frame `frame` at the host rate plus `part` of one, which stays exact. Everything else is built on `read`: playing, reversing, scrubbing, granular, wavetables.

**Playing** is `@gen/sampler`, written in Rill on `read`:

- `samples(audio, speed, loop)` plays the file from the start of the program, once or over and over.
- `play(audio, gate, speed, loop)` plays it from its start every time `gate` rises above 0, as an envelope follows a gate; with `loop` it loops while the gate is held.
- `speed` is a stream: 2 is an octave up and half as long, as on a tape. Nothing is stretched; changing pitch and length apart is a later feature.

A voice per note, its speed from the note's pitch, makes a sampler or a drum machine (`examples/sampler.rill`):

```rill
rill kick() Sample {
    state gate: Float = 0
    state rate: Float = 1
    on kicks_note_on claim { gate = 1 }
    on kicks_note_change(n) { rate = pow(2, (n.pitch - C2) / 12st) }
    on kicks_note_off release { gate = 0 }
    return play(KICK, gate, rate)
}
```

## Images

A path to an image is an `Image`, read in the build stage like an audio file and through the same `Sources`:

```rill
const FACE = ./face.png

rill main() Sample {
    let red = pixel(FACE, 10.5, 3.5)[0]   // [red, green, blue, alpha]
    ...
}
```

- **Formats:** PNG, JPEG, GIF (the first frame), WebP, BMP, TGA, ICO, QOI, PNM, TIFF, HDR and OpenEXR, all pure Rust (the `image` crate), behind the `images` cargo feature (on by default). Not AVIF, which needs a C library.
- **Fields:** `width` and `height` (`Int` constants, usable as sizes: `[Float; FACE.height]`) and `alpha` (`Bool`: whether the file has an alpha channel; without one, alpha is 1).
- **`pixel(image, x, y)`** (prelude, pure) gives red, green, blue and alpha, each 0 to 1. `y` 0 is the top row; a pixel's colour is at its centre (`x + 0.5`), and between centres it is blended from the four nearest; outside the image everything is 0.
- An image is fixed when the program is built: like an audio file, it cannot be `state`, a frame element, or an entry parameter.

## Modules

A file is a module. A file names the files it uses with `import`, and sees only what they `export`.

```rill
// osc.rill
fn blep(t: Float, dt: Float) Float { ... }      // private: only osc.rill sees it
export rill saw(freq: Freq) Sample { ... }

// synth.rill
export import ./osc                             // importers of synth get saw too
import ./fx                                     // only synth sees fx's exports
export const VOICES = 4
export rill pad(pitch: Freq) Sample { return saw(pitch) |> lowpass() }

// song.rill
import ./synth
rill main() Sample { return pad(110Hz) + saw(55Hz) * 0.1 }
```

- **Exports.** `export` goes before `fn`, `rill`, `const`, `event`, `seq` or `import`. Everything else is private to its file. Exporting a sequence exports the events it makes and its fields.
- **Re-exports.** `export import ./b` passes on everything `b` exports, including what `b` passes on; `b` is imported in the file as well. Importing that file gives both.
- **Core.** A path starting with `@` names a module of the core library (see Prelude and core), wherever the file is: `import @gen/osc`.
- **Paths** are written as they are, without quotes, as in Nix: `./` for the importing file's folder, `../` for the one above, or `/` for an absolute path, with `/` between folders: `import ./lib/osc` reads `lib/osc.rill`. A path has letters, digits and `_ . - + /`, but no spaces, so `import ./my-osc` works. Rill has no strings. `import osc` without `./` is an error. The extension is never written; `import ./osc.rill` is an error, and any other extension is not a Rill file.
- **Names.** Imported names are used as they are: `saw`, not `osc.saw`. A file's own declaration wins over an imported one with the same name. Two imports exporting different things by one name is an error only where the name is used; one thing reached by two routes is not a clash.
- **One program.** The program is every file reachable from the one being run, each loaded once. A file reached by two routes is one file: its sequences and `const`s exist once.
- **Cycles** are allowed: files can import each other, and re-export each other. Recursion across files is still recursion.
- **The entry** is looked up in the file being run, including what it imports. Every file can have its own private `main` to try it out alone. A file that exports something is a library, and `rill check` does not need it to have a `main`.
- `import` and `export` are keywords, and only work at the top level of a file.

## Prelude and core

Two libraries come with the compiler, so every program has them wherever it runs.

**The prelude** is what cannot be written in Rill, or would be much slower in it: the constants `PI`, `TAU` and `RATE`; maths (`sin`, `cos`, `tan`, `tanh`, `exp`, `log`, `sqrt`, `pow`, `abs`, `floor`, `ceil`, `round`, `wrap`, `min`, `max`, `clamp`); `sum` over frames of any size; `random`; the tunings (`equal`, `just`, `pythagorean`, `meantone`); `level` and `amp`; `read` for audio files and `pixel` for images; and two that keep state, so only rills can use them: `noise()` and `delay`. It is always there and never imported.

**`noise()`** is a new random number in [-1, 1) every sample. Every call has its own sequence, seeded from the build's seed, so voices never share one and `--seed` repeats it.

**`delay(x, time, max)`** is what `x` was `time` ago:

- A frame of times reads several taps from one buffer and gives a frame back: `delay(x, [10ms + lfo * 2ms, 25ms], max: 30ms)`. Every time is a stream, each tap its own, read between samples so a moving time does not crackle. A time is held between one sample and `max`.
- `max` is the buffer's length, set aside when the program is built, so it must be a constant: the one thing about a delay that cannot move while it runs. Constant times are their own `max`. Moving times without one are a warning, and the delay holds 2s to start with. A rill can pass its own parameter as `max`; it then has to be a constant where that rill is called. A time past `max` is held at it until the line grows (see Live rebuilds).
- Every call has its own buffer, as every rill call has its own state; a frame of channels gets one per channel.
- Feedback goes through `state`, and whatever sits between the delay and the `state` is in the loop (it adds one sample to the loop's length):

```rill
rill echo(x: Sample, time: Time, feedback: Float) Sample {
    state back: Sample = 0
    let wet = delay(x + back, time)
    back = lowpass(wet, 3kHz) * feedback
    return x + wet
}
```

**A parameter called `max`** is the ceiling for the arguments of its type: what has to exist before audio starts (a delay's memory, a filter's stages) is sized for it. When an argument of that type changes while playing and `max` is left out, the checker warns, and the call uses `max`'s default. This holds for any rill, not only the prelude's and core's. A `max` is where a size starts, not a limit: a value that goes past it is held there for a few milliseconds while the program is rebuilt with room for it (see Live rebuilds). `ceiling(x, start)` (prelude) is how a rill picks such a size: `x` itself when it is a constant once the program is built; otherwise `start`, or what this place has grown to, and it notes when `x` goes past it.

**Most powerful first.** Core keeps one function where one can express another: `slew` is the one-pole filter, the smoother and the envelope follower; `dynamics` is the compressor, limiter, expander and gate; `comb` is the echo, the comb filter and the allpass. A plain convenience on top (`sine` for `phasor |> as_sine()`) stays only where it reads much better.

**`core`** is the standard library: ordinary Rill modules, imported with `@` and their folder: `import @gen/osc`. Only core paths start with `@`, so they never clash with a file next to yours. Their sources are the files under `core/` in the compiler's repository, built into the compiler, so `core` is the same everywhere and never read from disk. A program compiled from a string, with no file, can import core too. `rill core` lists the modules and what they export; `rill core @gen/osc` prints one.

- **`@gen`**: generators. `@gen/osc` has oscillators in two halves: `phasor(freq, offset)`, a ramp from 0 to 1 per cycle that says where in the cycle you are (not meant to be heard), and the shapes `as_sine`, `as_saw` and `as_square`, which turn a phase into a wave (the saw and square band-limited with `poly_blep`, which is exported for custom shapes). `sine`, `saw` and `square` are the two together: `saw(f)` is `phasor(f) |> as_saw()`. Anything done to the phase in between works for every shape: `warp(phase, width)` moves the middle of the cycle, which is pulse width on a square and a brighter, squeezed wave on a sine or saw.
- **`@gen/noise`**: `pink()` and `brown()` (white is `noise()`). Instruments, such as a plucked string, are made from these parts, not kept in core (`examples/delay.rill` builds one).
- **`@gen/sampler`**: `samples` and `play`, which play audio files (see Audio files).
- **`@effects/env`**: `slew(x, rise, fall)` (smoothing, following, one-pole lowpass) and `ahdsr(gate, attack, hold, decay, sustain, release)` (with `hold: 0s`, the default, an ADSR). Envelopes follow a gate: a `Float` above 0 while a note is held.
- **`@effects/filter`**: `lowpass`, `highpass`, `bandpass`, `notch`, each `(x, cutoff, resonance, slope, max)`, built from state-variable stages that stay stable under any modulation. `slope` is a stream (`24dB / 1oct`; a slope between whole poles blends the two nearest); `max` is the steepest it will be to start with (48 dB/oct when left out), which sets how many stages are built.
- **`@effects/dynamics`**: `dynamics(x, threshold, above, below, attack, release)`; `above: 0.25` is 4:1 compression, `above: 0` a limiter, `below: 100` a gate.
- **`@effects/delay`**: `comb(x, time, feedback, blend, tone, max)`: an echo, a pitched comb, or an allpass (`blend: -feedback`).
- **`@mix/stereo`**: `pan(x, position)` (equal power, -1 to 1) and `width(x, amount)`.
- **`@util/control`**: `map`, `rising`, `sample_hold`.
- Next: reverb and modulation effects (chorus, flanger, phaser) on `delay` and `comb`.

## Execution model

A Rill program runs in two stages: a build stage that may do anything, and a run stage that only executes a frozen graph inside the audio callback.

1. **Parse and type-check** the source.
2. **Build stage** (any thread): instantiate the entry rill with its parameter defaults, resolve pitches and units, inline every rill call (each call site gets its own state), expand chords and channel lifting, and produce a graph of nodes.
3. **Optimize**: constant folding, specialization for constant parameters, control-rate inference, dead-node removal, buffer reuse via liveness analysis.
4. **Freeze**: allocate every buffer and every `state` variable in one arena; topologically sort nodes.
5. **Run stage** (audio thread): on each callback, pop due events, split the block at their sample offsets, run each node over its slice in order, write the final node into the host's output buffer.

Run-stage rules: no allocation, no locks, no syscalls, no unbounded loops; denormals flushed to zero. Control input arrives through a lock-free single-producer/single-consumer queue, and parameter changes are smoothed over a few milliseconds.

Per-callback inputs the runtime receives from the host: input buffer, output buffer, frame count (up to a declared maximum), channel counts, sample rate, frame position, and timestamped events. Buffers are planar internally; interleaved host buffers are converted at the boundary.

### Stages over segments

A rill's body describes one tick, but most of it does not need to run one sample at a time. The compiled tick is split into stages:

- **Streaming stages** hold plain maths on values from this sample only: a phasor's increment, a filter's coefficients, shaping, mixing, levels. Each instruction runs over a whole segment of samples in one go, on buffers, with SIMD.
- **Looping stages** hold what needs the previous sample first: recurrences (a phasor's phase, a filter's state), values read before they are written, noise, delay lines, voice pools running together, and `if`s with side effects on only one side. They run sample by sample, as before, but only their own instructions.

`if`s without such side effects become straight-line code first, so they can stream: both sides run, and selects keep what the side taken writes.

A segment runs from one moment where something happens to the next: a host event or control change (the engine already splits blocks there), or a sequence's next note, beat or step. Handlers run only at segment boundaries, and see every register as the last sample left it. Segments are at most 64 samples long.

Output is identical to the bit to running the tick sample by sample. That is still how a program runs whose sequences follow values it computes, since when their steps fall is only known sample by sample. `rill profile` reports how much streams, and `--per-sample` runs the old way, to compare.

### Live rebuilds

A running program can be replaced by a new build without a gap and without losing what it was doing: oscillators keep their phase, delays keep their echoes, envelopes, voices and playing sequences carry on. Two things rebuild it:

- **Hot reload.** `rill play song.rill` watches every file in the program's tree and rebuilds when one changes (changes within 50ms of each other count as one), with the same seed. A build with errors is printed and the old program keeps playing; otherwise the terminal says `reloaded song.rill (12 states carried, 1 new)`. `--no-watch` turns this off, and a build without the `watch` cargo feature (on by default) leaves the watcher out entirely.
- **Growing ceilings.** Every `max` is where a size starts. When a value goes past one (a host pushes a filter's slope past the stages built for it, or a delay's time past its line), the program holds it at the ceiling and notes how far it wanted to go. The control side rebuilds with that one place raised to 1.5 times what was asked, at most once per 50ms. Ceilings never shrink, and there is no upper size.

**State is carried across by name.** Every piece of state (`state` registers, delay lines, noise, voice pools, sequences) has a key made of names, not positions: the `let`, `state`, assignment, `return` or handler each call is in, then the callee and its count among unnamed calls in that statement, as in `main#1/lead/saw#1/return/phasor#1.phase`. Copies and loop iterations add their index. So moving statements around, or editing one rill, never disturbs state elsewhere. Keys only on one side are then paired when they differ in one level and the state below has the same shape, so renaming a `let` keeps its state; two candidates pair neither. New keys start fresh, gone ones are dropped, and `random()` is seeded per key, so an edit only changes the picks of what it touched. Live controls keep their values.

**Handing over.** The new program is built off the audio thread and passed over through an atomic slot. At the start of a block the audio thread copies state across, then runs both programs for 20ms, crossfading along half a cosine (the gains always add to 1), and passes the old one back to be freed. Events and controls reach both while they overlap. Nothing is allocated, locked or waited on by the audio thread. A build that arrives while one waits replaces it; one that arrives during a fade waits for it to end.

In Rust, `rill::live::Live` is the control side (`reload`, `reload_text`, `grow`, `collect`) and `Player` the audio side (`render_*`, `set_param`, `dispatch`).

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
- **Feedback in block processing.** Cycles now run sample by sample in looping stages of their own (see Stages over segments). Fusing a common one (a one-pole, a phasor, a filter stage) into a single instruction that loops internally would make them faster still.
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
- [x] **M7 — Module system.** Export/import, standard library.
- [x] **M8 — Hot reload.** Swap graphs with a crossfade while audio plays.
- [ ] **M100 — Embedded spike.** Run a fixed patch on a microcontroller through the C API.
