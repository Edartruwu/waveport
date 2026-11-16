//! Speech synthesis.
//!
//! A stream is the text of one response going in and its audio coming
//! out, with marks that say which text the audio has reached. The marks
//! are what lets an interrupted response be cut at the words the listener
//! heard.

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};

use waveport_domain::time::MediaDur;

use crate::audio::{PcmBytes, Rate, RateSet};
use crate::error::{BoxFuture, Full, PortError};

/// A speech synthesiser.
pub trait Tts: Send + Sync + 'static {
    /// What it can do. The answer does not change.
    fn caps(&self) -> TtsCaps;

    /// Opens a stream for the speech of one response.
    fn open(&self, config: TtsConfig) -> BoxFuture<'_, Result<TtsStream, PortError>>;
}

/// The two halves of a stream of speech synthesis.
pub struct TtsStream {
    /// Text goes in here.
    pub tx: Box<dyn TtsTx>,
    /// Audio comes out here. Dropping it stops the synthesis.
    pub rx: Pin<Box<dyn TtsRx>>,
}

impl fmt::Debug for TtsStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TtsStream").finish_non_exhaustive()
    }
}

/// The half of a stream that takes text.
pub trait TtsTx: Send {
    /// Takes the next piece of the response's text, to be spoken after
    /// what was given before.
    ///
    /// # Errors
    ///
    /// [`Full`]: the adapter is behind. The text was not taken.
    fn try_speak(&mut self, text: &str) -> Result<(), Full>;

    /// Says that no more text follows. The audio of what was given still
    /// arrives.
    ///
    /// # Errors
    ///
    /// [`Full`]: the adapter is behind. The request was not taken.
    fn try_finish(&mut self) -> Result<(), Full>;
}

/// The half of a stream that gives audio. Polling it is what runs the
/// adapter.
pub trait TtsRx: Send {
    /// The next event. [`TtsEvent::Done`] is the last one of a stream that
    /// came to its end, and `None` follows it. After an error nothing more
    /// follows.
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<TtsEvent, PortError>>>;
}

/// How a stream of speech synthesis is set up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TtsConfig {
    /// The rate of the audio wanted: one of the adapter's
    /// [`rates`](TtsCaps::rates).
    pub rate: Rate,
    /// The language of the text, as a BCP 47 tag, when it is known.
    pub language: Option<String>,
}

impl TtsConfig {
    /// A stream that gives audio at `rate`.
    #[must_use]
    pub const fn new(rate: Rate) -> Self {
        Self {
            rate,
            language: None,
        }
    }
}

/// Something a speech synthesiser gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TtsEvent {
    /// The next piece of audio, at the rate asked for.
    Audio(PcmBytes),
    /// The audio has reached a place in the text.
    Mark(TtsMark),
    /// All the audio of the text given has been delivered.
    Done,
}

/// A place in the text and the place in the audio where it ends.
///
/// Both are counted from the start of the stream. Marks arrive in order,
/// and a mark may arrive before the audio it points into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TtsMark {
    /// Bytes of the text given to the stream that are spoken by then.
    pub text_end: u32,
    /// The length of the stream's audio at which that text has been
    /// spoken.
    pub audio_end: MediaDur,
}

/// What a speech synthesiser can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TtsCaps {
    /// The rates it gives audio at.
    pub rates: RateSet,
    /// How it wants its text.
    pub input: TtsInput,
    /// What it says about where the words are in the audio.
    pub alignment: Alignment,
}

/// How a speech synthesiser wants its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TtsInput {
    /// Piece by piece, as it is written. The pieces are spoken as one
    /// utterance.
    Incremental,
    /// Each piece is spoken on its own, so a piece has to be something
    /// that can be: a sentence.
    Whole,
}

/// What a speech synthesiser says about where the words are in the audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Alignment {
    /// Nothing. All that is known is that the audio of a stream is the
    /// text of the stream.
    None,
    /// A mark for every word.
    Word,
    /// A mark for every character.
    Char,
}
