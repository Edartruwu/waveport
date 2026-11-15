//! Speech recognition.
//!
//! A stream is one participant's audio going in and that participant's
//! words coming out. The words carry the stretch of the media clock they
//! were heard in, because that, and not the time they arrive, is what
//! joins them to a turn.

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};

use waveport_domain::time::MediaSpan;

use crate::audio::{AudioRef, Rate, RateSet};
use crate::error::{BoxFuture, Full, PortError};

/// A speech recogniser.
pub trait Stt: Send + Sync + 'static {
    /// What it can do. The answer does not change.
    fn caps(&self) -> SttCaps;

    /// Opens a stream for one participant's audio.
    fn open(&self, config: SttConfig) -> BoxFuture<'_, Result<SttStream, PortError>>;
}

/// The two halves of a stream of speech recognition.
pub struct SttStream {
    /// Audio goes in here.
    pub tx: Box<dyn SttTx>,
    /// Words come out here. Dropping it ends the stream.
    pub rx: Pin<Box<dyn SttRx>>,
}

impl fmt::Debug for SttStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SttStream").finish_non_exhaustive()
    }
}

/// The half of a stream that takes audio.
pub trait SttTx: Send {
    /// Takes the next piece of audio. Pieces follow one another; where the
    /// stream of audio was broken off, the next piece is later on the
    /// media clock than the last one ended.
    ///
    /// # Errors
    ///
    /// [`Full`]: the adapter is behind. The piece was not taken.
    fn try_send(&mut self, audio: AudioRef<'_>) -> Result<(), Full>;

    /// Asks for the final transcript of everything sent so far, without
    /// waiting for more audio. An adapter whose provider decides that for
    /// itself ([`SttFinalize::ByProvider`]) takes the request and does
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`Full`]: the adapter is behind. The request was not taken.
    fn try_finalize(&mut self) -> Result<(), Full>;
}

/// The half of a stream that gives words. Polling it is what runs the
/// adapter.
pub trait SttRx: Send {
    /// The next event. `None` means the stream is over; after an error
    /// nothing more follows.
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<SttEvent, PortError>>>;
}

/// How a stream of speech recognition is set up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SttConfig {
    /// The rate of the audio that will be sent: one of the adapter's
    /// [`rates`](SttCaps::rates).
    pub rate: Rate,
    /// The language spoken, as a BCP 47 tag, when it is known.
    pub language: Option<String>,
}

impl SttConfig {
    /// A stream for audio at `rate` in a language the recogniser is left
    /// to find out.
    #[must_use]
    pub const fn new(rate: Rate) -> Self {
        Self {
            rate,
            language: None,
        }
    }
}

/// Something a speech recogniser says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SttEvent {
    /// Words.
    Transcript(Transcript),
}

/// Words recognised in a stretch of audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    /// The audio the words were heard in, on the media clock of the audio
    /// that was sent.
    pub span: MediaSpan,
    /// The words. For an interim transcript, everything heard in `span`
    /// so far. A final transcript may be empty: the recogniser heard
    /// nothing it could make words of.
    pub text: String,
    /// Whether the recogniser will not revise these words.
    pub is_final: bool,
}

/// What a speech recogniser can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SttCaps {
    /// The rates it takes audio at.
    pub rates: RateSet,
    /// Whether it gives words before they are final.
    pub interim: bool,
    /// Where the span of a transcript comes from.
    pub timing: SttTiming,
    /// What makes it give a final transcript.
    pub finalize: SttFinalize,
}

/// Where the span of a transcript comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SttTiming {
    /// The provider says when the words were spoken.
    Words,
    /// The provider gives no times. The span is the audio sent since the
    /// transcript before, which is wider than the words.
    Window,
}

/// What makes a speech recogniser give a final transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SttFinalize {
    /// Being asked, and nothing else.
    OnRequest,
    /// Its own judgement that the speaker has paused. Asking does nothing.
    ByProvider,
    /// Its own judgement, and being asked.
    Both,
}

impl SttFinalize {
    /// Whether a request is honoured.
    #[must_use]
    pub const fn on_request(self) -> bool {
        matches!(self, Self::OnRequest | Self::Both)
    }
}
