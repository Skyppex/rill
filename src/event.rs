//! Events: what hosts send, and the declarations that accept them.
//!
//! Every incoming [`Event`] comes from a `sender` on a `channel` and carries
//! a [`Payload`] whose variant is its kind. What senders and channels mean is
//! up to the host; Rill only compares the numbers. A program's sequences are
//! senders too ([`Sender::Seq`]). A program declares the
//! events it handles ([`EventDecl`]), each one a kind with optional filters,
//! and the engine runs the handlers of every declaration an event matches.

use std::fmt;

use crate::lang::types::Type;
use crate::lang::value::Value;

/// The kinds of event. The first four can be declared and sent by hosts;
/// the rest are made by sequences only (see [`EventKind::SEQ`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// A key goes down: the gate opens.
    NoteOn,
    /// A key comes up: the gate closes.
    NoteOff,
    /// A held note's pitch, pressure or timbre changes; also sent right
    /// after every `note_on`, with its starting values.
    NoteChange,
    ControlChange,
    /// An instance starts playing.
    Start,
    /// An instance's last pass is over.
    Finished,
    /// An instance is stopped early, by `halt` or a restarting `trigger`.
    Halted,
    /// An instance is stopped to make room for a new one.
    Replaced,
    /// An instance stops, for any of the three reasons above.
    End,
    /// An instance goes back to step 1 for another pass.
    Repeated,
    Step,
    /// A step with no notes.
    Rest,
    Beat,
    Bar,
}

impl EventKind {
    /// The kinds a program can declare, as in `event keys note_on`.
    pub const ALL: [EventKind; 4] = [
        EventKind::NoteOn,
        EventKind::NoteOff,
        EventKind::NoteChange,
        EventKind::ControlChange,
    ];

    /// The events every sequence makes, named after it: `riff_note_on`,
    /// `riff_start` and so on (see [`EventKind::suffix`]).
    pub const SEQ: [EventKind; 13] = [
        EventKind::NoteOn,
        EventKind::NoteOff,
        EventKind::NoteChange,
        EventKind::Start,
        EventKind::Finished,
        EventKind::Halted,
        EventKind::Replaced,
        EventKind::End,
        EventKind::Repeated,
        EventKind::Step,
        EventKind::Rest,
        EventKind::Beat,
        EventKind::Bar,
    ];

    /// The keyword used in declarations, as in `event keys note_on`; for
    /// the kinds only sequences make, `seq_` and their suffix.
    pub fn name(self) -> &'static str {
        match self {
            EventKind::NoteOn => "note_on",
            EventKind::NoteOff => "note_off",
            EventKind::NoteChange => "note_change",
            EventKind::ControlChange => "control_change",
            EventKind::Start => "seq_start",
            EventKind::Finished => "seq_finished",
            EventKind::Halted => "seq_halted",
            EventKind::Replaced => "seq_replaced",
            EventKind::End => "seq_end",
            EventKind::Repeated => "seq_repeated",
            EventKind::Step => "seq_step",
            EventKind::Rest => "seq_rest",
            EventKind::Beat => "seq_beat",
            EventKind::Bar => "seq_bar",
        }
    }

    /// What follows a sequence's name in the event it makes of this kind:
    /// `riff_` + `step`.
    pub fn suffix(self) -> &'static str {
        self.name().strip_prefix("seq_").unwrap_or(self.name())
    }

    /// The type a handler's parameter has, as in `on keys(note: NoteOn)`.
    pub fn type_name(self) -> &'static str {
        match self {
            EventKind::NoteOn => "NoteOn",
            EventKind::NoteOff => "NoteOff",
            EventKind::NoteChange => "NoteChange",
            EventKind::ControlChange => "Float",
            EventKind::Start => "SeqStart",
            EventKind::Finished => "SeqFinished",
            EventKind::Halted => "SeqHalted",
            EventKind::Replaced => "SeqReplaced",
            EventKind::End => "SeqEnd",
            EventKind::Repeated => "SeqRepeated",
            EventKind::Step => "SeqStep",
            EventKind::Rest => "SeqRest",
            EventKind::Beat => "SeqBeat",
            EventKind::Bar => "SeqBar",
        }
    }

    /// A kind a program can declare, by its keyword.
    pub fn from_name(name: &str) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Made by sequences only.
    pub fn is_seq_only(self) -> bool {
        !EventKind::ALL.contains(&self)
    }

    /// Names of the payload's values, in the order a handler receives them.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            EventKind::NoteOn => &["pitch", "velocity", "instance"],
            EventKind::NoteOff => &["pitch", "release", "instance"],
            EventKind::NoteChange => &["pitch", "pressure", "timbre", "slide", "instance", "from"],
            EventKind::ControlChange => &["value"],
            EventKind::Start
            | EventKind::Finished
            | EventKind::Halted
            | EventKind::Replaced
            | EventKind::End => &["instance", "step"],
            EventKind::Repeated => &["instance", "pass"],
            EventKind::Step | EventKind::Rest => &["instance", "step", "pass"],
            EventKind::Beat => &["instance", "beat", "bar", "pass"],
            EventKind::Bar => &["instance", "bar", "pass"],
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The most values a payload has: a `note_change`'s.
pub const MAX_FIELDS: usize = 6;

