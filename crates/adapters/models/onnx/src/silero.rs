//! Silero VAD: is there speech in this frame.

use std::path::Path;

use ort::session::Session;
use ort::value::{Tensor, TensorRef};
use waveport_ports::audio::Rate;
use waveport_ports::error::PortError;
use waveport_ports::model::VadModel;

use crate::{Runtime, failed};

/// Samples in a frame: 32 ms.
const FRAME: usize = 512;
/// Samples of the frame before that the model is given with each frame.
const CONTEXT: usize = 64;
/// What the model reads for one frame.
const INPUT: usize = CONTEXT + FRAME;
/// The width of one of the two layers of what the model remembers.
const WIDTH: usize = 128;
/// The most frames in one call. A stream's answer is the same, to the
/// last bit, whether it is run alone or with 63 others.
const MAX_BATCH: usize = 64;

/// What the model remembers of one stream between frames.
#[derive(Clone, Debug, PartialEq)]
pub struct SileroState {
    /// The two layers, one after the other.
    memory: [f32; 2 * WIDTH],
    /// The end of the frame before.
    context: [f32; CONTEXT],
}

impl Default for SileroState {
    fn default() -> Self {
        Self {
            memory: [0.0; 2 * WIDTH],
            context: [0.0; CONTEXT],
        }
    }
}

/// The voice-activity model.
pub struct SileroVad {
    session: Session,
    /// The names of what the model gives: how likely speech is, and what
    /// it remembers.
    likely: String,
    remembered: String,
    /// Room for what goes in and comes out, so that a call allocates as
    /// little as the library lets it.
    input: Vec<f32>,
    memory: Vec<f32>,
}

impl core::fmt::Debug for SileroVad {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SileroVad").finish_non_exhaustive()
    }
}

impl SileroVad {
    /// Loads the model from its file.
    ///
    /// # Errors
    ///
    /// The file is not there, or is not this model.
    pub fn load(runtime: Runtime, model: &Path) -> Result<Self, PortError> {
        let session = runtime.session(model)?;
        let takes = |name: &str| session.inputs().iter().any(|input| input.name() == name);
        let mut gives = session
            .outputs()
            .iter()
            .map(|output| output.name().to_owned());
        let (Some(likely), Some(remembered)) = (gives.next(), gives.next()) else {
            return Err(not_the_model(model));
        };
        if !(takes("input") && takes("state") && takes("sr")) {
            return Err(not_the_model(model));
        }
        Ok(Self {
            session,
            likely,
            remembered,
            input: Vec::with_capacity(MAX_BATCH * INPUT),
            memory: Vec::with_capacity(MAX_BATCH * 2 * WIDTH),
        })
    }
}

fn not_the_model(model: &Path) -> PortError {
    PortError::permanent(format!(
        "{} is not the voice-activity model: it does not take and give what that model does",
        model.display()
    ))
}

impl VadModel for SileroVad {
    type State = SileroState;

    fn rate(&self) -> Rate {
        Rate::Hz16000
    }

    fn frame(&self) -> usize {
        FRAME
    }

    fn max_batch(&self) -> usize {
        MAX_BATCH
    }

    fn infer(
        &mut self,
        frames: &[&[i16]],
        states: &mut [SileroState],
        out: &mut [f32],
    ) -> Result<(), PortError> {
        let batch = frames.len();
        if batch == 0 {
            return Ok(());
        }
        if batch > MAX_BATCH || states.len() != batch || out.len() != batch {
            return Err(PortError::permanent(format!(
                "the voice-activity model was given {batch} frames, {} states and room for {} \
                 answers; it takes as many of each, {MAX_BATCH} at most",
                states.len(),
                out.len()
            )));
        }
        if let Some(frame) = frames.iter().find(|frame| frame.len() != FRAME) {
            return Err(PortError::permanent(format!(
                "the voice-activity model was given a frame of {} samples; a frame is {FRAME}",
                frame.len()
            )));
        }

        // What the model reads: for each stream the end of its frame
        // before, then the frame. What it remembers goes in layer by
        // layer, each layer stream by stream.
        self.input.clear();
        for (frame, state) in frames.iter().zip(states.iter()) {
            self.input.extend_from_slice(&state.context);
            self.input
                .extend(frame.iter().map(|sample| f32::from(*sample) / 32_768.0));
        }
        self.memory.clear();
        for layer in 0..2 {
            for state in states.iter() {
                let (first, second) = state.memory.split_at(WIDTH);
                self.memory
                    .extend_from_slice(if layer == 0 { first } else { second });
            }
        }

        let what = "the voice-activity model failed";
        let input = TensorRef::from_array_view(([batch, INPUT], self.input.as_slice()))
            .map_err(|error| failed(what, &error))?;
        let memory = TensorRef::from_array_view(([2, batch, WIDTH], self.memory.as_slice()))
            .map_err(|error| failed(what, &error))?;
        let rate = Tensor::from_array(((), vec![i64::from(Rate::Hz16000.hz())]))
            .map_err(|error| failed(what, &error))?;
        let outputs = self
            .session
            .run(ort::inputs! { "input" => input, "state" => memory, "sr" => rate })
            .map_err(|error| failed(what, &error))?;
        let (_, likely) = outputs
            .get(&self.likely)
            .ok_or_else(|| failed(what, &"it gave no answer"))?
            .try_extract_tensor::<f32>()
            .map_err(|error| failed(what, &error))?;
        let (_, remembered) = outputs
            .get(&self.remembered)
            .ok_or_else(|| failed(what, &"it gave nothing to remember"))?
            .try_extract_tensor::<f32>()
            .map_err(|error| failed(what, &error))?;
        if likely.len() != batch || remembered.len() != batch * 2 * WIDTH {
            return Err(failed(what, &"it gave an answer of another size"));
        }

        // Nothing was changed until here, so a failure leaves every
        // state as it was.
        out.copy_from_slice(likely);
        let (first, second) = remembered.split_at(batch * WIDTH);
        let layers = first
            .as_chunks::<WIDTH>()
            .0
            .iter()
            .zip(second.as_chunks::<WIDTH>().0);
        for ((state, frame), (first, second)) in states.iter_mut().zip(frames).zip(layers) {
            let (into_first, into_second) = state.memory.split_at_mut(WIDTH);
            into_first.copy_from_slice(first);
            into_second.copy_from_slice(second);
            let end = frame.get(FRAME - CONTEXT..).unwrap_or_default();
            for (slot, sample) in state.context.iter_mut().zip(end) {
                *slot = f32::from(*sample) / 32_768.0;
            }
        }
        Ok(())
    }
}
