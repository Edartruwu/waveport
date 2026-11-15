//! ONNX Runtime through the `ort` crate, restricted to the calling thread.

use std::path::Path;
use std::sync::OnceLock;

use ort::environment::Environment;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{Tensor, TensorRef};

use super::{Turn, VAD_INPUT, VAD_STATE_WIDTH, Vad};
use crate::Result;

fn environment() -> Result<&'static Environment> {
    static ENVIRONMENT: OnceLock<Environment> = OnceLock::new();
    if let Some(environment) = ENVIRONMENT.get() {
        return Ok(environment);
    }
    let built = ort::init().with_name("density-spike").build()?;
    Ok(ENVIRONMENT.get_or_init(|| built))
}

/// One thread, no spinning: inference runs on the caller's pinned core and
/// nowhere else.
fn session(path: &Path) -> Result<Session> {
    // A failed builder step returns the builder inside the error, which is
    // not `Send`; only the message is kept.
    let text = |error: &dyn std::fmt::Display| error.to_string();
    let session = Session::builder(environment()?)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| text(&e))?
        .with_intra_threads(1)
        .map_err(|e| text(&e))?
        .with_inter_threads(1)
        .map_err(|e| text(&e))?
        .with_intra_op_spinning(false)
        .map_err(|e| text(&e))?
        .commit_from_file(path)?;
    Ok(session)
}

/// A line per input and output of a model, for the probe.
pub fn describe(path: &Path) -> Result<Vec<String>> {
    let session = session(path)?;
    let mut lines = Vec::new();
    for outlet in session.inputs() {
        lines.push(format!("input  {}: {:?}", outlet.name(), outlet.dtype()));
    }
    for outlet in session.outputs() {
        lines.push(format!("output {}: {:?}", outlet.name(), outlet.dtype()));
    }
    Ok(lines)
}

pub struct OrtVad {
    session: Session,
    /// Whether the model has the sample-rate input. The export without
    /// control flow may not.
    has_rate: bool,
    /// The new state, held while the outputs still borrow the old one.
    next_state: Vec<f32>,
}

impl OrtVad {
    pub fn load(path: &Path) -> Result<Self> {
        let session = session(path)?;
        let has_rate = session.inputs().iter().any(|outlet| outlet.name() == "sr");
        Ok(Self {
            session,
            has_rate,
            next_state: Vec::new(),
        })
    }
}

impl Vad for OrtVad {
    fn run(
        &mut self,
        batch: usize,
        input: &[f32],
        state: &mut [f32],
        probs: &mut [f32],
    ) -> Result<()> {
        let frames = TensorRef::from_array_view(([batch, VAD_INPUT], input))?;
        let before = TensorRef::from_array_view(([2, batch, VAD_STATE_WIDTH], &*state))?;
        let outputs = if self.has_rate {
            let rate = Tensor::from_array(((), vec![16_000_i64]))?;
            self.session
                .run(ort::inputs! { "input" => frames, "state" => before, "sr" => rate })?
        } else {
            self.session
                .run(ort::inputs! { "input" => frames, "state" => before })?
        };
        let (_, values) = outputs[0].try_extract_tensor::<f32>()?;
        probs[..batch].copy_from_slice(&values[..batch]);
        let (_, after) = outputs[1].try_extract_tensor::<f32>()?;
        self.next_state.clear();
        self.next_state.extend_from_slice(after);
        drop(outputs);
        state.copy_from_slice(&self.next_state);
        Ok(())
    }
}

pub struct OrtTurn {
    session: Session,
}

impl OrtTurn {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            session: session(path)?,
        })
    }
}

impl Turn for OrtTurn {
    fn run(&mut self, features: &[f32]) -> Result<f32> {
        let input = TensorRef::from_array_view(([1_usize, 80, 800], features))?;
        let outputs = self
            .session
            .run(ort::inputs! { "input_features" => input })?;
        let (_, values) = outputs[0].try_extract_tensor::<f32>()?;
        values
            .first()
            .copied()
            .ok_or_else(|| "the end-of-turn model returned nothing".into())
    }
}