/// The most values an event only sequences make has.
pub const SEQ_FIELDS: usize = 4;

/// A `note_change`'s values that a host leaves out: no pressure, timbre in
/// the middle.
pub const DEFAULT_PRESSURE: f32 = 0.0;
pub const DEFAULT_TIMBRE: f32 = 0.5;

/// What happened. Pitches are Rill `Pitch` values, which count semitones
/// like MIDI note numbers (A4 is 69); velocities, releases, pressures and
/// timbres are 0–1.
/// `instance` is the id of the sequence instance that played a note, and 0
/// for notes from the host. A control change's value is passed on as it is.
/// The events only sequences make carry whole numbers, in
/// [`EventKind::fields`] order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Payload {
    NoteOn {
        pitch: f32,
        velocity: f32,
        instance: i32,
    },
    NoteOff {
        pitch: f32,
        release: f32,
        instance: i32,
    },
    /// The held note at `from` is now at `pitch`, with this pressure and
    /// timbre. `slide` is whether it was sounding already: false for the
    /// change that comes with a `note_on`.
    NoteChange {
        pitch: f32,
        pressure: f32,
        timbre: f32,
        slide: bool,
        instance: i32,
        from: f32,
    },
    Control(f32),
    Seq {
        kind: EventKind,
        values: [i32; SEQ_FIELDS],
    },
}

impl Payload {
    pub fn kind(&self) -> EventKind {
        match self {
            Payload::NoteOn { .. } => EventKind::NoteOn,
            Payload::NoteOff { .. } => EventKind::NoteOff,
            Payload::NoteChange { .. } => EventKind::NoteChange,
            Payload::Control(_) => EventKind::ControlChange,
            Payload::Seq { kind, .. } => *kind,
        }
    }

    /// The values in [`EventKind::fields`] order.
    pub fn values(&self) -> [f32; MAX_FIELDS] {
        match *self {
            Payload::NoteOn {
                pitch,
                velocity,
                instance,
            } => [pitch, velocity, instance as f32, 0.0, 0.0, 0.0],
            Payload::NoteOff {
                pitch,
                release,
                instance,
            } => [pitch, release, instance as f32, 0.0, 0.0, 0.0],
            Payload::NoteChange {
                pitch,
                pressure,
                timbre,
                slide,
                instance,
                from,
            } => [
                pitch,
                pressure,
                timbre,
                f32::from(u8::from(slide)),
                instance as f32,
                from,
            ],
            Payload::Control(value) => [value, 0.0, 0.0, 0.0, 0.0, 0.0],
            Payload::Seq {
                values: [a, b, c, d],
                ..
            } => [a as f32, b as f32, c as f32, d as f32, 0.0, 0.0],
        }
    }

    /// The payload of kind `kind` with `values` in [`EventKind::fields`]
    /// order.
    pub fn from_values(kind: EventKind, values: [f32; MAX_FIELDS]) -> Payload {
        match kind {
            EventKind::NoteOn => Payload::NoteOn {
                pitch: values[0],
                velocity: values[1],
                instance: values[2] as i32,
            },
            EventKind::NoteOff => Payload::NoteOff {
                pitch: values[0],
                release: values[1],
                instance: values[2] as i32,
            },
            EventKind::NoteChange => Payload::NoteChange {
                pitch: values[0],
                pressure: values[1],
                timbre: values[2],
                slide: values[3] != 0.0,
                instance: values[4] as i32,
                from: values[5],
            },
            EventKind::ControlChange => Payload::Control(values[0]),
            kind => Payload::Seq {
                kind,
                values: [0, 1, 2, 3].map(|i| values[i] as i32),
            },
        }
    }

