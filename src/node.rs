//! The run-stage interface every node implements.

/// Per-block information handed to every node.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    /// Host sample rate in Hz.
    pub sample_rate: f32,
    /// Frames in this block. Never more than the engine's `max_frames`.
    pub frames: usize,
    /// Absolute frame index of the first frame in this block.
    pub position: u64,
}

/// One input as seen from inside [`Node::process`].
#[derive(Clone, Copy, Debug)]
pub enum Signal<'a> {
    Const(f32),
    Buffer(&'a [f32]),
}

impl Signal<'_> {
    /// Value at frame `i` of the current block.
    #[inline(always)]
    pub fn at(&self, i: usize) -> f32 {
        match *self {
            Signal::Const(v) => v,
            Signal::Buffer(b) => b[i],
        }
    }

    pub fn as_const(&self) -> Option<f32> {
        match *self {
            Signal::Const(v) => Some(v),
            Signal::Buffer(_) => None,
        }
    }
}

/// Resolved source of one node input inside a frozen graph.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Port {
    Const(f32),
    /// Channel of a node's output: `buffers[node][offset..offset + frames]`.
    Buffer {
        node: usize,
        offset: usize,
    },
}

/// The inputs of the node currently being processed.
pub struct Inputs<'a> {
    pub(crate) ports: &'a [Port],
    pub(crate) buffers: &'a [Box<[f32]>],
    pub(crate) frames: usize,
}

impl<'a> Inputs<'a> {
    #[inline(always)]
    pub fn get(&self, index: usize) -> Signal<'a> {
        match self.ports[index] {
            Port::Const(v) => Signal::Const(v),
            Port::Buffer { node, offset } => {
                Signal::Buffer(&self.buffers[node][offset..offset + self.frames])
            }
        }
    }

    pub fn len(&self) -> usize {
        self.ports.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ports.is_empty()
    }
}

/// The output channels of the node currently being processed.
pub struct Outputs<'a> {
    pub(crate) buf: &'a mut [f32],
    /// Distance between channels in `buf` (the engine's `max_frames`).
    pub(crate) stride: usize,
    pub(crate) frames: usize,
}

impl Outputs<'_> {
    /// Channel `c`, exactly `frames` long.
    #[inline(always)]
    pub fn channel(&mut self, c: usize) -> &mut [f32] {
        let start = c * self.stride;
        &mut self.buf[start..start + self.frames]
    }

    /// Channel 0, for single-output nodes.
    #[inline(always)]
    pub fn mono(&mut self) -> &mut [f32] {
        self.channel(0)
    }

    /// Write frame `i` of channel `c`.
    #[inline(always)]
    pub fn set(&mut self, c: usize, i: usize, value: f32) {
        self.buf[c * self.stride + i] = value;
    }

    pub fn frames(&self) -> usize {
        self.frames
    }
}

/// A stream processor that runs inside the audio callback.
///
/// `process` must be real-time safe: no allocation, locks, syscalls or
/// unbounded loops. All state lives in the node itself and is created during
/// the build stage.
pub trait Node: Send {
    /// Name used in error messages.
    fn name(&self) -> &'static str;

    /// Number of inputs the node expects.
    fn inputs(&self) -> usize;

    /// Number of output channels.
    fn outputs(&self) -> usize {
        1
    }

    /// Fill every output channel (`ctx.frames` long) from `inputs`.
    fn process(&mut self, ctx: &Context, inputs: &Inputs, out: &mut Outputs);

    /// Return to the state the node had when it was built.
    fn reset(&mut self) {}
}
