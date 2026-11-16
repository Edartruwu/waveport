//! Smart Turn: has the speaker finished.

use std::path::Path;

use ort::session::Session;
use ort::value::TensorRef;
use waveport_ports::audio::Rate;
use waveport_ports::error::PortError;
use waveport_ports::model::TurnModel;

use crate::mel::{BANDS, FEATURES, Mel, SAMPLES, STEPS};
use crate::{Runtime, failed};

/// The end-of-turn model.
pub struct SmartTurn {
    session: Session,
    /// The name of what the model takes, and of what it gives.
    takes: String,
    gives: String,
    mel: Mel,
    features: Vec<f32>,
}

impl core::fmt::Debug for SmartTurn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SmartTurn").finish_non_exhaustive()
    }
}

impl SmartTurn {
    /// Loads the model from its file.
    ///
    /// # Errors
    ///
    /// The file is not there, or is not this model.
    pub fn load(runtime: Runtime, model: &Path) -> Result<Self, PortError> {
        let session = runtime.session(model)?;
        let takes = session
            .inputs()
            .first()
            .map(|input| input.name().to_owned());
        let gives = session
            .outputs()
            .first()
            .map(|output| output.name().to_owned());
        let (Some(takes), Some(gives)) = (takes, gives) else {
            return Err(PortError::permanent(format!(
                "{} is not the end-of-turn model: it takes or gives nothing",
                model.display()
            )));
        };
        Ok(Self {
            session,
            takes,
            gives,
            mel: Mel::new(),
            features: vec![0.0; FEATURES],
        })
    }
}

impl TurnModel for SmartTurn {
    fn rate(&self) -> Rate {
        Rate::Hz16000
    }

    fn window(&self) -> usize {
        SAMPLES
    }

    fn max_batch(&self) -> usize {
        // One at a time: a call is forty milliseconds of work, and the
        // turns that wait for it are few.
        1
    }

    fn infer(&mut self, turns: &[&[i16]], out: &mut [f32]) -> Result<(), PortError> {
        if turns.len() != out.len() {
            return Err(PortError::permanent(format!(
                "the end-of-turn model was given {} turns and room for {} answers",
                turns.len(),
                out.len()
            )));
        }
        let what = "the end-of-turn model failed";
        for (turn, likely) in turns.iter().zip(out.iter_mut()) {
            self.mel.features(turn, &mut self.features);
            let features =
                TensorRef::from_array_view(([1, BANDS, STEPS], self.features.as_slice()))
                    .map_err(|error| failed(what, &error))?;
            let outputs = self
                .session
                .run(ort::inputs! { self.takes.as_str() => features })
                .map_err(|error| failed(what, &error))?;
            let (_, values) = outputs
                .get(&self.gives)
                .ok_or_else(|| failed(what, &"it gave no answer"))?
                .try_extract_tensor::<f32>()
                .map_err(|error| failed(what, &error))?;
            *likely = *values
                .first()
                .ok_or_else(|| failed(what, &"it gave an empty answer"))?;
        }
        Ok(())
    }
}
