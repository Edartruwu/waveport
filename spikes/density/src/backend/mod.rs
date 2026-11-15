//! The two inference backends behind one interface.

use std::path::Path;

use crate::Result;

#[cfg(feature = "backend-ort")]
pub mod ort;
#[cfg(feature = "backend-tract")]
pub mod tract;

/// Samples of context that precede each voice-activity frame.
pub const VAD_CONTEXT: usize = 64;
/// New samples in each voice-activity frame, at 16 kHz.
pub const VAD_FRAME: usize = 512;
/// What the model reads for one frame: context, then the frame.
pub const VAD_INPUT: usize = VAD_CONTEXT + VAD_FRAME;
/// Width of one layer of recurrent state for one stream.
pub const VAD_STATE_WIDTH: usize = 128;
/// Recurrent state for one stream: two layers.
pub const VAD_STATE: usize = 2 * VAD_STATE_WIDTH;

/// Which library runs a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ort,
    Tract,
}

impl Kind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ort => "ort",
            Self::Tract => "tract",
        }
    }
}

impl std::str::FromStr for Kind {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, String> {
        match text {
            "ort" => Ok(Self::Ort),
            "tract" => Ok(Self::Tract),
            other => Err(format!("unknown backend `{other}`")),
        }
    }
}

/// The voice-activity model, run on a batch of independent streams.
pub trait Vad: Send {
    /// Runs one frame for each of `batch` streams.
    ///
    /// `input` holds `batch * VAD_INPUT` samples. `state` holds the
    /// recurrent state as the model lays it out, `[2, batch, 128]`, and is
    /// replaced by the new state. `probs` receives one probability per
    /// stream.
    fn run(
        &mut self,
        batch: usize,
        input: &[f32],
        state: &mut [f32],
        probs: &mut [f32],
    ) -> Result<()>;
}

/// The end-of-turn model.
pub trait Turn: Send {
    /// The probability that the turn is complete, from `80 * 800` features.
    fn run(&mut self, features: &[f32]) -> Result<f32>;
}

/// Loads the voice-activity model with the chosen backend.
pub fn load_vad(kind: Kind, path: &Path) -> Result<Box<dyn Vad>> {
    match kind {
        #[cfg(feature = "backend-ort")]
        Kind::Ort => Ok(Box::new(ort::OrtVad::load(path)?)),
        #[cfg(feature = "backend-tract")]
        Kind::Tract => Ok(Box::new(tract::TractVad::load(path)?)),
        #[allow(unreachable_patterns)]
        other => Err(format!("this binary was built without the {} backend", other.name()).into()),
    }
}

/// Loads the end-of-turn model with the chosen backend.
pub fn load_turn(kind: Kind, path: &Path) -> Result<Box<dyn Turn>> {
    match kind {
        #[cfg(feature = "backend-ort")]
        Kind::Ort => Ok(Box::new(ort::OrtTurn::load(path)?)),
        #[cfg(feature = "backend-tract")]
        Kind::Tract => Ok(Box::new(tract::TractTurn::load(path)?)),
        #[allow(unreachable_patterns)]
        other => Err(format!("this binary was built without the {} backend", other.name()).into()),
    }
}
