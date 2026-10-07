//! Rill: a language for real-time audio.
//!
//! This is the M1 runtime: graphs are built by hand in Rust, frozen into an
//! [`Engine`], and rendered either to the audio device ([`device`], behind the
//! `device` feature) or offline through a simulated callback loop
//! ([`offline`]).

pub mod denormal;
#[cfg(feature = "device")]
pub mod device;
pub mod engine;
pub mod event;
pub mod format;
pub mod graph;
pub mod lang;
pub mod node;
pub mod nodes;
pub mod offline;
pub mod ops;
pub mod patches;
pub mod wav;

pub use engine::{BuildError, Config, Engine, ParamEvent, RillEvent};
pub use event::{Dispatch, Event, EventDecl, EventId, EventKind, Payload};
pub use graph::{Graph, Input, NodeId};
pub use node::{Context, Inputs, Node, Signal};
