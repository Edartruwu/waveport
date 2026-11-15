//! `tract`, the pure-Rust inference engine.
//!
//! A `tract` plan is optimised for fixed input shapes, so the voice-activity
//! model keeps one plan per batch size it has been asked for.

use std::path::{Path, PathBuf};

use tract_onnx::prelude::*;

use super::{Turn, VAD_INPUT, VAD_STATE_WIDTH, Vad};
use crate::Result;

type Plan = Arc<TypedRunnableModel>;

fn boxed(error: TractError) -> Box<dyn std::error::Error + Send + Sync> {
    format!("{error:#}").into()
}

/// The ONNX loader. The shapes a model file records for its intermediate
/// values use the exporter's symbolic batch size, which contradicts the
/// fixed batch size a plan is built for; the loader is told to derive them
/// instead.
fn loader() -> tract_onnx::Onnx {
    tract_onnx::onnx()
        .with_ignore_output_shapes(true)
        .with_ignore_value_info(true)
}

/// The position of each named input, in the order the model declares them.
fn input_names(model: &InferenceModel) -> Result<Vec<String>> {
    Ok(model
        .input_outlets()
        .map_err(boxed)?
        .iter()
        .map(|outlet| model.node(outlet.node).name.clone())
        .collect())
}

/// Builds a plan for one batch size. Returns the plan and the order in
/// which it takes its inputs.
fn vad_plan(path: &Path, batch: usize) -> Result<(Plan, Vec<String>)> {
    let mut model = loader().model_for_path(path).map_err(boxed)?;
    let names = input_names(&model)?;
    for (index, name) in names.iter().enumerate() {
        let fact = match name.as_str() {
            "input" => InferenceFact::dt_shape(f32::datum_type(), tvec!(batch, VAD_INPUT)),
            "state" => InferenceFact::dt_shape(f32::datum_type(), tvec!(2, batch, VAD_STATE_WIDTH)),
            // The sample rate selects a branch of the graph. Making it a
            // constant lets the optimiser remove the branch not taken.
            "sr" => InferenceFact::from(tensor0(16_000_i64)),
            other => return Err(format!("unexpected model input `{other}`").into()),
        };
        model.set_input_fact(index, fact).map_err(boxed)?;
    }
    let plan = model
        .into_optimized()
        .map_err(boxed)?
        .into_runnable()
        .map_err(boxed)?;
    Ok((plan, names))
}

pub struct TractVad {
    path: PathBuf,
    plans: Vec<Option<Plan>>,
    names: Vec<String>,
}

impl TractVad {
    pub fn load(path: &Path) -> Result<Self> {
        let (plan, names) = vad_plan(path, 1)?;
        Ok(Self {
            path: path.to_owned(),
            plans: vec![None, Some(plan)],
            names,
        })
    }

    /// Builds the plan for `batch` ahead of time, so that the cost does not
    /// land inside a measurement.
    pub fn prepare(&mut self, batch: usize) -> Result<()> {
        if self.plans.len() <= batch {
            self.plans.resize(batch + 1, None);
        }
        if self.plans[batch].is_none() {
            self.plans[batch] = Some(vad_plan(&self.path, batch)?.0);
        }
        Ok(())
    }
}

impl Vad for TractVad {
    fn run(
        &mut self,
        batch: usize,
        input: &[f32],
        state: &mut [f32],
        probs: &mut [f32],
    ) -> Result<()> {
        self.prepare(batch)?;
        let plan = self.plans[batch].as_ref().ok_or("no plan")?;
        let mut inputs: TVec<TValue> = tvec!();
        for name in &self.names {
            let tensor = match name.as_str() {
                "input" => Tensor::from_shape(&[batch, VAD_INPUT], input).map_err(boxed)?,
                "state" => {
                    Tensor::from_shape(&[2, batch, VAD_STATE_WIDTH], state).map_err(boxed)?
                }
                _ => tensor0(16_000_i64),
            };
            inputs.push(tensor.into());
        }
        let outputs = plan.run(inputs).map_err(boxed)?;
        let values = outputs[0].try_as_plain_ram().map_err(boxed)?;
        probs[..batch].copy_from_slice(&values.as_slice::<f32>().map_err(boxed)?[..batch]);
        let after = outputs[1].try_as_plain_ram().map_err(boxed)?;
        state.copy_from_slice(after.as_slice::<f32>().map_err(boxed)?);
        Ok(())
    }
}

pub struct TractTurn {
    plan: Plan,
}

impl TractTurn {
    /// Loads the model. `TRACT_TURN_LOAD=symbols` keeps the shapes the file
    /// records and substitutes the batch symbol instead of ignoring them;
    /// the probe uses it to check that the result does not depend on how
    /// the model was loaded.
    pub fn load(path: &Path) -> Result<Self> {
        let model = if std::env::var("TRACT_TURN_LOAD").as_deref() == Ok("symbols") {
            let mut model = tract_onnx::onnx().model_for_path(path).map_err(boxed)?;
            let batch = model.symbols.sym("s6");
            let mut values = std::collections::HashMap::new();
            values.insert(batch, 1.to_dim());
            model.set_symbols(&values).map_err(boxed)?;
            model
        } else {
            loader()
                .model_for_path(path)
                .map_err(boxed)?
                .with_input_fact(
                    0,
                    InferenceFact::dt_shape(f32::datum_type(), tvec!(1, 80, 800)),
                )
                .map_err(boxed)?
        };
        let plan = model
            .into_optimized()
            .map_err(boxed)?
            .into_runnable()
            .map_err(boxed)?;
        Ok(Self { plan })
    }
}

impl Turn for TractTurn {
    fn run(&mut self, features: &[f32]) -> Result<f32> {
        let input = Tensor::from_shape(&[1, 80, 800], features).map_err(boxed)?;
        let outputs = self.plan.run(tvec!(input.into())).map_err(boxed)?;
        let values = outputs[0].try_as_plain_ram().map_err(boxed)?;
        values
            .as_slice::<f32>()
            .map_err(boxed)?
            .first()
            .copied()
            .ok_or_else(|| "the end-of-turn model returned nothing".into())
    }
}
