//! Audio codecs: from the bytes a transport carries to samples and back.

pub mod g711;
pub mod opus;

use core::fmt;

/// Why a codec refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecError {
    /// The codec has no such settings.
    Settings,
    /// The packet is not one the codec can read.
    Packet,
    /// The audio or the buffer given has a length the codec cannot work
    /// with.
    Length,
    /// The codec failed inside.
    Internal,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Settings => "the codec has no such settings",
            Self::Packet => "the packet is not one the codec can read",
            Self::Length => "the audio or the buffer has a length the codec cannot work with",
            Self::Internal => "the codec failed inside",
        })
    }
}

impl core::error::Error for CodecError {}
