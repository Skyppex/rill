//! Events sent to the audio thread while it plays: a fixed ring with one
//! sender and one receiver, so neither side allocates, frees, locks or
//! waits.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::event::Dispatch;

struct Ring {
    slots: Box<[UnsafeCell<MaybeUninit<Dispatch>>]>,
    /// Events taken so far, by the receiver.
    read: AtomicUsize,
    /// Events put so far, by the sender.
    written: AtomicUsize,
}

// SAFETY: the sender writes a slot only once the receiver is done with it
// (it is `capacity` or more past `read`), and the receiver reads a slot only
// once the sender is done with it (it is before `written`). Each side is one
// value that needs `&mut` to use.
unsafe impl Sync for Ring {}

/// The sending half of [`channel`].
pub struct Sender(Arc<Ring>);

/// The receiving half of [`channel`], for the audio thread.
pub struct Receiver(Arc<Ring>);

/// A queue holding up to `capacity` events at a time.
pub fn channel(capacity: usize) -> (Sender, Receiver) {
    assert!(capacity > 0, "an inbox needs room for an event");
    let ring = Arc::new(Ring {
        slots: (0..capacity)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect(),
        read: AtomicUsize::new(0),
        written: AtomicUsize::new(0),
    });
    (Sender(ring.clone()), Receiver(ring))
}

impl Sender {
    /// Queue `dispatch`. False, and the event is dropped, if the queue is
    /// full.
    pub fn send(&mut self, dispatch: Dispatch) -> bool {
        let ring = &*self.0;
        let written = ring.written.load(Ordering::Relaxed);
        if written.wrapping_sub(ring.read.load(Ordering::Acquire)) == ring.slots.len() {
            return false;
        }
        // SAFETY: as on `Ring`; the queue is not full, so the receiver is
        // done with this slot.
        unsafe { (*ring.slots[written % ring.slots.len()].get()).write(dispatch) };
        ring.written.store(written.wrapping_add(1), Ordering::Release);
        true
    }
}

impl Receiver {
    /// The oldest event queued, if any.
    pub fn recv(&mut self) -> Option<Dispatch> {
        let ring = &*self.0;
        let read = ring.read.load(Ordering::Relaxed);
        if read == ring.written.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: as on `Ring`; the sender wrote this slot before moving
        // `written` past it.
        let dispatch = unsafe { (*ring.slots[read % ring.slots.len()].get()).assume_init_read() };
        ring.read.store(read.wrapping_add(1), Ordering::Release);
        Some(dispatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Payload};

    fn note(pitch: f32) -> Dispatch {
        Dispatch::Incoming(Event {
            sender: 0,
            channel: 0,
            payload: Payload::NoteOn {
                pitch,
                velocity: 1.0,
                instance: 0,
            },
        })
    }

    #[test]
    fn events_arrive_in_order_until_full() {
        let (mut tx, mut rx) = channel(2);
        assert!(tx.send(note(60.0)));
        assert!(tx.send(note(61.0)));
        assert!(!tx.send(note(62.0)), "full");
        assert_eq!(rx.recv(), Some(note(60.0)));
        assert!(tx.send(note(63.0)));
        assert_eq!(rx.recv(), Some(note(61.0)));
        assert_eq!(rx.recv(), Some(note(63.0)));
        assert_eq!(rx.recv(), None);
    }

    #[test]
    fn events_cross_threads() {
        let (mut tx, mut rx) = channel(4);
        let sending = std::thread::spawn(move || {
            for i in 0..1000 {
                while !tx.send(note(i as f32)) {
                    std::thread::yield_now();
                }
            }
        });
        let mut next = 0;
        while next < 1000 {
            match rx.recv() {
                Some(got) => {
                    assert_eq!(got, note(next as f32));
                    next += 1;
                }
                None => std::thread::yield_now(),
            }
        }
        sending.join().unwrap();
    }
}
