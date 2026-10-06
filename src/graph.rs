//! Build-stage graph description.
//!
//! A [`Graph`] is a plain list of nodes and the wires between them. It may be
//! built on any thread and allocates freely; [`crate::Engine::new`] freezes it
//! into something the audio callback can run.

use crate::node::Node;

/// Handle to a node inside a [`Graph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) usize);

impl NodeId {
    /// Output channel `c` of this node.
    pub fn channel(self, c: usize) -> Input {
        Input::Port(self, c)
    }
}

/// What feeds a node input or a device output channel.
///
/// A constant is a stream that never changes, so every input accepts either.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    Const(f32),
    /// An output channel of a node.
    Port(NodeId, usize),
}

impl From<f32> for Input {
    fn from(value: f32) -> Self {
        Input::Const(value)
    }
}

impl From<NodeId> for Input {
    fn from(id: NodeId) -> Self {
        Input::Port(id, 0)
    }
}

pub(crate) struct Entry {
    pub(crate) node: Box<dyn Node>,
    pub(crate) inputs: Vec<Input>,
}

/// Where the graph sends audio.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Output {
    /// Nothing is routed to the device; it plays silence.
    None,
    /// One stream copied to every output channel.
    Mono(Input),
    /// One stream per output channel, in order.
    Channels(Vec<Input>),
}

/// A graph of stream processors under construction.
pub struct Graph {
    pub(crate) entries: Vec<Entry>,
    pub(crate) output: Output,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    pub fn new() -> Self {
        Graph {
            entries: Vec::new(),
            output: Output::None,
        }
    }

    /// Add `node`, wired to `inputs` in the order the node declares them.
    ///
    /// The input count is checked when the graph is frozen, not here, so a
    /// graph can be assembled in any order.
    pub fn node(
        &mut self,
        node: impl Node + 'static,
        inputs: impl IntoIterator<Item = Input>,
    ) -> NodeId {
        let id = NodeId(self.entries.len());
        self.entries.push(Entry {
            node: Box::new(node),
            inputs: inputs.into_iter().collect(),
        });
        id
    }

    /// Rewire input `index` of an existing node. This is the only way to
    /// close a loop, which [`crate::Engine::new`] will then reject until
    /// delays exist.
    pub fn set_input(&mut self, id: NodeId, index: usize, input: impl Into<Input>) {
        self.entries[id.0].inputs[index] = input.into();
    }

    /// Sine oscillator at `freq` Hz.
    pub fn sine(&mut self, freq: impl Into<Input>) -> NodeId {
        self.node(crate::nodes::Sine::new(), [freq.into()])
    }

    /// `x` scaled by `amount`.
    pub fn gain(&mut self, x: impl Into<Input>, amount: impl Into<Input>) -> NodeId {
        self.node(crate::nodes::Gain, [x.into(), amount.into()])
    }

    /// `a * b`. The same node as [`Graph::gain`]; the name reads better when
    /// neither side is a level.
    pub fn mul(&mut self, a: impl Into<Input>, b: impl Into<Input>) -> NodeId {
        self.gain(a, b)
    }

    /// `a + b`, which on audio streams is mixing.
    pub fn add(&mut self, a: impl Into<Input>, b: impl Into<Input>) -> NodeId {
        self.node(crate::nodes::Add, [a.into(), b.into()])
    }

    /// Send `x` to every output channel.
    pub fn out(&mut self, x: impl Into<Input>) {
        self.output = Output::Mono(x.into());
    }

    /// Send one stream per output channel. The count must match the engine's
    /// channel count.
    pub fn out_channels(&mut self, channels: impl IntoIterator<Item = Input>) {
        self.output = Output::Channels(channels.into_iter().collect());
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
