//! The build stage: turn a checked program into a [`Graph`].
//!
//! A program runs by instantiating its entry rill (`main` unless told
//! otherwise) with every parameter at its default. The entry and everything
//! it calls compile to one [`Program`](super::vm::Program) node whose
//! outputs feed the device: a scalar result plays on every channel, a frame
//! result goes one value per channel.

use super::ast::Program;
use super::check::{Checked, check_entry};
use super::compile::{ArgSpec, CVal, Defs, compile_instance, default_value};
use super::diag::Diagnostic;
use super::types::Type;
use super::vm::{Operand, Program as ProgramNode};
use crate::engine::Config;
use crate::graph::{Graph, Input};

/// The entry rill used when none is named.
pub const DEFAULT_ENTRY: &str = "main";

/// Build the graph that runs `entry` on an engine with `config`.
pub fn build(
    program: &Program,
    checked: &Checked,
    config: &Config,
    entry: &str,
) -> Result<Graph, Vec<Diagnostic>> {
    check_entry(program, checked, entry)?;
    let defs = Defs::new(program, checked);
    let (def, sig) = defs.get(entry).expect("checked by check_entry");

    let mut args = Vec::with_capacity(sig.params.len());
    let mut defaults = Vec::with_capacity(sig.params.len());
    for i in 0..sig.params.len() {
        let value = default_value(&defs, &checked.types, config.sample_rate as f32, def, i)
            .map_err(|d| vec![d])?;
        let CVal::Scalar(Operand::Const(value)) = value else {
            return Err(vec![Diagnostic::error(
                def.params[i].name.span,
                format!(
                    "`{}` must have a constant scalar default",
                    sig.params[i].name
                ),
            )]);
        };
        defaults.push(value);
        args.push(ArgSpec::Stream(None));
    }
    let code = compile_instance(
        &defs,
        &checked.types,
        config.sample_rate as f32,
        def,
        sig,
        &args,
    )
    .map_err(|d| vec![d])?;

    let channels = code.output.len();
    let n = config.out_channels;
    if matches!(sig.ret, Type::Frame(..)) && channels != 1 && channels != n {
        return Err(vec![
            Diagnostic::error(
                def.ret.span(),
                format!("`{entry}` returns {channels} channels, but the output has {n}"),
            )
            .with_help("return one channel to play it everywhere, or one per output channel"),
        ]);
    }

    let mut graph = Graph::new();
    // Every declaration, so ids match the ones in the compiled handlers.
    for decl in &checked.events {
        graph.event(decl.clone().expect("checked: every kind is known"));
    }
    let outputs: Vec<Input> = match code.output.iter().all(|o| matches!(o, Operand::Const(_))) {
        // Nothing varies over time, so no node is needed.
        true => code
            .output
            .iter()
            .map(|o| match o {
                Operand::Const(c) => Input::Const(*c),
                Operand::Reg(_) => unreachable!(),
            })
            .collect(),
        false => {
            let params = sig
                .params
                .iter()
                .zip(defaults)
                .map(|(p, value)| graph.param(p.name.clone(), value).into())
                .collect::<Vec<Input>>();
            let id = graph.node(ProgramNode::new(code), params);
            (0..channels).map(|c| id.channel(c)).collect()
        }
    };
    if outputs.len() == 1 {
        graph.out(outputs[0]);
    } else {
        graph.out_channels(outputs);
    }
    Ok(graph)
}
