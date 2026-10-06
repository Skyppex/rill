//! Freezing a [`Graph`] and running it.

use std::fmt;

use crate::denormal::FlushDenormals;
use crate::format::OutSample;
use crate::graph::{Graph, Input, Output};
use crate::node::{Context, Inputs, Node, Port};

/// What the host promises the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub sample_rate: u32,
    /// Largest block processed in one pass. Bigger host callbacks are split,
    /// so this only bounds memory, never what the host may ask for.
    pub max_frames: usize,
    pub out_channels: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sample_rate: 48_000,
            max_frames: 1024,
            out_channels: 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    InvalidConfig(&'static str),
    /// A node was wired with the wrong number of inputs.
    Arity {
        node: &'static str,
        expected: usize,
        got: usize,
    },
    /// An input refers to a node that is not in this graph.
    UnknownNode(usize),
    /// `out_channels` got a different number of streams than the engine has.
    ChannelMismatch {
        expected: usize,
        got: usize,
    },
    /// The graph has a loop. Lists the node names along it.
    Cycle(Vec<&'static str>),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::InvalidConfig(why) => write!(f, "invalid config: {why}"),
            BuildError::Arity {
                node,
                expected,
                got,
            } => write!(f, "`{node}` takes {expected} input(s) but was given {got}"),
            BuildError::UnknownNode(i) => write!(f, "input refers to unknown node #{i}"),
            BuildError::ChannelMismatch { expected, got } => {
                write!(
                    f,
                    "graph outputs {got} channel(s) but the engine has {expected}"
                )
            }
            BuildError::Cycle(names) => write!(
                f,
                "cycle without a delay: {} -> {}",
                names.join(" -> "),
                names[0]
            ),
        }
    }
}

impl std::error::Error for BuildError {}

/// A frozen graph, ready to run on the audio thread.
///
/// Everything is allocated in [`Engine::new`]; the render methods never
/// allocate, lock or make syscalls.
pub struct Engine {
    config: Config,
    /// Live nodes in topological order.
    nodes: Vec<Box<dyn Node>>,
    /// `ports[port_ranges[i].0..port_ranges[i].1]` are node `i`'s inputs.
    port_ranges: Vec<(usize, usize)>,
    ports: Vec<Port>,
    /// One block-sized buffer per node, same order as `nodes`.
    buffers: Vec<Box<[f32]>>,
    /// Source for each output channel.
    outputs: Vec<Port>,
    position: u64,
}

impl Engine {
    /// The build stage's last step: check the graph, drop nodes that do not
    /// reach an output, sort the rest and allocate every buffer.
    pub fn new(graph: Graph, config: Config) -> Result<Engine, BuildError> {
        if config.sample_rate == 0 {
            return Err(BuildError::InvalidConfig("sample_rate must be > 0"));
        }
        if config.max_frames == 0 {
            return Err(BuildError::InvalidConfig("max_frames must be > 0"));
        }
        if config.out_channels == 0 {
            return Err(BuildError::InvalidConfig("out_channels must be > 0"));
        }

        let Graph { entries, output } = graph;

        for entry in &entries {
            let expected = entry.node.inputs();
            if entry.inputs.len() != expected {
                return Err(BuildError::Arity {
                    node: entry.node.name(),
                    expected,
                    got: entry.inputs.len(),
                });
            }
            for input in &entry.inputs {
                if let Input::Node(id) = input
                    && id.0 >= entries.len()
                {
                    return Err(BuildError::UnknownNode(id.0));
                }
            }
        }

        let outputs: Vec<Input> = match output {
            Output::None => vec![Input::Const(0.0); config.out_channels],
            Output::Mono(x) => vec![x; config.out_channels],
            Output::Channels(chs) => {
                if chs.len() != config.out_channels {
                    return Err(BuildError::ChannelMismatch {
                        expected: config.out_channels,
                        got: chs.len(),
                    });
                }
                chs
            }
        };
        for input in &outputs {
            if let Input::Node(id) = input
                && id.0 >= entries.len()
            {
                return Err(BuildError::UnknownNode(id.0));
            }
        }

        let order = schedule(&entries, &outputs)?;

        // Old graph index -> position in `order`, which is also the buffer index.
        let mut slot = vec![usize::MAX; entries.len()];
        for (pos, &i) in order.iter().enumerate() {
            slot[i] = pos;
        }
        let resolve = |input: &Input| match *input {
            Input::Const(v) => Port::Const(v),
            Input::Node(id) => Port::Buffer(slot[id.0]),
        };

        let mut ports = Vec::new();
        let mut port_ranges = Vec::with_capacity(order.len());
        for &i in &order {
            let start = ports.len();
            ports.extend(entries[i].inputs.iter().map(resolve));
            port_ranges.push((start, ports.len()));
        }
        let outputs = outputs.iter().map(resolve).collect();

        let mut entries: Vec<Option<_>> = entries.into_iter().map(Some).collect();
        let nodes = order
            .iter()
            .map(|&i| entries[i].take().expect("scheduled twice").node)
            .collect::<Vec<_>>();

        let buffers = (0..nodes.len())
            .map(|_| vec![0.0; config.max_frames].into_boxed_slice())
            .collect();

        Ok(Engine {
            config,
            nodes,
            port_ranges,
            ports,
            buffers,
            outputs,
            position: 0,
        })
    }

