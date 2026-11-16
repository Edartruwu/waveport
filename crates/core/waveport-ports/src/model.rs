//! The models that listen: is someone speaking, have they finished, do
//! they want the floor.
//!
//! One instance serves every session. It is owned by one thread and is
//! called with a batch: one input for each session that has one ready.
//! Audio is 16-bit samples at the model's rate; turning them into what
//! the model eats is the adapter's work, since it has to copy them into
//! the model's input anyway.

pub use waveport_domain::event::OverlapVerdict;

use crate::audio::Rate;
use crate::error::PortError;

/// Says how likely it is that a frame of audio holds speech.
pub trait VadModel: Send + 'static {
    /// What the model remembers of one stream between frames.
    type State: Default + Send;

    /// The rate of the audio it takes.
    fn rate(&self) -> Rate;

    /// How many samples one frame has.
    fn frame(&self) -> usize;

    /// The most frames it takes in one call.
    fn max_batch(&self) -> usize;

    /// Judges one frame of each of several streams. `frames`, `states`
    /// and `out` are as long as each other; `out[i]` is set to how likely
    /// it is, from 0 to 1, that `frames[i]` holds speech, and `states[i]`
    /// is brought up to date.
    ///
    /// # Errors
    ///
    /// The model failed. Nothing is known about any of the frames, and
    /// the states are as they were.
    fn infer(
        &mut self,
        frames: &[&[i16]],
        states: &mut [Self::State],
        out: &mut [f32],
    ) -> Result<(), PortError>;
}

/// Says how likely it is that a speaker who paused has finished.
pub trait TurnModel: Send + 'static {
    /// The rate of the audio it takes.
    fn rate(&self) -> Rate;

    /// The most audio it looks at, in samples. It is given the end of the
    /// turn so far: the newest audio, this much at most.
    fn window(&self) -> usize;

    /// The most turns it takes in one call.
    fn max_batch(&self) -> usize;

    /// Judges several turns. `out[i]` is set to how likely it is, from 0
    /// to 1, that the speaker of `turns[i]` has finished.
    ///
    /// # Errors
    ///
    /// The model failed. Nothing is known about any of the turns.
    fn infer(&mut self, turns: &[&[i16]], out: &mut [f32]) -> Result<(), PortError>;
}

/// Says what speech over the agent's own is: a listener's noise of
/// agreement, a wish to take the floor, or nothing meant for anyone.
pub trait InterruptionClassifier: Send + 'static {
    /// The rate of the audio it takes.
    fn rate(&self) -> Rate;

    /// The most audio it looks at, in samples.
    fn window(&self) -> usize;

    /// Judges the speech of several speakers. `out[i]` is set to what
    /// `speech[i]` is.
    ///
    /// # Errors
    ///
    /// The model failed. Nothing is known about any of them.
    fn infer(&mut self, speech: &[&[i16]], out: &mut [OverlapVerdict]) -> Result<(), PortError>;
}
