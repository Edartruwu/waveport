//! Transports: how audio reaches a session and leaves it.
//!
//! A transport carries packets and knows nothing about what is in them.
//! Decoding, reordering and pacing are done by the media path; a
//! transport hands over what arrived, in the order it arrived, and sends
//! what it is given when it is given it.
//!
//! A transport need not carry audio. One that carries text alone says so
//! in what it can do ([`TransportCaps::audio`]), and a session that
//! hears and speaks is not started on it.

use core::task::{Context, Poll};

use bytes::Bytes;
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_domain::time::MediaDur;

use crate::audio::Codec;
use crate::error::Full;

/// One session's connection to its participants.
pub trait Transport: Send + 'static {
    /// What it can do. The answer does not change.
    fn caps(&self) -> TransportCaps;

    /// The next thing that happened. `None` means the transport is closed
    /// and nothing more will.
    ///
    /// The transport is not pinned: the same owner polls it and sends
    /// through it, and holds it in a box. A transport that needs a place
    /// that does not move keeps that part of itself in a box of its own.
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>>;

    /// Sends a frame of a response's audio.
    ///
    /// # Errors
    ///
    /// [`Full`]: the transport is behind. The frame was not taken, and the
    /// caller does not keep it for later: audio that cannot be sent in
    /// time is dropped.
    fn try_send(&mut self, frame: FrameOut<'_>) -> Result<(), Full>;

    /// Asks the transport for something other than sending audio.
    ///
    /// # Errors
    ///
    /// [`Full`]: the transport is behind. The request was not taken.
    fn try_control(&mut self, control: TransportControl) -> Result<(), Full>;

    /// Tells the far end that the session is over, in whatever way the
    /// transport has for that: a goodbye, a call that is hung up. `Ready`
    /// means that it is said, or that nobody is left to say it to.
    ///
    /// It is asked for when a session has closed, and until it is ready
    /// or the session will not wait any longer. Nothing is sent after
    /// it. A session that has lost its journal does not ask: whoever has
    /// the session now may still be talking to the far end.
    ///
    /// A transport with nothing to say need not have this at all.
    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let _ = cx;
        Poll::Ready(())
    }
}

/// Something that happened on a transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportEvent {
    /// A participant is there.
    Joined {
        /// Who.
        who: ParticipantId,
    },
    /// A participant is gone for good.
    Left {
        /// Who.
        who: ParticipantId,
    },
    /// A packet of a participant's audio arrived.
    Audio {
        /// Whose.
        who: ParticipantId,
        /// Its number in the stream, which wraps round.
        sequence: u16,
        /// Where its audio begins, in units of the codec's clock, which
        /// wraps round.
        timestamp: u32,
        /// The packet.
        payload: Bytes,
    },
    /// The far end says how much of a response it has played.
    Played {
        /// The response.
        tag: OpTag,
        /// How much of its audio has been played.
        played: MediaDur,
    },
    /// A participant sent text instead of speaking.
    Text {
        /// Who.
        who: ParticipantId,
        /// What they wrote.
        text: String,
    },
    /// The connection dropped. It may come back.
    Detached,
    /// The connection is back.
    Attached,
}

/// A frame of a response's audio, encoded for the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameOut<'a> {
    /// The response it belongs to.
    pub tag: OpTag,
    /// Where in the response's audio it begins.
    pub at: MediaDur,
    /// How long it lasts.
    pub length: MediaDur,
    /// The packet.
    pub payload: &'a [u8],
    /// Whether it is the last frame of the response.
    pub last: bool,
}

/// A request to a transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransportControl {
    /// Discard the audio of this response that the far end holds and has
    /// not played. It follows the last frame of the response: none of its
    /// frames is sent after it. A transport that cannot
    /// ([`RemoteBuffer::clearable`]) takes the request and does nothing.
    Clear {
        /// The response.
        tag: OpTag,
    },
}

/// What a transport can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportCaps {
    /// How it carries audio. `None` for one that carries none.
    pub audio: Option<AudioCaps>,
    /// Whether participants can send text.
    pub text: bool,
}

/// How a transport carries audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioCaps {
    /// How the audio that arrives is written.
    pub inbound: Codec,
    /// How the audio to send has to be written.
    pub outbound: Codec,
    /// What can be known and done about audio the far end holds.
    pub remote_buffer: RemoteBuffer,
}

/// What can be known and done about audio that was sent and that the far
/// end has not played yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RemoteBuffer {
    /// Whether it can be discarded
    /// ([`TransportControl::Clear`]).
    pub clearable: bool,
    /// Whether the far end says what it has played
    /// ([`TransportEvent::Played`]).
    pub reports_played: bool,
}
