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
