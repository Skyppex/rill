//! The run stage must not allocate. This test binary installs a counting
//! allocator and checks that rendering never touches it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rill::{Config, Engine, patches};

struct Counting;

thread_local! {
    // Only count on the thread under test; the harness allocates elsewhere.
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = TRACKING.try_with(|t| {
        if t.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn allocations_during(f: impl FnOnce()) -> usize {
    COUNT.with(|c| c.set(0));
    TRACKING.with(|t| t.set(true));
    f();
    TRACKING.with(|t| t.set(false));
    COUNT.with(|c| c.get())
}

#[test]
fn rendering_does_not_allocate() {
    let config = Config {
        sample_rate: 48_000,
        max_frames: 128,
        out_channels: 2,
    };
    let mut engine = Engine::new(patches::vibrato(440.0, 0.3), config).unwrap();
    let mut interleaved_f32 = vec![0.0f32; 2 * 1000];
    let mut interleaved_i16 = vec![0i16; 2 * 333];
    let mut left = vec![0.0f32; 700];
    let mut right = vec![0.0f32; 700];

    let count = allocations_during(|| {
        for _ in 0..10 {
            engine.render_interleaved(&mut interleaved_f32);
            engine.render_interleaved(&mut interleaved_i16);
            engine.render_planar(&mut [&mut left, &mut right]);
            engine.render_interleaved_with(&mut interleaved_f32, |x| x * 0.5);
        }
        engine.reset();
    });
    assert_eq!(count, 0);
    // Sanity check that the allocator is actually being observed.
    assert!(allocations_during(|| drop(std::hint::black_box(vec![0u8; 16]))) > 0);
}

#[test]
fn compiled_rill_programs_do_not_allocate() {
    for name in ["sketch", "stereo", "polyphony", "sequencer"] {
        let path = format!("{}/examples/{name}.rill", env!("CARGO_MANIFEST_DIR"));
        let src = std::fs::read_to_string(path).unwrap();
        let config = Config {
            sample_rate: 48_000,
            max_frames: 64,
            out_channels: 2,
        };
        let (graph, _) = rill::lang::load(&src, &config, "main").unwrap();
        let mut engine = Engine::new(graph, config).unwrap();
        let mut out = vec![0.0f32; 2 * 1000];
        let count = allocations_during(|| {
            for _ in 0..5 {
                engine.render_interleaved(&mut out);
            }
            engine.reset();
        });
        assert_eq!(count, 0, "{name}");
        assert!(out.iter().any(|&x| x != 0.0), "{name} rendered silence");
    }
}

#[test]
fn matching_and_handling_events_does_not_allocate() {
    use rill::{Dispatch, Event, Payload, RillEvent};
    let path = format!("{}/examples/live_control.rill", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(path).unwrap();
    let config = Config {
        sample_rate: 48_000,
        max_frames: 64,
        out_channels: 2,
    };
    let (graph, _) = rill::lang::load(&src, &config, "main").unwrap();
    let mut engine = Engine::new(graph, config).unwrap();
    let press = Event {
        sender: 1,
        channel: 3,
        payload: Payload::NoteOn {
            pitch: 64.0,
            velocity: 0.8,
            instance: 0,
        },
    };
    let brightness = Event {
        sender: 1,
        channel: 74,
        payload: Payload::Control(0.5),
    };
    let events = [
        RillEvent {
            frame_offset: 10,
            dispatch: Dispatch::Incoming(press),
        },
        RillEvent {
            frame_offset: 500,
            dispatch: Dispatch::Incoming(brightness),
        },
    ];
    let mut out = vec![0.0f32; 2 * 1000];
    let count = allocations_during(|| {
        engine.render_interleaved_with_rill_events(&mut out, |x| x, &events);
        engine.send(&press);
    });
    assert_eq!(count, 0);
}

#[test]
fn playing_sequences_does_not_allocate() {
    use rill::{Event, Payload};
    let path = format!("{}/examples/sequencer.rill", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(path).unwrap();
    let config = Config {
        sample_rate: 48_000,
        max_frames: 256,
        out_channels: 2,
    };
    let (graph, _) = rill::lang::load(&src, &config, "main").unwrap();
    let mut engine = Engine::new(graph, config).unwrap();
    let pad = Event {
        sender: 2,
        channel: 10,
        payload: Payload::NoteOn {
            pitch: 60.0,
            velocity: 1.0,
            instance: 0,
        },
    };
    let mut out = vec![0.0f32; 2 * 48_000];
    let count = allocations_during(|| {
        // Four seconds: every step, the loop around, and a restart.
        for second in 0..4 {
            if second == 2 {
                engine.send(&pad);
            }
            engine.render_interleaved(&mut out);
        }
    });
    assert_eq!(count, 0);
    assert!(out.iter().any(|&x| x != 0.0));
}

#[test]
fn every_sequence_event_without_allocating() {
    use rill::{Event, Payload};
    // Every kind handled; chords held while instances are halted, restarted
    // and replaced, many at a time.
    let src = "
        seq riff(step: 1/16, tempo: 600bpm, gate: 1, repeat: 3, instances: 4) {
            [C4, E4, G4], _, [D4, F4, A4], C5,
        }
        event go control_change(sender: 1)
        rill voice() Sample {
            state level: Float = 0
            on riff_note_on(n) claim { level = n.velocity }
            on riff_note_off release { level = 0 }
            return level
        }
        rill main() [Sample; 2] {
            state count: Float = 0
            on start { invoke 1 riff }
            on go(v) {
                if v == 1 { invoke riff; invoke riff; invoke riff; invoke riff; invoke riff }
                if v == 2 { trigger 3 1 riff }
                if v == 3 { halt riff }
            }
            on riff_start(s) { count = count + s.step as Float }
            on riff_finished { invoke riff }
            on riff_halted { count = count + 1 }
            on riff_replaced { count = count + 1 }
            on riff_end { count = count + 1 }
            on riff_repeated(r) { count = count + r.pass as Float }
            on riff_step(s) { count = count + s.step as Float }
            on riff_rest { count = count + 1 }
            on riff_beat(b) { count = count + b.beat as Float }
            on riff_bar(b) { count = count + b.bar as Float }
            let mix = sum([voice(); 6])
            return [mix, count]
        }
    ";
    let config = Config {
        sample_rate: 48_000,
        max_frames: 256,
        out_channels: 2,
    };
    let (graph, _) = rill::lang::load(src, &config, "main").unwrap();
    let mut engine = Engine::new(graph, config).unwrap();
    let go = |v| Event {
        sender: 1,
        channel: 0,
        payload: Payload::Control(v),
    };
    let mut out = vec![0.0f32; 2 * 4800];
    // Warm up: let everything happen once.
    engine.render_interleaved(&mut out);
    let count = allocations_during(|| {
        for round in 0..20 {
            engine.send(&go(1.0 + (round % 3) as f32));
            engine.render_interleaved(&mut out);
        }
    });
    assert_eq!(count, 0);
    assert!(out.chunks(2).any(|f| f[1] > 0.0));
}
