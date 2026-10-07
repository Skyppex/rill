//! Events: what hosts send, and the declarations that accept them.
//!
//! Every incoming [`Event`] comes from a `sender` on a `channel` and carries
//! a [`Payload`] whose variant is its kind. What senders and channels mean is
//! up to the host; Rill only compares the numbers. A program declares the
//! events it handles ([`EventDecl`]), each one a kind with optional filters,
//! and the engine runs the handlers of every declaration an event matches.

use std::fmt;

/// The kinds of event a program can declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    NoteOn,
    NoteOff,
    ControlChange,
}

impl EventKind {
    pub const ALL: [EventKind; 3] = [
        EventKind::NoteOn,
        EventKind::NoteOff,
        EventKind::ControlChange,
    ];

    /// The keyword used in declarations, as in `event keys note_on`.
    pub fn name(self) -> &'static str {
        match self {
            EventKind::NoteOn => "note_on",
            EventKind::NoteOff => "note_off",
            EventKind::ControlChange => "control_change",
        }
    }

    pub fn from_name(name: &str) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Names of the payload's values, in the order a handler receives them.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            EventKind::NoteOn => &["pitch", "velocity"],
            EventKind::NoteOff => &["pitch", "release"],
            EventKind::ControlChange => &["value"],
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What happened. Pitches are Rill `Pitch` values, which count semitones
/// like MIDI note numbers (A4 is 69); velocities and releases are 0–1. A control change's value is passed on as it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Payload {
    NoteOn { pitch: f32, velocity: f32 },
    NoteOff { pitch: f32, release: f32 },
    Control(f32),
}

impl Payload {
    pub fn kind(&self) -> EventKind {
        match self {
            Payload::NoteOn { .. } => EventKind::NoteOn,
            Payload::NoteOff { .. } => EventKind::NoteOff,
            Payload::Control(_) => EventKind::ControlChange,
        }
    }

    /// The values in [`EventKind::fields`] order.
    pub fn values(&self) -> [f32; 2] {
        match *self {
            Payload::NoteOn { pitch, velocity } => [pitch, velocity],
            Payload::NoteOff { pitch, release } => [pitch, release],
            Payload::Control(value) => [value, 0.0],
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

/// A declared event, by index into the program's declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId(pub u16);

/// `event NAME KIND(sender: S, channel: C)`. A filter left out matches
/// anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventDecl {
    pub name: String,
    pub kind: EventKind,
    pub sender: Option<u32>,
    pub channel: Option<u32>,
}

impl EventDecl {
    pub fn matches(&self, event: &Event) -> bool {
        self.kind == event.payload.kind()
            && self.sender.is_none_or(|s| s == event.sender)
            && self.channel.is_none_or(|c| c == event.channel)
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
    fields: &[(String, f32)],
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
    let mut values = [0.0f32; 2];
    let (mut sender, mut channel) = (0u32, 0u32);
    for (name, value) in fields {
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
    let payload = match kind {
        EventKind::NoteOn => Payload::NoteOn {
            pitch: values[0],
            velocity: values[1],
        },
        EventKind::NoteOff => Payload::NoteOff {
            pitch: values[0],
            release: values[1],
        },
        EventKind::ControlChange => Payload::Control(values[0]),
    };
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
            sender: Some(5),
            channel: None,
        }]
    }

    fn fields(xs: &[(&str, f32)]) -> Vec<(String, f32)> {
        xs.iter().map(|(n, v)| ((*n).to_owned(), *v)).collect()
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
            },
        };
        assert!(d.matches(&note(5, 0)));
        assert!(d.matches(&note(5, 9)));
        assert!(!d.matches(&note(4, 0)));
        let off = Event {
            payload: Payload::NoteOff {
                pitch: 0.0,
                release: 0.0,
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
                    velocity: 0.5
                },
            }))
        );
        assert_eq!(
            parse_dispatch(&decls(), "keys", &fields(&[("pitch", 60.0)])),
            Ok(Dispatch::To(
                EventId(0),
                Payload::NoteOn {
                    pitch: 60.0,
                    velocity: 0.0
                }
            ))
        );
        assert_eq!(
            parse_dispatch(&decls(), "note_on", &fields(&[("release", 1.0)])),
            Err("a note_on event has no field `release` (it has pitch, velocity)".into())
        );
        assert!(parse_dispatch(&decls(), "keys", &fields(&[("channel", 1.0)])).is_err());
        assert!(parse_dispatch(&decls(), "nope", &[]).is_err());
        assert!(parse_dispatch(&decls(), "note_on", &fields(&[("sender", 1.5)])).is_err());
    }
}