    /// The pitch and instance of a note, for telling notes apart. For a
    /// `note_change`, the note it changes: the pitch it is held at.
    pub fn note(&self) -> Option<(f32, i32)> {
        match *self {
            Payload::NoteOn {
                pitch, instance, ..
            }
            | Payload::NoteOff {
                pitch, instance, ..
            } => Some((pitch, instance)),
            Payload::NoteChange { from, instance, .. } => Some((from, instance)),
            Payload::Control(_) | Payload::Seq { .. } => None,
        }
    }

    /// The `note_change` that follows a `note_on` from a host: its pitch,
    /// with no pressure and timbre in the middle.
    pub fn starting_change(&self) -> Option<Payload> {
        match *self {
            Payload::NoteOn {
                pitch, instance, ..
            } => Some(Payload::NoteChange {
                pitch,
                pressure: DEFAULT_PRESSURE,
                timbre: DEFAULT_TIMBRE,
                slide: false,
                instance,
                from: pitch,
            }),
            _ => None,
        }
    }
}

/// An event as a host sends it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    pub sender: u32,
    pub channel: u32,
    pub payload: Payload,
}

/// Who sent an event: a host sender by number, or one of the program's
/// sequences by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sender {
    Host(u32),
    Seq(u16),
}

/// A declared event, by index into the program's declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId(pub u16);

/// `event NAME KIND(sender: S, channel: C)`. A filter left out matches
/// anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventDecl {
    pub name: String,
    pub kind: EventKind,
    pub sender: Option<Sender>,
    pub channel: Option<u32>,
}

impl EventDecl {
    /// Does an event from the host match?
    pub fn matches(&self, event: &Event) -> bool {
        self.accepts(
            Sender::Host(event.sender),
            event.channel,
            event.payload.kind(),
        )
    }

    /// Does an event of `kind` from `sender` on `channel` match?
    pub fn accepts(&self, sender: Sender, channel: u32, kind: EventKind) -> bool {
        self.kind == kind
            && self.sender.is_none_or(|s| s == sender)
            && self.channel.is_none_or(|c| c == channel)
    }
}

/// How an event reaches a program.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dispatch {
    /// Matched against every declaration; each match runs.
    Incoming(Event),
    /// Straight to one declaration, skipping its filters.
    To(EventId, Payload),
}