    pub fn config(&self) -> Config {
        self.config
    }

    /// Frames rendered since creation or the last [`Engine::reset`].
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Nodes that survived dead-node removal.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Bytes owned by the frozen graph. Fixed for the engine's lifetime, so a
    /// host can size its arena from it.
    pub fn memory_bytes(&self) -> usize {
        use std::mem::size_of;
        let buffers = self.buffers.len() * self.config.max_frames * size_of::<f32>();
        let state: usize = self.nodes.iter().map(|n| size_of_val(&**n)).sum();
        let wiring = self.ports.len() * size_of::<Port>()
            + self.port_ranges.len() * size_of::<(usize, usize)>()
            + self.outputs.len() * size_of::<Port>();
        size_of::<Self>() + buffers + state + wiring
    }

    /// Put every node back in its initial state and rewind the clock.
    pub fn reset(&mut self) {
        for node in &mut self.nodes {
            node.reset();
        }
        self.position = 0;
    }

    /// Render into planar output, one slice per channel. All slices must be
    /// the same length; that length is the frame count.
    pub fn render_planar(&mut self, out: &mut [&mut [f32]]) {
        assert_eq!(out.len(), self.config.out_channels, "channel count");
        let frames = out.first().map_or(0, |c| c.len());
        assert!(
            out.iter().all(|c| c.len() == frames),
            "channels differ in length"
        );

        let _ftz = FlushDenormals::new();
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(self.config.max_frames);
            self.process_block(n);
            for (channel, port) in out.iter_mut().zip(&self.outputs) {
                let dst = &mut channel[done..done + n];
                match *port {
                    Port::Const(v) => dst.fill(v),
                    Port::Buffer(b) => dst.copy_from_slice(&self.buffers[b][..n]),
                }
            }
            done += n;
        }
    }

    /// Render into an interleaved host buffer, converting to `T`. Trailing
    /// samples that do not make up a whole frame are zeroed.
    pub fn render_interleaved<T: OutSample>(&mut self, out: &mut [T]) {
        self.render_interleaved_with(out, T::from_f32);
    }

    /// [`Engine::render_interleaved`] with a caller-supplied conversion, for
    /// sample types this crate does not know about.
    pub fn render_interleaved_with<T: Copy>(&mut self, out: &mut [T], convert: impl Fn(f32) -> T) {
        let channels = self.config.out_channels;
        let frames = out.len() / channels;
        out[frames * channels..].fill(convert(0.0));

        let _ftz = FlushDenormals::new();
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(self.config.max_frames);
            self.process_block(n);
            let dst = &mut out[done * channels..(done + n) * channels];
            for (c, port) in self.outputs.iter().enumerate() {
                match *port {
                    Port::Const(v) => {
                        let v = convert(v);
                        for frame in dst.chunks_exact_mut(channels) {
                            frame[c] = v;
                        }
                    }
                    Port::Buffer(b) => {
                        for (frame, &x) in dst.chunks_exact_mut(channels).zip(&self.buffers[b][..n])
                        {
                            frame[c] = convert(x);
                        }
                    }
                }
            }
            done += n;
        }
    }

    /// Run every node over `n <= max_frames` frames.
    fn process_block(&mut self, n: usize) {
        let ctx = Context {
            sample_rate: self.config.sample_rate as f32,
            frames: n,
            position: self.position,
        };
        for (i, node) in self.nodes.iter_mut().enumerate() {
            // Take the node's own buffer out so the inputs can borrow the rest.
            // A node never reads itself (cycles are rejected), and swapping in
            // an empty box does not allocate.
            let mut out = std::mem::take(&mut self.buffers[i]);
            let (start, end) = self.port_ranges[i];
            let inputs = Inputs {
                ports: &self.ports[start..end],
                buffers: &self.buffers,
                frames: n,
            };
            node.process(&ctx, &inputs, &mut out[..n]);
            self.buffers[i] = out;
        }
        self.position += n as u64;
    }
}

