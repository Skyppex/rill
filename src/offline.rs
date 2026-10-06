//! A fake audio callback loop, for tests and rendering to files.

use crate::engine::Engine;

/// How a simulated host slices time into callbacks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocks {
    /// Every callback asks for the same number of frames.
    Fixed(usize),
    /// Callback sizes repeat this list.
    Pattern(Vec<usize>),
    /// Sizes drawn uniformly from `min..=max` by a seeded generator, so a
    /// failing run can be reproduced.
    Random { min: usize, max: usize, seed: u64 },
}

impl Blocks {
    fn sizes(&self) -> impl Iterator<Item = usize> + '_ {
        let mut i = 0;
        let mut state = match self {
            Blocks::Random { seed, .. } => seed | 1,
            _ => 0,
        };
        std::iter::from_fn(move || {
            let n = match self {
                Blocks::Fixed(n) => *n,
                Blocks::Pattern(p) => p[i % p.len()],
                Blocks::Random { min, max, .. } => {
                    // xorshift64
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    min + (state % (max - min + 1) as u64) as usize
                }
            };
            i += 1;
            Some(n)
        })
    }
}

/// Run `engine` for `frames` frames, calling it the way a host would, and
/// return the interleaved result.
///
/// Zero-sized callbacks are allowed; real hosts produce them.
pub fn render(engine: &mut Engine, frames: usize, blocks: &Blocks) -> Vec<f32> {
    if let Blocks::Pattern(p) = blocks {
        assert!(p.iter().any(|&n| n > 0), "pattern never advances");
    }
    if let Blocks::Fixed(n) = blocks {
        assert!(*n > 0, "fixed block size must be > 0");
    }
    if let Blocks::Random { min, max, .. } = blocks {
        assert!(min <= max && *max > 0, "bad random block range");
    }

    let channels = engine.config().out_channels;
    let mut out = vec![0.0; frames * channels];
    let mut done = 0;
    for n in blocks.sizes() {
        if done == frames {
            break;
        }
        let n = n.min(frames - done);
        engine.render_interleaved(&mut out[done * channels..(done + n) * channels]);
        done += n;
    }
    out
}