/// Turn a textual event into a [`Dispatch`], for command lines and scripts.
///
/// `target` is a kind (`note_on`), which makes an incoming event with
/// optional `sender` and `channel` fields, or the name of a declared event,
/// which is sent straight to it. The other fields are the payload's, by
/// name; a missing one is 0.
pub fn parse_dispatch(
    decls: &[EventDecl],
    target: &str,
    fields: &[(String, Value)],
) -> Result<Dispatch, String> {
    let (kind, to) = match EventKind::from_name(target) {
        Some(kind) => (kind, None),
        None => match decls.iter().position(|d| d.name == target) {
            Some(i) => (decls[i].kind, Some(EventId(i as u16))),
            None => {
                let mut known: Vec<&str> = EventKind::ALL.iter().map(|k| k.name()).collect();
                known.extend(decls.iter().map(|d| d.name.as_str()));
                return Err(format!(
                    "unknown event `{target}` (available: {})",
                    known.join(", ")
                ));
            }
        },
    };
    let mut values = [0.0f32; MAX_FIELDS];
    if kind == EventKind::NoteChange {
        values[1] = DEFAULT_PRESSURE;
        values[2] = DEFAULT_TIMBRE;
        values[5] = f32::NAN;
    }
    let (mut sender, mut channel) = (0u32, 0u32);
    for (name, value) in fields {
        // A pitch is a `Pitch`; every other field a plain number.
        let ty = match name.as_str() {
            "pitch" | "from" => Type::Pitch,
            _ => Type::Float,
        };
        let value = &value
            .resolve(Some(&ty))
            .map_err(|e| format!("`{name}`: {e}"))?;
        let whole = |what: &str| {
            if *value >= 0.0 && value.fract() == 0.0 && *value <= u32::MAX as f32 {
                Ok(*value as u32)
            } else {
                Err(format!("`{what}` must be a whole number ≥ 0, not {value}"))
            }
        };
        match name.as_str() {
            "sender" if to.is_none() => sender = whole("sender")?,
            "channel" if to.is_none() => channel = whole("channel")?,
            "sender" | "channel" => {
                return Err(format!(
                    "`{name}` cannot be set when sending straight to `{target}`; its filters are skipped"
                ));
            }
            field => match kind.fields().iter().position(|f| *f == field) {
                Some(i) => values[i] = *value,
                None => {
                    return Err(format!(
                        "a {kind} event has no field `{field}` (it has {})",
                        kind.fields().join(", ")
                    ));
                }
            },
        }
    }
    // The pitch the changed note is held at: its own pitch if left out (a
    // change of pressure or timbre only).
    if kind == EventKind::NoteChange && values[5].is_nan() {
        values[5] = values[0];
    }
    let payload = Payload::from_values(kind, values);
    Ok(match to {
        Some(id) => Dispatch::To(id, payload),
        None => Dispatch::Incoming(Event {
            sender,
            channel,
            payload,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decls() -> Vec<EventDecl> {
        vec![EventDecl {
            name: "keys".into(),
            kind: EventKind::NoteOn,
            sender: Some(Sender::Host(5)),
            channel: None,
        }]
    }

    fn fields(xs: &[(&str, f32)]) -> Vec<(String, Value)> {
        xs.iter()
            .map(|(n, v)| ((*n).to_owned(), Value::number(f64::from(*v))))
            .collect()
    }

    #[test]
    fn filters_match_sender_and_channel() {
        let d = &decls()[0];
        let note = |sender, channel| Event {
            sender,
            channel,
            payload: Payload::NoteOn {
                pitch: 0.0,
                velocity: 1.0,
                instance: 0,
            },
        };
        assert!(d.matches(&note(5, 0)));
        assert!(d.matches(&note(5, 9)));
        assert!(!d.matches(&note(4, 0)));
        let off = Event {
            payload: Payload::NoteOff {
                pitch: 0.0,
                release: 0.0,
                instance: 0,
            },
            ..note(5, 0)
        };
        assert!(!d.matches(&off), "kinds must match");
    }

    #[test]
    fn textual_events() {
        assert_eq!(
            parse_dispatch(
                &decls(),
                "note_on",
                &fields(&[("sender", 5.0), ("pitch", 69.0), ("velocity", 0.5)])
            ),
            Ok(Dispatch::Incoming(Event {
                sender: 5,
                channel: 0,
                payload: Payload::NoteOn {
                    pitch: 69.0,
                    velocity: 0.5,
                    instance: 0,
                },
            }))
        );
        assert_eq!(
            parse_dispatch(&decls(), "keys", &fields(&[("pitch", 60.0)])),
            Ok(Dispatch::To(
                EventId(0),
                Payload::NoteOn {
                    pitch: 60.0,
                    velocity: 0.0,
                    instance: 0,
                }
            ))
        );
        assert_eq!(
            parse_dispatch(&decls(), "note_on", &fields(&[("release", 1.0)])),
            Err("a note_on event has no field `release` (it has pitch, velocity, instance)".into())
        );
        assert!(parse_dispatch(&decls(), "keys", &fields(&[("channel", 1.0)])).is_err());
        assert!(parse_dispatch(&decls(), "nope", &[]).is_err());
        assert!(parse_dispatch(&decls(), "note_on", &fields(&[("sender", 1.5)])).is_err());
        // Fields are written as in the language: a pitch by name, and no
        // units on a plain number.
        let written = |f: &str, v: &str| vec![(f.to_owned(), Value::parse(v).unwrap())];
        assert_eq!(
            parse_dispatch(&decls(), "keys", &written("pitch", "A4")),
            parse_dispatch(&decls(), "keys", &fields(&[("pitch", 69.0)]))
        );
        let unit = parse_dispatch(&decls(), "keys", &written("velocity", "-6dB")).unwrap_err();
        assert!(unit.starts_with("`velocity`: "), "{unit}");
        // A change names the note it changes by `from`; without it, its
        // own pitch.
        let fields = |xs: &[(&str, &str)]| -> Vec<(String, Value)> {
            xs.iter()
                .map(|(f, v)| ((*f).to_owned(), Value::parse(v).unwrap()))
                .collect()
        };
        let change = |xs: &[(&str, &str)]| match parse_dispatch(&[], "note_change", &fields(xs)) {
            Ok(Dispatch::Incoming(Event { payload, .. })) => payload,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            change(&[("from", "C2"), ("pitch", "D2")]),
            Payload::NoteChange {
                pitch: 38.0,
                pressure: DEFAULT_PRESSURE,
                timbre: DEFAULT_TIMBRE,
                slide: false,
                instance: 0,
                from: 36.0,
            }
        );
        assert!(matches!(
            change(&[("pitch", "D2"), ("pressure", ".5")]),
            Payload::NoteChange {
                from: 38.0,
                pressure: 0.5,
                ..
            }
        ));
    }
}
