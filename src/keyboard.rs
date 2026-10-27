//! Playing notes from the computer's keys, laid out as FL Studio's typing
//! keyboard: the bottom row is the white keys from C4 (`z` `x` `c` ...)
//! with the black keys on the row above it (`s` `d`, `g` `h` `j`), and the
//! top two rows are the same from C5 (`q` `w` `e` ..., `2` `3`, `5` `6` `7`).
//!
//! Terminals send a key when it goes down, and again while it is held, but
//! most never say when it comes up. Those that speak the kitty keyboard
//! protocol do, and a [`Session`] asks for it; elsewhere a note ends when
//! its key stops repeating.

use std::time::{Duration, Instant};

use crate::event::{Dispatch, Event, Payload};

/// The bottom two rows, from C4, and the top two, from C5, a semitone a key.
const ROWS: [(&str, u8); 2] = [("zsxdcvgbhnjm,l.;/", 60), ("q2w3er5t6y7ui9o0p[=]", 72)];

/// FL Studio's: 100 and 64 of 127.
const VELOCITY: f32 = 100.0 / 127.0;
const RELEASE: f32 = 64.0 / 127.0;

/// Without word from the terminal that a key is up, how long it may go
/// quiet before its note ends: first long enough to wait out the delay
/// before a held key starts repeating, then the gap between repeats.
const FIRST_REPEAT: Duration = Duration::from_millis(600);
const NEXT_REPEAT: Duration = Duration::from_millis(150);

/// The pitch a key plays (C4 is 60), by the character it types without
/// Shift.
pub fn pitch(key: char) -> Option<f32> {
    ROWS.iter().find_map(|(row, from)| Some(f32::from(*from) + row.find(key)? as f32))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Press,
    Repeat,
    Release,
}

/// What a terminal sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// A key, by the character it types without Shift.
    Key(char, Kind),
    /// Enter, Ctrl-C or Ctrl-D.
    Stop,
    /// The answer to asking which kitty keyboard flags are on: the terminal
    /// speaks the protocol.
    Flags,
    /// The answer to asking what the terminal is, which every terminal
    /// gives, after any other answer.
    Attributes,
}

/// Read what a terminal sent from the start of `bytes` into `out`. Returns
/// how many bytes were used; the rest are the start of an escape sequence,
/// to read again with what comes after.
pub fn parse(bytes: &[u8], out: &mut Vec<Input>) -> usize {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => match bytes.get(i + 1) {
                None => return i,
                Some(b'[') => {
                    let rest = &bytes[i + 2..];
                    match rest.iter().position(|b| (0x40..=0x7e).contains(b)) {
                        Some(end) => {
                            out.extend(csi(&rest[..end], rest[end]));
                            i += end + 3;
                        }
                        // Not a sequence after all; give up on it.
                        None if rest.len() > 32 => i += 1,
                        None => return i,
                    }
                }
                // Arrows and F1 to F4 on some terminals.
                Some(b'O') if i + 2 < bytes.len() => i += 3,
                Some(b'O') => return i,
                // Alt with a key, or Esc on its own.
                Some(_) => i += 1,
            },
            b'\r' | b'\n' | 0x03 | 0x04 => {
                out.push(Input::Stop);
                i += 1;
            }
            b => {
                if b.is_ascii_graphic() {
                    out.push(Input::Key(char::from(b.to_ascii_lowercase()), Kind::Press));
                }
                i += 1;
            }
        }
    }
    i
}

/// A control sequence: `ESC [`, `params`, then `last`.
fn csi(params: &[u8], last: u8) -> Option<Input> {
    let params = std::str::from_utf8(params).ok()?;
    match (params.strip_prefix('?'), last) {
        (Some(_), b'u') => Some(Input::Flags),
        (Some(_), b'c') => Some(Input::Attributes),
        (None, b'u') => kitty_key(params),
        _ => None,
    }
}

