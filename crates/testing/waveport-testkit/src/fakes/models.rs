//! Listening models that need no weights.
//!
//! A scripted microphone says something by sending a tone and says
//! nothing by sending silence. These models go by that: a frame is speech
//! when it is loud, and a turn is over when its audio ends in quiet. They
//! are exact where real models are likely, which is what a test wants.

use std::sync::{Arc, Mutex, PoisonError};

use waveport_domain::event::OverlapVerdict;
use waveport_ports::audio::Rate;
use waveport_ports::error::PortError;
use waveport_ports::model::{InterruptionClassifier, TurnModel, VadModel};

/// A sample this far from zero is loud. The tone of a scripted
/// microphone never comes closer to zero than 2,000.
const LOUD: u16 = 1_000;

/// Says that a frame holds speech when any of it is loud.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoudVad {
    /// The samples in a frame.
    pub frame: usize,
    /// The most frames taken in one call.
    pub batch: usize,
}

impl Default for LoudVad {
    /// Frames of 512 samples, 64 of them at once: what Silero takes.
    fn default() -> Self {
        Self {
            frame: 512,
            batch: 64,
        }
    }
}

impl VadModel for LoudVad {
    type State = ();

    fn rate(&self) -> Rate {
        Rate::Hz16000
    }

    fn frame(&self) -> usize {
        self.frame
    }

    fn max_batch(&self) -> usize {
        self.batch
    }

    fn infer(&mut self, frames: &[&[i16]], _: &mut [()], out: &mut [f32]) -> Result<(), PortError> {
        for (frame, p) in frames.iter().zip(out) {
            *p = if is_loud(frame) { 0.95 } else { 0.02 };
        }
        Ok(())
    }
}

fn is_loud(audio: &[i16]) -> bool {
    audio.iter().any(|sample| sample.unsigned_abs() > LOUD)
}

/// What a scripted model of turns was asked: how much audio each turn it
/// judged had, in milliseconds.
#[derive(Clone, Debug, Default)]
pub struct TurnLog {
    asked: Arc<Mutex<Vec<u32>>>,
}

impl TurnLog {
    /// The length of each turn that was judged, in the order they were.
    #[must_use]
    pub fn asked(&self) -> Vec<u32> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// What a test makes of the audio of a turn.
type Rule = Box<dyn FnMut(&[i16]) -> f32 + Send>;

/// Says how likely it is that a speaker has finished, by a rule the test
/// gives it.
pub struct ScriptedTurns {
    rule: Rule,
    log: TurnLog,
}

impl ScriptedTurns {
    /// A model that takes a speaker to have finished whenever it is
    /// asked: 0.95 for every turn.
    #[must_use]
    pub fn always() -> Self {
        Self::with(|_| 0.95)
    }

    /// A model that answers with what `rule` makes of the audio of a
    /// turn.
    #[must_use]
    pub fn with(rule: impl FnMut(&[i16]) -> f32 + Send + 'static) -> Self {
        Self {
            rule: Box::new(rule),
            log: TurnLog::default(),
        }
    }

    /// A look at what the model is asked, for the test that gives the
    /// model away.
    #[must_use]
    pub fn log(&self) -> TurnLog {
        self.log.clone()
    }
}

impl core::fmt::Debug for ScriptedTurns {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ScriptedTurns").finish_non_exhaustive()
    }
}

impl TurnModel for ScriptedTurns {
    fn rate(&self) -> Rate {
        Rate::Hz16000
    }

    fn window(&self) -> usize {
        // Eight seconds, as Smart Turn looks at.
        128_000
    }

    fn max_batch(&self) -> usize {
        8
    }

    fn infer(&mut self, turns: &[&[i16]], out: &mut [f32]) -> Result<(), PortError> {
        let mut asked = self
            .log
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (turn, p) in turns.iter().zip(out) {
            asked.push(u32::try_from(turn.len() / 16).unwrap_or(u32::MAX));
            *p = (self.rule)(turn);
        }
        Ok(())
    }
}

/// Says what speech over the agent's own is, by how long it is: speech
/// of at least `barge_in_ms` wants the floor, and anything shorter is a
/// listener's noise of agreement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByLength {
    /// The length from which speech is taken to want the floor.
    pub barge_in_ms: u32,
}

impl InterruptionClassifier for ByLength {
    fn rate(&self) -> Rate {
        Rate::Hz16000
    }

    fn window(&self) -> usize {
        32_000
    }

    fn infer(&mut self, speech: &[&[i16]], out: &mut [OverlapVerdict]) -> Result<(), PortError> {
        for (speech, verdict) in speech.iter().zip(out) {
            let loud = speech
                .chunks(16)
                .filter(|millisecond| is_loud(millisecond))
                .count();
            *verdict = if loud >= self.barge_in_ms as usize {
                OverlapVerdict::BargeIn
            } else if loud > 0 {
                OverlapVerdict::Backchannel
            } else {
                OverlapVerdict::Noise
            };
        }
        Ok(())
    }
}