/// Topological order of the nodes reachable from `outputs`, or the first
/// cycle found.
fn schedule(entries: &[crate::graph::Entry], outputs: &[Input]) -> Result<Vec<usize>, BuildError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }

    let mut mark = vec![Mark::New; entries.len()];
    let mut order = Vec::with_capacity(entries.len());
    // Explicit stack of (node, next input to visit) so deep chains cannot
    // overflow the thread stack.
    let mut stack: Vec<(usize, usize)> = Vec::new();

    for root in outputs {
        let Input::Node(root) = *root else { continue };
        if mark[root.0] != Mark::New {
            continue;
        }
        mark[root.0] = Mark::Open;
        stack.push((root.0, 0));

        while let Some(top) = stack.last_mut() {
            let node = top.0;
            let inputs = &entries[node].inputs;
            if top.1 == inputs.len() {
                mark[node] = Mark::Done;
                order.push(node);
                stack.pop();
                continue;
            }
            let input = inputs[top.1];
            top.1 += 1;
            let Input::Node(dep) = input else { continue };
            match mark[dep.0] {
                Mark::Done => {}
                Mark::New => {
                    mark[dep.0] = Mark::Open;
                    stack.push((dep.0, 0));
                }
                Mark::Open => {
                    let from = stack.iter().position(|&(n, _)| n == dep.0).unwrap();
                    let names = stack[from..]
                        .iter()
                        .map(|&(n, _)| entries[n].node.name())
                        .collect();
                    return Err(BuildError::Cycle(names));
                }
            }
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::Sine;

    fn config(channels: usize) -> Config {
        Config {
            sample_rate: 48_000,
            max_frames: 64,
            out_channels: channels,
        }
    }

    #[test]
    fn rejects_bad_config() {
        let bad = Config {
            max_frames: 0,
            ..config(1)
        };
        assert!(matches!(
            Engine::new(Graph::new(), bad),
            Err(BuildError::InvalidConfig(_))
        ));
    }

    #[test]
    fn rejects_wrong_arity() {
        let mut g = Graph::new();
        let s = g.node(Sine::new(), []);
        g.out(s);
        assert_eq!(
            Engine::new(g, config(1)).err(),
            Some(BuildError::Arity {
                node: "sine",
                expected: 1,
                got: 0
            })
        );
    }

    #[test]
    fn rejects_channel_mismatch() {
        let mut g = Graph::new();
        let s = g.sine(440.0);
        g.out_channels([s.into(), s.into(), s.into()]);
        assert_eq!(
            Engine::new(g, config(2)).err(),
            Some(BuildError::ChannelMismatch {
                expected: 2,
                got: 3
            })
        );
    }

    #[test]
    fn rejects_cycles() {
        let mut g = Graph::new();
        let osc = g.sine(440.0);
        let amp = g.gain(osc, 0.5);
        g.set_input(osc, 0, amp);
        g.out(amp);
        let err = Engine::new(g, config(1)).err().unwrap();
        assert_eq!(err, BuildError::Cycle(vec!["gain", "sine"]));
        assert_eq!(
            err.to_string(),
            "cycle without a delay: gain -> sine -> gain"
        );
    }

    #[test]
    fn self_loop_is_a_cycle() {
        let mut g = Graph::new();
        let a = g.gain(0.0, 1.0);
        g.set_input(a, 0, a);
        g.out(a);
        assert_eq!(
            Engine::new(g, config(1)).err(),
            Some(BuildError::Cycle(vec!["gain"]))
        );
    }

    #[test]
    fn removes_dead_nodes() {
        let mut g = Graph::new();
        let used = g.sine(440.0);
        let unused = g.sine(220.0);
        g.gain(unused, 0.5);
        g.out(used);
        assert_eq!(g.len(), 3);
        let engine = Engine::new(g, config(1)).unwrap();
        assert_eq!(engine.node_count(), 1);
    }

    #[test]
    fn shared_node_is_scheduled_once() {
        // A bound stream used twice is one node, not two copies.
        let mut g = Graph::new();
        let s = g.sine(440.0);
        let a = g.gain(s, 0.5);
        let b = g.gain(s, 0.25);
        let mix = g.add(a, b);
        g.out(mix);
        let mut engine = Engine::new(g, config(1)).unwrap();
        assert_eq!(engine.node_count(), 4);

        let mut out = [0.0f32; 16];
        engine.render_planar(&mut [&mut out]);
        let mut reference = Engine::new(crate::patches::sine(440.0, 0.75), config(1)).unwrap();
        let mut expected = [0.0f32; 16];
        reference.render_planar(&mut [&mut expected]);
        for (x, y) in out.iter().zip(expected) {
            assert!((x - y).abs() < 1e-6);
        }
    }

    #[test]
    fn unrouted_output_is_silent_and_constants_pass_through() {
        let mut engine = Engine::new(Graph::new(), config(2)).unwrap();
        let mut out = [1.0f32; 10];
        engine.render_interleaved(&mut out);
        assert_eq!(out, [0.0; 10]);

        let mut g = Graph::new();
        g.out_channels([0.25.into(), (-0.5).into()]);
        let mut engine = Engine::new(g, config(2)).unwrap();
        // Odd length: the half frame at the end is zeroed.
        let mut out = [1.0f32; 5];
        engine.render_interleaved(&mut out);
        assert_eq!(out, [0.25, -0.5, 0.25, -0.5, 0.0]);
    }

    #[test]
    fn routes_channels_independently() {
        let mut g = Graph::new();
        let s = g.sine(1000.0);
        let quiet = g.gain(s, 0.5);
        g.out_channels([s.into(), quiet.into()]);
        let mut engine = Engine::new(g, config(2)).unwrap();
        let mut out = [0.0f32; 200];
        engine.render_interleaved(&mut out);
        for frame in out.as_chunks::<2>().0 {
            assert_eq!(frame[1], frame[0] * 0.5);
        }
        assert!(out.iter().any(|&x| x > 0.4));
    }

    #[test]
    fn reset_replays_identically() {
        let mut engine = Engine::new(crate::patches::vibrato(440.0, 0.3), config(1)).unwrap();
        let mut first = vec![0.0f32; 500];
        engine.render_planar(&mut [&mut first]);
        assert_eq!(engine.position(), 500);
        engine.reset();
        assert_eq!(engine.position(), 0);
        let mut second = vec![0.0f32; 500];
        engine.render_planar(&mut [&mut second]);
        assert_eq!(first, second);
    }

    #[test]
    fn reports_memory() {
        let engine = Engine::new(crate::patches::sine(440.0, 0.3), config(2)).unwrap();
        // Two nodes, 64 frames of f32 each, plus bookkeeping.
        assert!(engine.memory_bytes() >= 2 * 64 * 4);
    }
}