/// A key in the kitty keyboard protocol: `code[:...][;mods[:kind][;...]]`.
fn kitty_key(params: &str) -> Option<Input> {
    let mut fields = params.split(';');
    let code: u32 = fields.next()?.split(':').next()?.parse().ok()?;
    let (mods, kind) = match fields.next() {
        None => (0, Kind::Press),
        Some(field) => {
            let mut parts = field.split(':');
            let mods = match parts.next() {
                None | Some("") => 0,
                Some(m) => m.parse::<u32>().ok()?.saturating_sub(1),
            };
            let kind = match parts.next() {
                None | Some("1") => Kind::Press,
                Some("2") => Kind::Repeat,
                Some("3") => Kind::Release,
                Some(_) => return None,
            };
            (mods, kind)
        }
    };
    const CTRL: u32 = 4;
    // Alt, Ctrl, Super, Hyper and Meta; not Shift, Caps Lock or Num Lock.
    const HELD: u32 = 2 | CTRL | 8 | 16 | 32;
    let key = char::from_u32(code)?;
    // Enter, and Enter on the keypad.
    let stops = matches!(code, 13 | 57414) || (mods & HELD == CTRL && matches!(key, 'c' | 'd'));
    match (stops, kind) {
        (true, Kind::Press) => Some(Input::Stop),
        (true, _) => None,
        (false, _) if mods & HELD != 0 => None,
        (false, kind) => Some(Input::Key(key, kind)),
    }
}

/// Which notes are held, turning keys into `note_on` and `note_off` events
/// from sender 0 on channel 0.
pub struct Keyboard {
    /// Whether the terminal says when a key comes up.
    releases: bool,
    held: Vec<Held>,
}

struct Held {
    key: char,
    pitch: f32,
    /// When the key was last seen going down or repeating.
    seen: Instant,
    repeating: bool,
}

impl Keyboard {
    pub fn new(releases: bool) -> Keyboard {
        Keyboard {
            releases,
            held: Vec::new(),
        }
    }

    /// The note a key starts or ends, at `now`, if any.
    pub fn key(&mut self, key: char, kind: Kind, now: Instant) -> Option<Dispatch> {
        let pitch = pitch(key)?;
        let held = self.held.iter().position(|h| h.key == key);
        match (kind, held) {
            (Kind::Press, None) => {
                self.held.push(Held {
                    key,
                    pitch,
                    seen: now,
                    repeating: false,
                });
                Some(note(Payload::NoteOn {
                    pitch,
                    velocity: VELOCITY,
                    instance: 0,
                }))
            }
            // Without releases, a key held down comes again as a press.
            (Kind::Press | Kind::Repeat, Some(i)) => {
                self.held[i].seen = now;
                self.held[i].repeating = true;
                None
            }
            (Kind::Release, Some(i)) => Some(off(self.held.swap_remove(i).pitch)),
            (Kind::Repeat | Kind::Release, None) => None,
        }
    }

    /// When the terminal does not say when keys come up: end the notes
    /// whose keys have gone quiet by `now`.
    pub fn expire(&mut self, now: Instant, mut send: impl FnMut(Dispatch)) {
        if self.releases {
            return;
        }
        self.held.retain(|h| {
            let wait = if h.repeating { NEXT_REPEAT } else { FIRST_REPEAT };
            let down = now.duration_since(h.seen) < wait;
            if !down {
                send(off(h.pitch));
            }
            down
        });
    }
}

fn note(payload: Payload) -> Dispatch {
    Dispatch::Incoming(Event {
        sender: 0,
        channel: 0,
        payload,
    })
}

fn off(pitch: f32) -> Dispatch {
    note(Payload::NoteOff {
        pitch,
        release: RELEASE,
        instance: 0,
    })
}

#[cfg(unix)]
pub use terminal::Session;

#[cfg(unix)]
mod terminal {
    use std::io::{self, IsTerminal, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use super::{Input, Keyboard, parse};
    use crate::inbox;

    /// Asks for keys to come as escape sequences, with releases: the kitty
    /// keyboard protocol's flags 1 (disambiguate), 2 (event types) and 8
    /// (all keys as escapes).
    const PUSH_FLAGS: &[u8] = b"\x1b[>11u";
    const POP_FLAGS: &[u8] = b"\x1b[<u";

    /// Notes playing from the terminal on stdin. The terminal is back as it
    /// was once this is dropped.
    pub struct Session {
        quit: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        terminal: Raw,
    }

    impl Session {
        /// Read keys from the terminal on stdin, sending notes to `events`,
        /// until Enter, Ctrl-C or Ctrl-D, which call `stop`.
        pub fn start(
            mut events: inbox::Sender,
            stop: impl FnOnce() + Send + 'static,
        ) -> io::Result<Session> {
            let terminal = Raw::new()?;
            let mut keyboard = Keyboard::new(terminal.kitty);
            let quit = Arc::new(AtomicBool::new(false));
            let thread = std::thread::spawn({
                let quit = quit.clone();
                move || {
                    let mut pending = Vec::new();
                    let mut inputs = Vec::new();
                    while !quit.load(Ordering::Relaxed) {
                        if read(&mut pending, Duration::from_millis(10)).is_err() {
                            stop();
                            return;
                        }
                        let used = parse(&pending, &mut inputs);
                        pending.drain(..used);
                        let now = Instant::now();
                        for input in inputs.drain(..) {
                            match input {
                                Input::Stop => {
                                    stop();
                                    return;
                                }
                                Input::Key(key, kind) => {
                                    if let Some(note) = keyboard.key(key, kind, now) {
                                        events.send(note);
                                    }
                                }
                                Input::Flags | Input::Attributes => {}
                            }
                        }
                        keyboard.expire(now, |note| {
                            events.send(note);
                        });
                    }
                }
            });
            Ok(Session {
                quit,
                thread: Some(thread),
                terminal,
            })
        }

        /// Whether the terminal says when a key comes up; if not, a note
        /// ends when its key stops repeating.
        pub fn releases(&self) -> bool {
            self.terminal.kitty
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.quit.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The terminal on stdin with keys passed straight through, not echoed,
    /// and Ctrl-C as a key; as it was again once dropped.
    struct Raw {
        saved: libc::termios,
        /// Whether keys come by the kitty keyboard protocol.
        kitty: bool,
    }

    impl Raw {
        fn new() -> io::Result<Raw> {
            // SAFETY: `tcgetattr` fills in the struct it is given, which
            // starts zeroed (a valid `termios`).
            let saved = unsafe {
                let mut saved: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                    return Err(io::Error::last_os_error());
                }
                saved
            };
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            raw.c_iflag &= !(libc::IXON | libc::ICRNL);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            // SAFETY: `raw` is a valid `termios`, from `tcgetattr`.
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut terminal = Raw {
                saved,
                kitty: false,
            };
            terminal.kitty = io::stdout().is_terminal() && speaks_kitty().unwrap_or(false);
            if terminal.kitty {
                let mut out = io::stdout();
                out.write_all(PUSH_FLAGS)?;
                out.flush()?;
            }
            Ok(terminal)
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            if self.kitty {
                let mut out = io::stdout();
                let _ = out.write_all(POP_FLAGS).and_then(|()| out.flush());
            }
            // SAFETY: `saved` is a valid `termios`, from `tcgetattr`.
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
        }
    }

    /// Ask the terminal for its kitty keyboard flags, then what it is. One
    /// that speaks the protocol should answer the first before the second,
    /// but some answer in the other order (WezTerm through ConPTY on WSL),
    /// so the flags have a moment more once it has said what it is.
    fn speaks_kitty() -> io::Result<bool> {
        let mut out = io::stdout();
        out.write_all(b"\x1b[?u\x1b[c")?;
        out.flush()?;
        let mut deadline = Instant::now() + Duration::from_millis(500);
        let (mut pending, mut inputs) = (Vec::new(), Vec::new());
        let mut answered = false;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            read(&mut pending, left)?;
            let used = parse(&pending, &mut inputs);
            pending.drain(..used);
            for input in inputs.drain(..) {
                match input {
                    Input::Flags => return Ok(true),
                    Input::Attributes if !answered => {
                        answered = true;
                        deadline = deadline.min(Instant::now() + Duration::from_millis(100));
                    }
                    _ => {}
                }
            }
        }
        Ok(false)
    }

    /// Append what stdin has to `to`, waiting up to `wait` for something.
    /// An error once stdin is closed.
    fn read(to: &mut Vec<u8>, wait: Duration) -> io::Result<()> {
        let mut poll = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = wait.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `poll` is one valid `pollfd`.
        match unsafe { libc::poll(&mut poll, 1, millis) } {
            0 => return Ok(()),
            n if n < 0 => {
                let err = io::Error::last_os_error();
                return match err.kind() {
                    io::ErrorKind::Interrupted => Ok(()),
                    _ => Err(err),
                };
            }
            _ => {}
        }
        let mut buf = [0u8; 256];
        // SAFETY: `buf` has room for the `buf.len()` bytes asked for.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        match n {
            0 => Err(io::ErrorKind::UnexpectedEof.into()),
            n if n < 0 => Err(io::Error::last_os_error()),
            n => {
                to.extend_from_slice(&buf[..n as usize]);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(bytes: &[u8]) -> (Vec<Input>, usize) {
        let mut out = Vec::new();
        let used = parse(bytes, &mut out);
        (out, used)
    }

    #[test]
    fn the_rows_are_laid_out_as_in_fl_studio() {
        assert_eq!(pitch('z'), Some(60.0));
        assert_eq!(pitch('s'), Some(61.0));
        assert_eq!(pitch('m'), Some(71.0));
        assert_eq!(pitch(','), Some(72.0));
        assert_eq!(pitch('/'), Some(76.0));
        assert_eq!(pitch('q'), Some(72.0));
        assert_eq!(pitch('y'), Some(81.0), "A5");
        assert_eq!(pitch(']'), Some(91.0));
        assert_eq!(pitch('a'), None);
        assert_eq!(pitch('4'), None, "no black key between E and F");
    }

    #[test]
    fn plain_keys_are_presses() {
        let (got, used) = inputs(b"zQ\x1b[A\r");
        assert_eq!(
            got,
            [
                Input::Key('z', Kind::Press),
                Input::Key('q', Kind::Press),
                Input::Stop,
            ]
        );
        assert_eq!(used, 6);
        assert_eq!(inputs(b"\x03"), (vec![Input::Stop], 1));
    }

    #[test]
    fn a_sequence_split_between_reads_waits_for_the_rest() {
        assert_eq!(inputs(b"z\x1b[12"), (vec![Input::Key('z', Kind::Press)], 1));
        assert_eq!(inputs(b"\x1b"), (vec![], 0));
    }

    #[test]
    fn kitty_keys_say_when_they_come_up() {
        // Shift comes as a modifier on the key it types without Shift.
        let (got, _) = inputs(b"\x1b[122u\x1b[122;1:2u\x1b[122;1:3u\x1b[122;2u");
        assert_eq!(
            got,
            [
                Input::Key('z', Kind::Press),
                Input::Key('z', Kind::Repeat),
                Input::Key('z', Kind::Release),
                Input::Key('z', Kind::Press),
            ]
        );
        // Enter and Ctrl-C stop when they go down; Ctrl with a note key
        // plays nothing.
        assert_eq!(inputs(b"\x1b[13u").0, [Input::Stop]);
        assert!(inputs(b"\x1b[13;1:3u").0.is_empty());
        assert_eq!(inputs(b"\x1b[99;5u").0, [Input::Stop]);
        assert!(inputs(b"\x1b[122;5u").0.is_empty());
        // Caps Lock is no reason to ignore a key.
        assert_eq!(inputs(b"\x1b[122;65u").0, [Input::Key('z', Kind::Press)]);
        assert_eq!(
            inputs(b"\x1b[?0u\x1b[?62;22c").0,
            [Input::Flags, Input::Attributes]
        );
    }

    #[test]
    fn notes_end_when_keys_come_up() {
        let now = Instant::now();
        let mut keys = Keyboard::new(true);
        assert!(matches!(
            keys.key('z', Kind::Press, now),
            Some(Dispatch::Incoming(Event {
                payload: Payload::NoteOn { pitch: 60.0, .. },
                ..
            }))
        ));
        assert_eq!(keys.key('z', Kind::Repeat, now), None);
        keys.expire(now + Duration::from_secs(5), |_| panic!("it says"));
        assert_eq!(keys.key('z', Kind::Release, now), Some(off(60.0)));
        assert_eq!(keys.key('z', Kind::Release, now), None);
        assert_eq!(keys.key('a', Kind::Press, now), None);
    }

    #[test]
    fn without_releases_notes_end_when_keys_stop_repeating() {
        let start = Instant::now();
        let t = |ms| start + Duration::from_millis(ms);
        let ended = |keys: &mut Keyboard, at| {
            let mut out = Vec::new();
            keys.expire(at, |d| out.push(d));
            out
        };
        let mut keys = Keyboard::new(false);
        assert!(keys.key('z', Kind::Press, t(0)).is_some());
        // Held through the wait before the first repeat.
        assert!(ended(&mut keys, t(500)).is_empty());
        assert_eq!(keys.key('z', Kind::Press, t(550)), None);
        assert!(ended(&mut keys, t(650)).is_empty());
        assert_eq!(ended(&mut keys, t(750)), [off(60.0)]);
        // Let go before it repeats.
        assert!(keys.key('x', Kind::Press, t(1000)).is_some());
        assert_eq!(ended(&mut keys, t(1700)), [off(62.0)]);
    }
}
